//! Small-file PACKING — PR PK2's contracts
//! (`docs/design-small-file-packing.md` §5.2–§5.5, §10; PR plan PK2).
//!
//! A staged-layout file (4 KiB < size ≤ block) promoted to the shared
//! backend used to take one whole 4 MiB block of its own — the 64× space
//! law of `.benchmarks/2026-09-09-fsync-promote-staged-ab.md` (140,183
//! promotions filled a 480 GiB set in 8 s). Under `SQUEEZEFS_SMALL_FILE_
//! PACKING=1` a file whose stored image's LBA-rounded slot fits
//! `pack_max_slot_bytes()` (derived CHUNK/2) reserves a slot in the mount's
//! OPEN PACK BLOCK (one `fetch_add`), DMAs its image at `base + off`, and
//! commits the size-carrying mapping `bk:off:len` the read path already
//! decodes — its C8 reference in the same transaction. N tenants of one
//! block are N durable references; the block is RAM-pinned (`refcount =
//! tenants + 1`) while open and frees terminally at population 0.
//!
//! The twelve contracts, lever ON through the seam unless stated:
//!  1. N small files promoted at dismount occupy ≤ ceil(Σ ceil(image) /
//!     CHUNK) + volumes blocks; `layout_promoted_packed = N`.
//!  2. A remount at a DIFFERENT mount point reads all N byte-exact,
//!     `staged_payload_lost_reads = 0`; then a second population is
//!     allocated past the promoted offsets and the first re-verified — on
//!     a NON-empty ledger with the C8 oracle armed (the FIND-PK-2 shape).
//!  3. After remount `refcount(base) = tenants` for every pack block; the
//!     derived walk and the durable seed agree; the oracle is clean.
//!  4. Deleting one tenant is nonterminal (`pack_partial_frees + 1`, the
//!     block stays allocated, survivors byte-exact); the last is terminal
//!     through the ROUTER ladder (`pack_terminal_frees + 1`, the reclaim
//!     queue takes it, the block returns to the free list).
//!  5. An image with slot > CHUNK/2 takes its own block
//!     (`pack_own_block_promotions`); `SQUEEZEFS_PACK_MAX_SLOT_BYTES` moves
//!     the boundary.
//!  6. An overwrite of a packed tenant re-stages, releases the old tenant
//!     (partial free) and re-packs on its next promotion.
//!  7. 💥 kill-9 between a tenant's DMA and its commit, and between two
//!     tenants' commits: committed tenants byte-exact, uncommitted stay
//!     ring-resident and promote at the next unmount, fsck C2/C3/C8 empty,
//!     `block_untracked_free_refusals = 0`.
//!  8. An online fsck during LIVE concurrent promotion reports nothing:
//!     `fsck_findings = 0` across ×10 runs under a create+fsync storm; in
//!     the quiet state the pack-open ledger excuses exactly the +1 pin
//!     (`pack_ledger_exempted = open packs`); every pack seals before
//!     "Dismount clean".
//!  9. A sealed pack block's word is STABLE: the drain mover moves it
//!     (`Moved`, every tenant byte-exact at the destination) and a
//!     peer-shaped shipped free of one tenant answers `NonTerminal`, never
//!     `Refused` (`block_live_free_refusals = 0`).
//! 10. A refill that hits `StorageFull` leaves every batch entry
//!     resident-and-counted (OQ-1: the arm stops for the batch, one WARN).
//! 11. A refused commit releases the tenant's reference after the guard
//!     (`pack_slots_abandoned + 1`) — terminal when the pack sealed and
//!     every sibling was deleted.
//! 12. Lever OFF ⇒ `layout_promoted_packed = 0` and the block arm's shape
//!     byte-identical (`bk:0:len`, its `+ref` staged).
//!
//! Plus the allocator's co-writer release arms, driven directly (PK2's
//! co-writer posture never packs — the batch pack is PK4's).
//!
//! Two venues: the mount-class contracts (1, 2, 7, 8, 12's mount face) run
//! the real daemon on an unprivileged file-backed sandbox and self-skip
//! through the testkit where a mount is not possible; the rest run the
//! in-process `SqueezefsFilesystem` fixture (the `fsync_economy_tests`
//! shape, plus the drain suite's job fabric).

use fuse3::raw::{Filesystem, Request};
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsync_economy;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobState, MoverCtx};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig};
use squeezefs_testkit::{mount_supported, site};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const KIB: usize = 1024;
const BLOCK: usize = 4 * 1024 * KIB;
/// The mount's default `--dismount-wait`.
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
const LEVER: &str = "SQUEEZEFS_SMALL_FILE_PACKING";
const FSYNC_LEVER: &str = "SQUEEZEFS_FSYNC_PROMOTE_STAGED";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic per-file content, salted by the index (a zeros read or a
/// cross-tenant mix-up is caught).
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

fn metric(a: &squeezefs::fuse_client::Align64<AtomicU64>) -> u64 {
    a.load(Ordering::Relaxed)
}

// ===========================================================================
// The mount-class venue
// ===========================================================================

/// The probe's population: 200 small files — sizes strictly above the
/// one-page inline ceiling, far below the block, all 4 KiB multiples on a
/// passthrough volume (the image IS the file; pad 0).
const FILES: usize = 200;
const SIZES_KIB: [usize; 4] = [8, 16, 32, 64];

fn file_len(idx: usize) -> usize {
    SIZES_KIB[idx % SIZES_KIB.len()] * KIB
}

fn file_name(idx: usize) -> String {
    format!("packed_{idx:04}.bin")
}

/// Σ ceil(image) over the population, in blocks, rounded up — the G-PK1
/// bound's first term (every size is a 4 KiB multiple: pad 0).
fn population_blocks() -> u64 {
    let total: u64 = (0..FILES).map(|i| file_len(i) as u64).sum();
    total.div_ceil(CHUNK_SIZE)
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_pack_{tag}_{}", std::process::id()));
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

    /// SIGKILL the daemon (no teardown runs) and detach the dead mount.
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

/// Spawn the real daemon (zc OFF — the fstests runner's default; the
/// one-page inline ceiling pinned so the 8–64 KiB population stays staged;
/// a short fsck settle so the ×10 online runs fit the suite). `extra_env`
/// rides on top — the packing lever IS the seam.
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
        // The online fsck rides the admin lane, whose KD-7 gate refuses a
        // `-dirty` build identity (a dev worktree) without the counted dev
        // override — both ends announce it.
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

/// Allocated chunks on the mounted set — `statvfs`'s used bytes are the
/// allocator's `used_blocks × chunk` (RAM-maintained, no I/O).
fn used_chunks(mnt: &Path) -> u64 {
    let c = std::ffi::CString::new(mnt.to_str().unwrap()).unwrap();
    // SAFETY: a zeroed statvfs is a valid out-parameter; the return is checked.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs({})", mnt.display());
    let used = (st.f_blocks as u64 - st.f_bavail as u64) * st.f_frsize as u64;
    used / CHUNK_SIZE
}

fn write_close(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn write_fsync(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
}

fn syncfs(mnt: &Path) {
    let root = std::fs::File::open(mnt).expect("open mount root");
    use std::os::fd::AsRawFd;
    // SAFETY: syncfs on a live fd; the return is checked.
    let rc = unsafe { libc::syncfs(root.as_raw_fd()) };
    assert_eq!(rc, 0, "syncfs({}) failed", mnt.display());
}

/// Write the population (open → write → close, no fsync) and wait for it
/// to settle as N staged-layout files with no active-block custody.
fn populate_staged_files(mnt: &Path) {
    for idx in 0..FILES {
        write_close(&mnt.join(file_name(idx)), &pattern(idx, file_len(idx)));
    }
    syncfs(mnt);
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
             blocks (staged = {staged}, active = {active})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_files(mnt: &Path) -> usize {
    (0..FILES)
        .filter(|&idx| {
            let path = mnt.join(file_name(idx));
            let got =
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            got != pattern(idx, file_len(idx))
        })
        .count()
}

/// The striped anchor: one file past the block size, so the durable
/// block-reference ledger is NON-EMPTY when the packed files join it (an
/// empty ledger is declined at mount and the walk backfills — the shape
/// that hid FIND-PK-2 from every suite).
const ANCHOR: &str = "anchor_striped.bin";
const ANCHOR_BLOCKS: u64 = 2;

fn anchor_bytes() -> Vec<u8> {
    pattern(usize::MAX, BLOCK + 64 * KIB)
}

/// `squeezefs fsck <mnt> --json` against the live mount: the report's
/// counters and findings.
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

// ---------------------------------------------------------------------------
// Contracts 1 + 2 (+ the mount face of 3): the dismount pack, the foreign
// read, the second population on a non-empty ledger
// ---------------------------------------------------------------------------

/// The dismount pass packs the whole population into ≤ ceil(Σ/CHUNK) +
/// volumes blocks (contract 1: 200 files, ≈ 5.9 MiB ⇒ ≤ 3 blocks against
/// 200 today); a remount elsewhere reads every file byte-exact from the
/// shared backend with `staged_payload_lost_reads = 0`, the C8 oracle on
/// the NON-empty ledger reads drift 0 with one record per tenant
/// (contracts 2, 3), and fresh striped allocation past the promoted
/// offsets clobbers nothing (the FIND-PK-2 shape).
#[test]
fn a_clean_unmount_packs_the_population_into_shared_blocks_other_clients_read_exactly() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("dismount");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, &[(LEVER, "1")]);
    assert_eq!(
        stats_json(&mnt)["small_file_packing"],
        true,
        "the lever is published live"
    );

    // The striped anchor first: its blocks are the ledger's population.
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    populate_staged_files(&mnt);
    let before = used_chunks(&mnt);
    assert_eq!(
        before, ANCHOR_BLOCKS,
        "only the anchor is on the data plane"
    );
    assert_eq!(
        stat_u64(&mnt, "invariant_tripwires"),
        0,
        "no concurrency-outcome tripwire fired on the writer; log: {}",
        log.display()
    );
    mount.umount_timed();

    // The engagement line: every file packed, none took its own block.
    assert!(
        log_contains(&log, &format!("promoted {FILES} staged-layout file(s)")),
        "the teardown must report promoting all {FILES} files; log: {}",
        log.display()
    );
    assert!(
        log_contains(&log, &format!("{FILES} packed, 0 to blocks")),
        "the teardown must report {FILES} PACKED and 0 to blocks (contract 1's engagement); \
         log: {}",
        log.display()
    );
    assert!(
        log_contains(&log, "dismount sealed 1 open pack block(s)"),
        "the open pack seals at dismount, before \"Dismount clean\"; log: {}",
        log.display()
    );
    assert!(log_contains(&log, "Dismount clean"));

    // Another client, the C8 oracle armed.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    let s = stats_json(&mnt2);
    let drift = s["metrics"]["meta_kv_block_refs_drift"].as_u64().unwrap();
    let recovered = s["metrics"]["meta_kv_block_refs_recovered"]
        .as_u64()
        .unwrap();
    assert_eq!(
        drift,
        0,
        "the C8 oracle: durable vs derived; log: {}",
        log2.display()
    );
    assert_eq!(
        recovered,
        ANCHOR_BLOCKS + FILES as u64,
        "one durable reference per tenant + the anchor's blocks seeded the allocator"
    );
    // Contract 1's space law, read on the remount: the packed population
    // occupies ≤ ceil(Σ ceil(image) / CHUNK) + one open-tail block.
    let used = used_chunks(&mnt2);
    let bound = ANCHOR_BLOCKS + population_blocks() + 1;
    assert!(
        used <= bound,
        "{FILES} packed files + the anchor occupy {used} blocks; bound {bound} \
         (= {ANCHOR_BLOCKS} anchor + ceil(Σ/CHUNK) = {} + 1 open tail) — one block per file \
         would be {}",
        population_blocks(),
        ANCHOR_BLOCKS + FILES as u64
    );
    assert!(
        used >= ANCHOR_BLOCKS + population_blocks(),
        "the population cannot occupy fewer blocks than its bytes: {used}"
    );

    // Contract 2: byte-exact from the shared backend alone.
    assert_eq!(
        verify_files(&mnt2),
        0,
        "every packed file reads byte-exact elsewhere"
    );
    assert_eq!(
        stat_u64(&mnt2, "staged_payload_lost_reads"),
        0,
        "no read degraded to zeros; log: {}",
        log2.display()
    );

    // The second population, past the promoted offsets: a ledger-blind
    // pack block would recover FREE and be minted here.
    for i in 0..4 {
        write_fsync(
            &mnt2.join(format!("fresh_{i}.bin")),
            &pattern(1_000_000 + i, BLOCK + 4096),
        );
    }
    assert_eq!(
        verify_files(&mnt2),
        0,
        "packed files read wrong after fresh striped allocation on the remount — a pack \
         block was re-minted under a new owner; log: {}",
        log2.display()
    );
    let got = std::fs::read(mnt2.join(ANCHOR)).unwrap();
    let want = anchor_bytes();
    if got != want {
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        let zeros = got.iter().filter(|&&b| b == 0).count();
        panic!(
            "the striped anchor read wrong: len {} vs {}, first diff at {:?}, {zeros} zero              bytes; log: {}",
            got.len(),
            want.len(),
            first,
            log2.display()
        );
    }
    assert_eq!(stat_u64(&mnt2, "block_untracked_free_refusals"), 0);
    assert_eq!(stat_u64(&mnt2, "pack_release_untracked_noops"), 0);
    mount2.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 12 (mount face): lever OFF is byte-identical to the block arm
// ---------------------------------------------------------------------------

/// With the lever OFF (`SQUEEZEFS_SMALL_FILE_PACKING=0` — the A/B control
/// since the PK7 flip; the shipped default through PK6) the dismount pass
/// takes one block per file — the FIXED block arm, its `+ref` staged
/// (drift 0) — and reports `0 packed`; the space law reads N blocks.
#[test]
fn lever_off_promotes_one_block_per_file_and_packs_nothing() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("leveroff");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, &[(LEVER, "0")]);
    assert_eq!(stats_json(&mnt)["small_file_packing"], false);
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    populate_staged_files(&mnt);
    mount.umount_timed();
    assert!(
        log_contains(&log, &format!("0 packed, {FILES} to blocks")),
        "lever OFF: every file takes its own block; log: {}",
        log.display()
    );
    assert!(!log_contains(&log, "open pack block"));

    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(stat_u64(&mnt2, "meta_kv_block_refs_drift"), 0);
    assert_eq!(
        used_chunks(&mnt2),
        ANCHOR_BLOCKS + FILES as u64,
        "one block per file — the 64× law the lever leaves in force when OFF"
    );
    assert_eq!(verify_files(&mnt2), 0);
    mount2.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 15: the DEFAULT packs (PK7's flip, 2026-09-10)
// ---------------------------------------------------------------------------

/// A plain mount — no lever in its environment — PACKS: `small_file_packing`
/// reads true on `.stats`, the dismount pass reports `N packed, 0 to
/// blocks`, the space law reads ≈ Σ slots / CHUNK blocks (not N), and every
/// file is byte-exact from a second mount point with the oracle clean. The
/// counted decision: `.benchmarks/2026-09-10-packing-rows-squeeze-test.md`
/// (A-B-B-A on the acceptance venue — files/s +0.5 %, fsync −3.7 %,
/// device/user bytes 1.00× both arms, 96,000 → 375 blocks). Red on the
/// pre-flip registry default (`off`).
#[test]
fn a_plain_mount_packs_by_default() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("default");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, &[]);
    assert_eq!(
        stats_json(&mnt)["small_file_packing"],
        true,
        "the lever's registry default is ON since PK7"
    );
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    populate_staged_files(&mnt);
    mount.umount_timed();
    assert!(
        log_contains(&log, &format!("{FILES} packed, 0 to blocks")),
        "the default dismount pass packs every staged file; log: {}",
        log.display()
    );

    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(stat_u64(&mnt2, "meta_kv_block_refs_drift"), 0);
    let used = used_chunks(&mnt2);
    assert!(
        used < ANCHOR_BLOCKS + FILES as u64 / 8,
        "packed: {used} blocks for {FILES} files (+ {ANCHOR_BLOCKS} anchor) — the one-block-per-file law would read {}",
        ANCHOR_BLOCKS + FILES as u64
    );
    assert_eq!(verify_files(&mnt2), 0);
    mount2.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 7 💥: kill-9 inside the two crash windows
// ---------------------------------------------------------------------------

/// The stall seam parks a packed tenant between its slot DMA and its
/// layout commit; the fsync lever makes each fsync a promotion.
const STALL_MS: &str = "2500";

/// fsync `path` on a helper thread (the fsync parks in the seam's window)
/// and return once the daemon is inside that window.
fn fsync_in_background(path: PathBuf) -> std::thread::JoinHandle<()> {
    let h = std::thread::spawn(move || {
        if let Ok(f) = std::fs::File::open(&path) {
            let _ = f.sync_all();
        }
    });
    std::thread::sleep(Duration::from_millis(800));
    h
}

/// Stage `files` as DURABLE staged-layout files: written and fsync'd on a
/// mount with the promotion lever OFF (the fsync persists the staged layout
/// — `file_type = staged`, `file_id`, size — and promotes nothing), then
/// kill-9'd so no dismount pass promotes them. The same-mount-point remount
/// recovers the ring entries (the residue note's §2 contract).
///
/// Why the crash legs need this: a file that was only `write` + `close`d
/// has a RAM-only layout until the RELEASE handler's BACKGROUND persist
/// lands, and the fsync-with-lever handler runs its promotion leg (and the
/// stall seam) AHEAD of its own layout persist — so a kill inside the
/// window can land before any layout was ever durable, and the file reads
/// as size 0 afterwards. That is the POSIX "fsync never returned" case, not
/// §5.4's: the crash contract is about a TENANT whose staged layout is
/// durable and whose only copy is the ring.
fn stage_durable(meta: &Path, mnt: &Path, log: &Path, files: &[(&str, usize, usize)]) {
    let mut m = spawn_mount(meta, mnt, log, &[(LEVER, "1")]);
    for (name, idx, len) in files {
        write_fsync(&mnt.join(name), &pattern(*idx, *len));
    }
    assert_eq!(
        stat_u64(mnt, "layout_promoted_packed"),
        0,
        "the staging mount promotes nothing (fsync lever off)"
    );
    m.kill9();
}

/// Leg A — kill-9 between the FIRST tenant's DMA and its commit: nothing
/// committed, so the pack block recovers FREE (no record names it), the
/// file stays ring-resident and byte-exact, fsck/oracle empty. Leg B —
/// kill-9 between two tenants' commits (tenant 1 committed, tenant 2
/// mid-window): the block recovers with exactly the committed tenant,
/// tenant 2 stays ring-resident, both byte-exact; at the next clean unmount
/// the resident files promote and pack. Every crashed tenant enters its
/// window with a DURABLE staged layout (`stage_durable`).
#[test]
fn kill9_inside_the_pack_windows_loses_nothing_and_leaks_nothing() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("kill9");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let a_len = 24 * KIB;
    let b_len = 40 * KIB;
    let c_len = 8 * KIB;

    // ---- Leg A: the first tenant's window --------------------------------
    let log_a0 = base.join("a0.log");
    let mut m = spawn_mount(&meta, &mnt, &log_a0, &[(LEVER, "1")]);
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    m.kill9();
    stage_durable(&meta, &mnt, &base.join("a1.log"), &[("a.bin", 1, a_len)]);

    let log_a = base.join("a.log");
    let mut m = spawn_mount(
        &meta,
        &mnt,
        &log_a,
        &[
            (LEVER, "1"),
            (FSYNC_LEVER, "1"),
            ("SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS", STALL_MS),
        ],
    );
    assert_eq!(
        std::fs::read(mnt.join("a.bin")).unwrap(),
        pattern(1, a_len),
        "leg A precondition: the durable staged file is ring-resident on the remount"
    );
    let h = fsync_in_background(mnt.join("a.bin"));
    // The pack block is allocated and the tenant DMA'd: the mid-window state.
    assert_eq!(stat_u64(&mnt, "pack_blocks_opened"), 1, "the pack opened");
    assert_eq!(
        stat_u64(&mnt, "layout_promoted_packed"),
        0,
        "nothing committed yet"
    );
    m.kill9();
    let _ = h.join();

    let log_a2 = base.join("a2.log");
    let mut m = spawn_mount(
        &meta,
        &mnt,
        &log_a2,
        &[(LEVER, "1"), ("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")],
    );
    assert_eq!(
        stat_u64(&mnt, "meta_kv_block_refs_drift"),
        0,
        "leg A: the oracle"
    );
    // Read on the REMOUNT (a live mount's count can carry a displaced
    // block still in the reclaim queue): only the anchor is allocated.
    assert_eq!(
        used_chunks(&mnt),
        ANCHOR_BLOCKS,
        "leg A: a pack block no tenant committed recovers FREE"
    );
    assert_eq!(
        std::fs::read(mnt.join("a.bin")).unwrap(),
        pattern(1, a_len),
        "leg A: the uncommitted tenant is ring-resident at the same mount point"
    );
    let report = online_fsck(&mnt);
    assert_eq!(
        report["counters"]["findings"], 0,
        "leg A: fsck clean: {report}"
    );
    assert_eq!(stat_u64(&mnt, "block_untracked_free_refusals"), 0);
    m.umount_timed();
    assert!(
        log_contains(&log_a2, "1 packed, 0 to blocks"),
        "leg A: the resident file promotes (packed) at the next clean unmount; log: {}",
        log_a2.display()
    );

    // ---- Leg B: between two tenants' commits ------------------------------
    // Tenant 2 and the bystander enter the leg with DURABLE staged layouts,
    // ring-resident, un-promoted.
    stage_durable(
        &meta,
        &mnt,
        &base.join("b0.log"),
        &[("b2.bin", 3, b_len), ("c.bin", 4, c_len)],
    );
    let log_b = base.join("b.log");
    let mut m = spawn_mount(
        &meta,
        &mnt,
        &log_b,
        &[
            (LEVER, "1"),
            (FSYNC_LEVER, "1"),
            ("SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS", STALL_MS),
        ],
    );
    let used1 = used_chunks(&mnt);
    // Tenant 1 commits (its fsync waits the stall out).
    write_fsync(&mnt.join("b1.bin"), &pattern(2, b_len));
    assert_eq!(
        stat_u64(&mnt, "layout_promoted_packed"),
        1,
        "tenant 1 committed"
    );
    assert_eq!(used_chunks(&mnt), used1 + 1, "one pack block opened");
    // Tenant 2 is DMA'd into the same open block and parked; the bystander
    // stays plain staged (never re-fsync'd).
    let h = fsync_in_background(mnt.join("b2.bin"));
    assert_eq!(
        stat_u64(&mnt, "pack_blocks_opened"),
        1,
        "tenant 2 shares the open block"
    );
    m.kill9();
    let _ = h.join();

    let log_b2 = base.join("b2.log");
    let mut m = spawn_mount(
        &meta,
        &mnt,
        &log_b2,
        &[(LEVER, "1"), ("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")],
    );
    let s = stats_json(&mnt);
    assert_eq!(
        s["metrics"]["meta_kv_block_refs_drift"], 0,
        "leg B: the oracle"
    );
    assert_eq!(
        used_chunks(&mnt),
        used1 + 1,
        "leg B: the pack block recovers ALLOCATED (tenant 1 names it); tenant 2's slot is \
         dead bytes inside it, never a leak and never a second block"
    );
    assert_eq!(
        std::fs::read(mnt.join("b1.bin")).unwrap(),
        pattern(2, b_len)
    );
    assert_eq!(
        std::fs::read(mnt.join("b2.bin")).unwrap(),
        pattern(3, b_len),
        "leg B: the mid-window tenant stays ring-resident"
    );
    assert_eq!(std::fs::read(mnt.join("c.bin")).unwrap(), pattern(4, c_len));
    let report = online_fsck(&mnt);
    assert_eq!(
        report["counters"]["findings"], 0,
        "leg B: fsck clean: {report}"
    );
    assert_eq!(stat_u64(&mnt, "block_untracked_free_refusals"), 0);
    assert_eq!(stat_u64(&mnt, "staged_payload_lost_reads"), 0);
    // The next clean unmount promotes both resident files — packed.
    m.umount_timed();
    assert!(
        log_contains(&log_b2, "2 packed, 0 to blocks"),
        "leg B: the two resident files pack at the next clean unmount; log: {}",
        log_b2.display()
    );

    // Everything reads back from a fresh client, oracle clean.
    let mnt3 = base.join("mnt3");
    let log3 = base.join("c.log");
    let mut m3 = spawn_mount(&meta, &mnt3, &log3, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(stat_u64(&mnt3, "meta_kv_block_refs_drift"), 0);
    for (name, idx, len) in [
        ("a.bin", 1, a_len),
        ("b1.bin", 2, b_len),
        ("b2.bin", 3, b_len),
        ("c.bin", 4, c_len),
    ] {
        assert_eq!(
            std::fs::read(mnt3.join(name)).unwrap(),
            pattern(idx, len),
            "{name} byte-exact from a foreign client"
        );
    }
    assert_eq!(stat_u64(&mnt3, "staged_payload_lost_reads"), 0);
    m3.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 8: online fsck under live concurrent promotion ×10
// ---------------------------------------------------------------------------

/// Under a create+fsync storm (each fsync a packed promotion) ten online
/// fsck runs report nothing — mid-flight tenants ride the in-flight
/// registry, the open pack's +1 pin rides the pack-open ledger. In the
/// quiet state the ledger excuses exactly one suspect per open pack. Every
/// pack seals before "Dismount clean".
#[test]
fn online_fsck_during_live_packing_reports_nothing_ten_times() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("fsck");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut m = spawn_mount(&meta, &mnt, &log, &[(LEVER, "1"), (FSYNC_LEVER, "1")]);
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());

    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU64::new(0));
    let writer = {
        let stop = stop.clone();
        let written = written.clone();
        let mnt = mnt.clone();
        std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                write_fsync(
                    &mnt.join(format!("storm_{i:05}.bin")),
                    &pattern(i, 12 * KIB),
                );
                written.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        })
    };
    // Let the storm engage before the first run.
    while written.load(Ordering::Relaxed) < 8 {
        std::thread::sleep(Duration::from_millis(20));
    }
    for run in 0..10 {
        let report = online_fsck(&mnt);
        assert_eq!(
            report["counters"]["findings"], 0,
            "run {run}: an online fsck under live packing found something: {report}"
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer thread");
    let n = written.load(Ordering::Relaxed);
    assert!(n >= 10, "the storm engaged ({n} files)");
    assert_eq!(
        stat_u64(&mnt, "layout_promoted_packed"),
        n,
        "every storm file was packed by its fsync"
    );

    // Quiet: the pin is the only discrepancy, excused exactly once per
    // open pack.
    let open = stat_u64(&mnt, "pack_open_blocks");
    assert_eq!(open, 1, "one open pack (one data volume)");
    let exempted0 = stat_u64(&mnt, "fsck_pack_ledger_exempted");
    let report = online_fsck(&mnt);
    assert_eq!(report["counters"]["findings"], 0, "quiet: {report}");
    assert_eq!(
        report["counters"]["pack_ledger_exempted"], open,
        "quiet: the pack-open ledger excuses exactly the +1 pin of each open pack: {report}"
    );
    assert_eq!(
        stat_u64(&mnt, "fsck_pack_ledger_exempted"),
        exempted0 + open
    );
    assert_eq!(stat_u64(&mnt, "pack_release_untracked_noops"), 0);

    m.umount_timed();
    let text = std::fs::read_to_string(&log).unwrap();
    let seal = text
        .find("dismount sealed 1 open pack block(s)")
        .expect("the open pack sealed at dismount");
    let clean = text.find("Dismount clean").expect("Dismount clean");
    assert!(seal < clean, "the seal precedes \"Dismount clean\"");
    let _ = std::fs::remove_dir_all(&base);
}

// ===========================================================================
// The in-process venue
// ===========================================================================

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Lever seams return to the knob on drop.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::test_set_small_file_packing(None);
        squeezefs::routing::set_pack_max_slot_bytes_override(None);
        squeezefs::routing::set_inline_max_bytes_override(None);
        fsync_economy::test_set_promote_staged(None);
        squeezefs::fuse_client::set_mount_posture(squeezefs::fuse_client::MountPosture::Writer);
    }
}

fn arm_levers() -> LeverGuard {
    squeezefs::routing::set_inline_max_bytes_override(Some(squeezefs::routing::INLINE_MAX_FLOOR));
    squeezefs::routing::test_set_small_file_packing(Some(true));
    fsync_economy::test_set_promote_staged(Some(true));
    LeverGuard
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

fn make_dev_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// Mount-shaped fixture: `n` data volumes registered (the first is the
/// default slot), a staging dir, the job fabric with the mover context
/// wired (contract 9's drain), allocator recovery like a mount.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    records: Vec<DataVolumeRecord>,
    _staging: TempDir,
}

/// A fresh format + open: `n` volumes of `dev_bytes` each.
async fn open_fresh(dir: &Path, n: usize, dev_bytes: u64, tag: &str) -> (PathBuf, Fx) {
    let meta = make_dev_file(dir, &format!("meta-{tag}"), 256 * 1024 * 1024);
    let paths: Vec<PathBuf> = (0..n)
        .map(|i| make_dev_file(dir, &format!("oss{}-{tag}", i + 1), dev_bytes))
        .collect();
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    format_meta(&meta, &refs).await;
    let records = base_format_config(&refs).resolved_data_volumes();
    let fx = open_at(&meta, &records).await;
    (meta, fx)
}

async fn open_at(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    // Allocator refcount recovery exactly like a mount.
    for kv in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }
    let fs = Arc::new(fs);
    let fabric = JobFabric::start(
        routed.clone(),
        2,
        100,
        Some(MoverCtx::new(fs.router.clone(), fs.mover_quiesce_probe())),
    )
    .await
    .expect("fabric start");
    fs.job_fabric.store(Arc::new(Some(fabric.clone())));
    Fx {
        fs,
        meta: routed,
        fabric,
        records: records.to_vec(),
        _staging: staging,
    }
}

impl Fx {
    /// Mount-faithful close: the dismount seal first (every open pack's
    /// pin releases and its pack-open ledger entry — process-global —
    /// leaves with it), then the reclaim drain, then the volumes.
    async fn close(self) {
        self.fs.router.seal_open_packs().await;
        self.fabric.shutdown_abrupt().await;
        self.fs.router.backend_router.reclaim_drain().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    fn alloc(&self, idx: usize) -> Arc<BlockAllocator> {
        self.fs
            .router
            .backend_router
            .backends
            .get(&self.records[idx].id)
            .expect("registered backend")
            .block_allocator
            .clone()
    }

    /// Force every NEW placement onto volume `idx`.
    fn place_only_on(&self, idx: usize) {
        for (i, rec) in self.records.iter().enumerate() {
            self.fs
                .router
                .backend_router
                .set_health_override(&rec.id, i != idx)
                .unwrap();
        }
    }

    fn clear_health_overrides(&self) {
        for rec in &self.records {
            self.fs
                .router
                .backend_router
                .set_health_override(&rec.id, false)
                .unwrap();
        }
    }

    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino
    }

    async fn write_at(&self, ino: u64, off: u64, data: &[u8]) {
        let written = self
            .fs
            .write(
                req(),
                ino,
                0,
                off,
                bytes::Bytes::copy_from_slice(data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
            .written;
        assert_eq!(written as usize, data.len(), "short write at {off}");
    }

    async fn fsync(&self, ino: u64) {
        self.fs
            .fsync(req(), ino, 0, false)
            .await
            .unwrap_or_else(|e| panic!("fsync ino {ino} failed: {e:?}"));
    }

    async fn read(&self, ino: u64, len: usize) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, 0, len as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read ino {ino} failed: {e:?}"))
            .data
            .to_vec()
    }

    /// Unlink + reclaim: block frees are deferred to FORGET + the reclaim
    /// pool (armed at FUSE INIT, which this harness never runs) — drive the
    /// batch entry point directly, exactly what the kernel's forget reaches.
    async fn unlink(&self, name: &str, ino: u64) {
        let _ = self.fs.release(req(), ino, 0, 0, 0, true).await;
        self.fs
            .unlink(req(), 1, OsStr::new(name))
            .await
            .unwrap_or_else(|e| panic!("unlink {name} failed: {e:?}"));
        self.fs.reclaim_orphaned_batch(vec![ino]).await;
    }

    /// A staged-layout file of `len` bytes (resident in the ring, no map).
    async fn staged_file(&self, name: &str, len: usize, tag: usize) -> (u64, String) {
        let ino = self.create(name).await;
        self.write_at(ino, 0, &pattern(tag, len)).await;
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        let fid = m.file_id.as_deref().expect("file_id").to_string();
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_some(),
            "fixture premise: ring-resident"
        );
        (ino, fid)
    }

    /// The tenant mapping `block_map[0]` of a promoted file, decoded:
    /// `(base key, off, len)`.
    fn mapping(&self, ino: u64) -> (String, u64, usize) {
        let m = self.fs.router.metadata_cache.get(&ino).expect("layout");
        let s = m
            .block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[0]: {m:?}"));
        let (prefix, rest) = match s.find("://") {
            Some(p) => s.split_at(p + 3),
            None => ("", s.as_str()),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        assert_eq!(parts.len(), 3, "size-carrying mapping: {s}");
        (
            format!("{prefix}{}", parts[0]),
            parts[1].parse().unwrap(),
            parts[2].parse().unwrap(),
        )
    }

    /// The device offset a base key names (through the router's parser).
    fn offset_of(&self, base_key: &str) -> u64 {
        self.fs
            .router
            .backend_router
            .parse_block_offset(base_key)
            .expect("base key parses")
    }

    /// The C8 oracle: durable vs derived, the drifting blocks.
    async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.fs
            .router
            .backend_router
            .verify_durable_block_refs(&self.meta)
            .await
            .expect("verification pass")
    }
}

// ---------------------------------------------------------------------------
// Contract 3 (+ the pin's arithmetic): refcount = tenants after remount
// ---------------------------------------------------------------------------

/// Six fsync-promoted files share ONE pack block at distinct LBA-aligned
/// offsets; while the pack is open `refcount = tenants + 1` (the pin); the
/// dismount seal releases the pin (`refcount = tenants`); a reopen with
/// mount-style recovery seeds `refcount = tenants` from the durable
/// ledger; the oracle is clean; every tenant reads byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_remount_every_pack_block_has_refcount_equal_to_its_tenants() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "remount").await;
    let opened0 = metric(&METRICS.pack_blocks_opened);
    let packed0 = metric(&METRICS.layout_promoted_packed);

    const N: usize = 6;
    let len = 16 * KIB;
    let mut inos = Vec::new();
    for i in 0..N {
        let (ino, fid) = fx.staged_file(&format!("t{i}.bin"), len, 10 + i).await;
        fx.fsync(ino).await;
        assert!(
            fx.fs.router.cache.nvme.read_staged(&fid).is_none(),
            "the promotion released the ring entry"
        );
        inos.push(ino);
    }
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, N as u64);
    assert_eq!(
        metric(&METRICS.pack_blocks_opened) - opened0,
        1,
        "one pack block"
    );

    let (base, off0, len0) = fx.mapping(inos[0]);
    assert_eq!((off0, len0), (0, len), "the first tenant sits at slot 0");
    let mut offs = vec![off0];
    for &ino in &inos[1..] {
        let (b, off, l) = fx.mapping(ino);
        assert_eq!(b, base, "every tenant names the same base block");
        assert_eq!(l, len);
        assert_eq!(off % 4096, 0, "LBA-aligned slot");
        assert!(off + l as u64 <= CHUNK_SIZE, "inside the chunk");
        offs.push(off);
    }
    offs.sort_unstable();
    offs.dedup();
    assert_eq!(offs.len(), N, "distinct slots");
    assert!(metric(&METRICS.pack_promoted_bytes) >= (N * len) as u64);

    let alloc = fx.alloc(0);
    let offset = fx.offset_of(&base);
    assert_eq!(
        alloc.refcount(offset),
        Some(N as u32 + 1),
        "open pack: refcount = tenants + the packer's pin"
    );
    assert!(
        alloc.fill_incarnation(offset).is_some(),
        "the word is STABLE from the first tenant's DMA on (KD-3)"
    );
    assert!(
        squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "the open pack is in the pack-open ledger"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the oracle is clean while open"
    );

    let sealed0 = metric(&METRICS.pack_blocks_sealed_dismount);
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    assert_eq!(metric(&METRICS.pack_blocks_sealed_dismount) - sealed0, 1);
    assert_eq!(
        alloc.refcount(offset),
        Some(N as u32),
        "sealed: the pin released, refcount = tenants"
    );
    assert!(
        !squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "the seal leaves the ledger"
    );
    assert_eq!(metric(&METRICS.pack_blocks_abandoned), 0);
    let records = fx.records.clone();
    fx.close().await;

    // Remount: the durable ledger seeds the count.
    let fx = open_at(&meta, &records).await;
    let alloc = fx.alloc(0);
    assert_eq!(
        alloc.refcount(offset),
        Some(N as u32),
        "after remount: refcount = tenants (one C8 record per tenant)"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the oracle is clean after remount"
    );
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(fx.read(ino, len).await, pattern(10 + i, len), "tenant {i}");
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 4: partial frees until the last tenant
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_tenants_is_nonterminal_until_the_last_which_frees_through_the_ladder() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "delete").await;
    let len = 12 * KIB;
    let mut files = Vec::new();
    for i in 0..3 {
        let name = format!("d{i}.bin");
        let (ino, _) = fx.staged_file(&name, len, 20 + i).await;
        fx.fsync(ino).await;
        files.push((name, ino));
    }
    let (base, _, _) = fx.mapping(files[0].1);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    assert_eq!(alloc.refcount(offset), Some(3));
    let idx = offset / alloc.chunk_size();

    let partial0 = metric(&METRICS.pack_partial_frees);
    let terminal0 = metric(&METRICS.pack_terminal_frees);
    let queued0 = metric(&METRICS.block_free_reclaim_queued);
    let untracked0 = metric(&METRICS.block_untracked_free_refusals);

    fx.unlink(&files[0].0, files[0].1).await;
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        1,
        "nonterminal"
    );
    assert_eq!(metric(&METRICS.pack_terminal_frees) - terminal0, 0);
    assert_eq!(alloc.refcount(offset), Some(2), "the block stays allocated");
    assert!(!alloc.free_list_contains(idx));
    assert_eq!(fx.read(files[1].1, len).await, pattern(21, len), "survivor");
    assert_eq!(fx.read(files[2].1, len).await, pattern(22, len), "survivor");
    assert!(fx.drift().await.is_empty());

    fx.unlink(&files[1].0, files[1].1).await;
    assert_eq!(metric(&METRICS.pack_partial_frees) - partial0, 2);
    assert_eq!(alloc.refcount(offset), Some(1));
    assert_eq!(fx.read(files[2].1, len).await, pattern(22, len));

    fx.unlink(&files[2].0, files[2].1).await;
    assert_eq!(
        metric(&METRICS.pack_terminal_frees) - terminal0,
        1,
        "the last is terminal"
    );
    assert_eq!(metric(&METRICS.pack_partial_frees) - partial0, 2);
    assert_eq!(
        metric(&METRICS.block_free_reclaim_queued) - queued0,
        1,
        "the terminal free rides the ROUTER ladder's reclaim queue"
    );
    assert_eq!(alloc.refcount(offset), None, "untracked once terminal");
    fx.fs.router.backend_router.reclaim_drain().await;
    assert!(
        alloc.free_list_contains(idx),
        "back on the free list after the reclaim"
    );
    assert_eq!(
        metric(&METRICS.block_untracked_free_refusals) - untracked0,
        0
    );
    assert_eq!(metric(&METRICS.block_double_frees), 0);
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 5: the own-block threshold
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_above_half_a_chunk_takes_its_own_block_and_the_knob_moves_the_boundary() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "threshold").await;
    let big = 3 * 1024 * KIB; // 3 MiB > CHUNK/2 = 2 MiB, < the 4 MiB block
    let own0 = metric(&METRICS.pack_own_block_promotions);
    let block0 = metric(&METRICS.layout_promoted_block);
    let packed0 = metric(&METRICS.layout_promoted_packed);

    let (ino, _) = fx.staged_file("big.bin", big, 30).await;
    fx.fsync(ino).await;
    assert_eq!(metric(&METRICS.pack_own_block_promotions) - own0, 1);
    assert_eq!(
        metric(&METRICS.layout_promoted_block) - block0,
        1,
        "the block arm"
    );
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 0);
    let (_, off, len) = fx.mapping(ino);
    assert_eq!((off, len), (0, big), "bk:0:len — its own block");
    assert_eq!(fx.read(ino, big).await, pattern(30, big));

    // The knob moves the boundary: at CHUNK_SIZE everything packs.
    squeezefs::routing::set_pack_max_slot_bytes_override(Some(CHUNK_SIZE));
    let (ino2, _) = fx.staged_file("big2.bin", big, 31).await;
    fx.fsync(ino2).await;
    assert_eq!(
        metric(&METRICS.pack_own_block_promotions) - own0,
        1,
        "no new own-block"
    );
    assert_eq!(
        metric(&METRICS.layout_promoted_packed) - packed0,
        1,
        "packed"
    );
    let (_, _, len2) = fx.mapping(ino2);
    assert_eq!(len2, big);
    assert_eq!(fx.read(ino2, big).await, pattern(31, big));
    // And a small file after it overflows that pack (3 MiB + 12 KiB fits;
    // a second 3 MiB does not) — the overflowing reservation seals FULL.
    let full0 = metric(&METRICS.pack_blocks_sealed_full);
    let (ino3, _) = fx.staged_file("big3.bin", big, 32).await;
    fx.fsync(ino3).await;
    assert_eq!(
        metric(&METRICS.pack_blocks_sealed_full) - full0,
        1,
        "sealed full"
    );
    assert_eq!(fx.read(ino3, big).await, pattern(32, big));
    assert!(fx.drift().await.is_empty());
    squeezefs::routing::set_pack_max_slot_bytes_override(None);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 6: an overwrite of a packed tenant re-stages and re-packs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwriting_a_packed_tenant_restages_releases_the_slot_and_repacks() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "overwrite").await;
    let len = 20 * KIB;
    let (ino, _) = fx.staged_file("ow.bin", len, 40).await;
    fx.fsync(ino).await;
    // A sibling so the block outlives the overwritten tenant.
    let (sib, _) = fx.staged_file("sib.bin", len, 41).await;
    fx.fsync(sib).await;
    let (base, off_a, _) = fx.mapping(ino);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    let partial0 = metric(&METRICS.pack_partial_frees);
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let rc0 = alloc.refcount(offset).unwrap();

    // The whole-image overwrite: re-staged in the ring, the old tenant
    // reference released (partial: the sibling and the pin hold the block).
    fx.write_at(ino, 0, &pattern(42, len)).await;
    let m = fx.fs.router.metadata_cache.get(&ino).unwrap();
    assert_eq!(m.file_type, "staged");
    assert!(
        m.block_map.as_ref().is_none_or(|bm| bm.is_empty()),
        "re-staged: the map is released"
    );
    assert!(fx
        .fs
        .router
        .cache
        .nvme
        .read_staged(m.file_id.as_deref().unwrap())
        .is_some());
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        1,
        "the old slot's partial free"
    );
    assert_eq!(alloc.refcount(offset), Some(rc0 - 1));
    assert_eq!(fx.read(ino, len).await, pattern(42, len));

    // Its next promotion re-packs: a fresh slot (the old one is dead space).
    fx.fsync(ino).await;
    assert_eq!(
        metric(&METRICS.layout_promoted_packed) - packed0,
        1,
        "re-packed"
    );
    let (base_b, off_b, len_b) = fx.mapping(ino);
    assert_eq!(len_b, len);
    assert!(
        base_b != base || off_b != off_a,
        "a fresh slot, never the dead one ({base}:{off_a} vs {base_b}:{off_b})"
    );
    assert_eq!(fx.read(ino, len).await, pattern(42, len));
    assert_eq!(
        fx.read(sib, len).await,
        pattern(41, len),
        "the sibling is untouched"
    );
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 9: a sealed pack block's word is STABLE — movable, and a peer's
// tenant free answerable
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sealed_pack_block_moves_as_a_unit_and_answers_a_peers_tenant_free_nonterminal() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 2, 4 << 30, "move").await;
    // Everything on the second volume — the drain's victim.
    fx.place_only_on(1);
    let len = 32 * KIB;
    let mut inos = Vec::new();
    for i in 0..4 {
        let (ino, _) = fx.staged_file(&format!("mv{i}.bin"), len, 50 + i).await;
        fx.fsync(ino).await;
        inos.push(ino);
    }
    let (base, _, _) = fx.mapping(inos[0]);
    let victim_id = fx.records[1].id.clone();
    assert!(
        base.starts_with(&format!("{victim_id}://")),
        "placed on the victim volume: {base}"
    );
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(1);
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    assert_eq!(alloc.refcount(offset), Some(4));
    assert!(alloc.fill_incarnation(offset).is_some(), "STABLE word");

    // (a) the drain mover moves the sealed pack as a unit: every tenant
    // republished on the survivor with its own `:off:len`, byte-exact.
    fx.clear_health_overrides();
    let moved0 = metric(&METRICS.evacuate_blocks_moved);
    let deferred0 = metric(&METRICS.evacuate_deferred_staged_blocks);
    let job_id = fx
        .fs
        .admin_remove_data_volume(&victim_id, 100)
        .await
        .expect("remove-data admits");
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(120))
        .await
        .expect("evacuation terminal");
    assert_eq!(
        end,
        JobState::Completed,
        "the drain converges (Moved, never Deferred)"
    );
    assert!(metric(&METRICS.evacuate_blocks_moved) - moved0 >= 1);
    assert_eq!(
        metric(&METRICS.evacuate_deferred_staged_blocks) - deferred0,
        0,
        "a sealed pack block is never deferred"
    );
    let survivor = fx.records[0].id.clone();
    let mut dst_bases = std::collections::HashSet::new();
    for (i, &ino) in inos.iter().enumerate() {
        let (b, off, l) = fx.mapping(ino);
        // The survivor is the default slot, whose keys carry no `be://`
        // prefix (`persist_block_key`); the victim's did.
        assert!(
            !b.starts_with(&format!("{victim_id}://")),
            "moved off the victim: {b}"
        );
        assert_eq!(l, len);
        assert_eq!(
            off,
            (i as u64) * len as u64,
            "the tenant's own off survives the move"
        );
        dst_bases.insert(b);
        assert_eq!(
            fx.read(ino, len).await,
            pattern(50 + i, len),
            "tenant {i} byte-exact"
        );
    }
    assert_eq!(dst_bases.len(), 1, "moved as ONE block");
    assert!(fx.drift().await.is_empty());

    // (b) a peer-shaped shipped free of ONE tenant on the authority's
    // (moved, sealed) pack block: the co-writer's delete SHIPS its
    // durable −ref first (served here as the owner would commit it), then
    // `FreeBlocks` — which the authority answers NonTerminal (three
    // references remain), never the finding-24 UNSTABLE refusal and never
    // finding 23's stale-duplicate refusal.
    let (moved_base, _, _) = fx.mapping(inos[3]);
    let moved_offset = fx.offset_of(&moved_base);
    let survivor_alloc = fx.alloc(0);
    assert_eq!(survivor_alloc.refcount(moved_offset), Some(4));
    assert!(
        survivor_alloc.fill_incarnation(moved_offset).is_some(),
        "the mover published the destination's word"
    );
    let mapping3 = fx
        .fs
        .router
        .metadata_cache
        .get(&inos[3])
        .and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned()))
        .unwrap();
    let peer_ref = fx
        .fs
        .router
        .backend_router
        .block_ref_for(&mapping3, inos[3], 0)
        .expect("the tenant's reference resolves");
    squeezefs::meta_ship::publish::commit_block_refs(
        &fx.meta,
        inos[3],
        &[squeezefs::meta_backend::kv::block_refs::BlockRefOp::released(peer_ref)],
    )
    .await
    .expect("the peer's −ref commits on the owner");
    let live0 = metric(&METRICS.block_live_free_refusals);
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(&survivor);
    let idx = moved_offset / survivor_alloc.chunk_size();
    let verdicts = squeezefs::cowriter::execute_shipped_frees(
        &fx.fs.router.backend_router,
        &fx.meta,
        tag,
        &[idx],
        &squeezefs::cowriter::local_owner_view(),
    )
    .await
    .expect("the executor runs");
    assert_eq!(
        verdicts,
        vec![squeezefs::meta_ship::publish::FreeVerdict::NonTerminal],
        "one tenant of four: NonTerminal"
    );
    assert_eq!(
        metric(&METRICS.block_live_free_refusals) - live0,
        0,
        "never Refused"
    );
    assert_eq!(survivor_alloc.refcount(moved_offset), Some(3));
    for (i, &ino) in inos.iter().enumerate().take(3) {
        assert_eq!(fx.read(ino, len).await, pattern(50 + i, len), "sibling {i}");
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 10: StorageFull on the refill — the never-wrong degrade (OQ-1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refill_that_hits_storage_full_leaves_the_batch_resident_and_counted() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    // A 3-chunk data volume: one striped anchor of 3 blocks fills it.
    let (_meta, fx) = open_fresh(dir.path(), 1, 3 * CHUNK_SIZE, "enospc").await;
    let anchor = fx.create("anchor.bin").await;
    // BLOCK + 1 bytes: striped from the first write (blocks 0 and 1), then
    // block 2 whole — three chunks, the device's whole capacity.
    fx.write_at(anchor, 0, &vec![0x5Au8; BLOCK + 1]).await;
    fx.write_at(anchor, 2 * BLOCK as u64, &vec![0xA5u8; BLOCK])
        .await;
    fx.fsync(anchor).await;
    fx.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(fx.alloc(0).get_used_blocks(), 3, "the volume is full");

    let full0 = metric(&METRICS.pack_refill_storage_full);
    let stopped0 = metric(&METRICS.pack_arm_stopped_promotions);
    let noops0 = metric(&METRICS.fsync_promote_noops);
    let fails0 = metric(&METRICS.fsync_promote_failures);
    let mut staged = Vec::new();
    for i in 0..3 {
        let (ino, fid) = fx.staged_file(&format!("s{i}.bin"), 8 * KIB, 60 + i).await;
        staged.push((ino, fid));
    }
    for (ino, _) in &staged {
        fx.fsync(*ino).await;
    }
    assert_eq!(
        metric(&METRICS.pack_refill_storage_full) - full0,
        1,
        "ONE refill hit StorageFull — the arm stopped for the batch"
    );
    assert_eq!(
        metric(&METRICS.pack_arm_stopped_promotions) - stopped0,
        3,
        "every promotion of the batch answered resident-and-counted"
    );
    assert_eq!(
        metric(&METRICS.fsync_promote_noops) - noops0,
        3,
        "the fsync leg's no-ops"
    );
    assert_eq!(
        metric(&METRICS.fsync_promote_failures) - fails0,
        0,
        "not a failure"
    );
    assert!(fx.fs.router.packer.arm_stopped());
    for (ino, fid) in &staged {
        assert!(
            fx.fs.router.cache.nvme.read_staged(fid).is_some(),
            "ino {ino} stays ring-resident"
        );
    }
    assert_eq!(fx.alloc(0).get_used_blocks(), 3, "nothing allocated");

    // The next batch re-arms; with space returned the files pack.
    fx.unlink("anchor.bin", anchor).await;
    fx.fs.router.backend_router.reclaim_drain().await;
    fx.fs.router.packer.begin_promotion_batch();
    let packed0 = metric(&METRICS.layout_promoted_packed);
    for (ino, _) in &staged {
        fx.fsync(*ino).await;
    }
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 3);
    assert_eq!(metric(&METRICS.pack_refill_storage_full) - full0, 1);
    for (i, (ino, _)) in staged.iter().enumerate() {
        assert_eq!(fx.read(*ino, 8 * KIB).await, pattern(60 + i, 8 * KIB));
    }
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 11: a refused commit releases the tenant after the guard
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_commit_releases_the_tenant_reference_terminal_when_the_pack_is_alone() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "refused").await;
    let len = 8 * KIB;
    let alloc = fx.alloc(0);

    // Nonterminal: a committed sibling keeps the block.
    let (sib, _) = fx.staged_file("sib.bin", len, 70).await;
    fx.fsync(sib).await;
    let (base, _, _) = fx.mapping(sib);
    let offset = fx.offset_of(&base);
    let (ino, fid) = fx.staged_file("x.bin", len, 71).await;
    let token = fx.fs.router.dlm.get_fencing_token_ino(ino);
    let path = squeezefs::keys::inode_path(ino);
    let prepared = fx
        .fs
        .router
        .prepare_promotion(&path, &fid)
        .await
        .expect("prepare")
        .expect("something to promote");
    assert_eq!(
        alloc.refcount(offset),
        Some(3),
        "sibling + pin + the mid-flight tenant"
    );
    // The identity moves under the prepared tenant: a re-stage bumps the
    // generation, so the commit's re-check refuses.
    fx.write_at(ino, 0, &pattern(72, len)).await;
    let abandoned0 = metric(&METRICS.pack_slots_abandoned);
    let outcome = fx
        .fs
        .router
        .commit_promotion(prepared, token)
        .await
        .expect("a refused commit is not an error");
    assert_eq!(outcome, None, "refused");
    assert_eq!(metric(&METRICS.pack_slots_abandoned) - abandoned0, 1);
    assert_eq!(
        alloc.refcount(offset),
        Some(2),
        "the tenant's reference released"
    );
    assert_eq!(
        fx.read(ino, len).await,
        pattern(72, len),
        "the re-staged image serves"
    );
    assert_eq!(fx.read(sib, len).await, pattern(70, len));

    // Terminal: the pack sealed and no sibling — the release frees the
    // block through the ladder (reclaim-queued, then on the free list).
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    let (lone, lone_fid) = fx.staged_file("lone.bin", len, 73).await;
    let lone_token = fx.fs.router.dlm.get_fencing_token_ino(lone);
    let lone_path = squeezefs::keys::inode_path(lone);
    let fx_router = fx.fs.router.clone();
    let prepared = fx_router
        .prepare_promotion(&lone_path, &lone_fid)
        .await
        .expect("prepare")
        .expect("a fresh pack opens after the seal");
    // The prepared tenant's block: the one open pack.
    let lone_base = {
        let keys = squeezefs::jobs::pack_open_ledger();
        assert_eq!(keys.len(), 1, "one open pack");
        keys[0].clone()
    };
    let lone_offset = fx.offset_of(&lone_base);
    assert_eq!(alloc.refcount(lone_offset), Some(2), "pin + tenant");
    assert_eq!(
        fx_router.seal_open_packs().await,
        1,
        "sealed: the pin released"
    );
    assert_eq!(
        alloc.refcount(lone_offset),
        Some(1),
        "only the mid-flight tenant"
    );
    fx.write_at(lone, 0, &pattern(74, len)).await;
    let queued0 = metric(&METRICS.block_free_reclaim_queued);
    let untracked0 = metric(&METRICS.block_untracked_free_refusals);
    assert_eq!(
        fx_router
            .commit_promotion(prepared, lone_token)
            .await
            .unwrap(),
        None
    );
    assert_eq!(metric(&METRICS.pack_slots_abandoned) - abandoned0, 2);
    assert_eq!(
        alloc.refcount(lone_offset),
        None,
        "TERMINAL: the block is gone"
    );
    assert_eq!(
        metric(&METRICS.block_free_reclaim_queued) - queued0,
        1,
        "through the ladder"
    );
    fx.fs.router.backend_router.reclaim_drain().await;
    assert!(alloc.free_list_contains(lone_offset / alloc.chunk_size()));
    assert_eq!(
        metric(&METRICS.block_untracked_free_refusals) - untracked0,
        0
    );
    assert_eq!(metric(&METRICS.block_double_frees), 0);
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 12 (in-process face): lever OFF — the block arm, `bk:0:len`,
// its reference staged
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_takes_the_block_arm_with_its_reference_staged() {
    let _g = serial().await;
    let _l = arm_levers();
    squeezefs::routing::test_set_small_file_packing(Some(false));
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "off").await;
    let len = 16 * KIB;
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let block0 = metric(&METRICS.layout_promoted_block);
    let opened0 = metric(&METRICS.pack_blocks_opened);
    let (ino, _) = fx.staged_file("off.bin", len, 80).await;
    fx.fsync(ino).await;
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 0);
    assert_eq!(metric(&METRICS.layout_promoted_block) - block0, 1);
    assert_eq!(
        metric(&METRICS.pack_blocks_opened) - opened0,
        0,
        "no pack opened"
    );
    let (base, off, l) = fx.mapping(ino);
    assert_eq!((off, l), (0, len), "bk:0:len");
    let alloc = fx.alloc(0);
    assert_eq!(
        alloc.refcount(fx.offset_of(&base)),
        Some(1),
        "its own block"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the +ref is staged (FIND-PK-2)"
    );
    assert_eq!(fx.read(ino, len).await, pattern(80, len));
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The allocator's co-writer release arms (driven directly)
// ---------------------------------------------------------------------------

/// §5.3's co-writer column of `release_pack_reference`: nonterminal → a
/// PRIVATE release (nothing ships, the word untouched); terminal + KNOWN →
/// the never-published abandon; terminal + UNKNOWN → abandon WITHOUT
/// recycle (the entry dropped, the offset NOT on this mount's free list,
/// `cowriter_unpublished_abandons + 1`); untracked → a counted no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_pack_reference_dispatches_on_the_cowriter_posture() {
    use squeezefs::block_allocator::{PackPublishOutcome, PackRelease};
    use squeezefs::fuse_client::{set_mount_posture, MountPosture};
    let _g = serial().await;
    let _l = LeverGuard;
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "cowriter").await;
    let alloc = fx.alloc(0);
    let router = &fx.fs.router.backend_router;
    let be_id = fx.records[0].id.clone();

    // Minted as the authority (a co-writer without a lane cannot mint).
    let offset = alloc.allocate_block().await.unwrap();
    alloc.publish_block(offset);
    assert!(alloc.increment_refcount(offset), "a second reference");
    let key = router.persist_block_key(&be_id, offset);
    let idx = offset / alloc.chunk_size();

    set_mount_posture(MountPosture::CoWriter);
    // Nonterminal: private release, no ship, the word stays stable.
    let v = alloc
        .release_pack_reference(router, &key, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::Nonterminal);
    assert_eq!(alloc.refcount(offset), Some(1));
    assert!(alloc.fill_incarnation(offset).is_some());
    // Terminal + UNKNOWN: abandon without recycle.
    let abandons0 = metric(&METRICS.cowriter_unpublished_abandons);
    let v = alloc
        .release_pack_reference(router, &key, PackPublishOutcome::Unknown)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::Terminal);
    assert_eq!(alloc.refcount(offset), None, "the private entry is dropped");
    assert!(
        !alloc.free_list_contains(idx),
        "NEVER recycled into this mount's free list"
    );
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_abandons) - abandons0,
        1
    );
    // Untracked: a counted no-op.
    let noops0 = metric(&METRICS.pack_release_untracked_noops);
    let v = alloc
        .release_pack_reference(router, &key, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::UntrackedNoop);
    assert_eq!(metric(&METRICS.pack_release_untracked_noops) - noops0, 1);
    set_mount_posture(MountPosture::Writer);

    // Terminal + KNOWN on a co-writer: the never-published abandon arm
    // (lane-less here ⇒ the leak-safe quiet abandon; a laned co-writer's
    // recycle is PK4's).
    let offset2 = alloc.allocate_block().await.unwrap();
    alloc.publish_block(offset2);
    let key2 = router.persist_block_key(&be_id, offset2);
    set_mount_posture(MountPosture::CoWriter);
    let abandons1 = metric(&METRICS.cowriter_unpublished_abandons);
    let v = alloc
        .release_pack_reference(router, &key2, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::Terminal);
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_abandons) - abandons1,
        1,
        "the never-published arm's quiet abandon (no lane to recycle into)"
    );
    assert!(
        !alloc.free_list_contains(offset2 / alloc.chunk_size()),
        "never recycled without a lane"
    );
    set_mount_posture(MountPosture::Writer);

    // The authority: the router ladder — nonterminal then terminal.
    let offset3 = alloc.allocate_block().await.unwrap();
    alloc.publish_block(offset3);
    assert!(alloc.increment_refcount(offset3));
    let key3 = router.persist_block_key(&be_id, offset3);
    let v = alloc
        .release_pack_reference(router, &key3, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::Nonterminal);
    assert_eq!(alloc.refcount(offset3), Some(1));
    let queued0 = metric(&METRICS.block_free_reclaim_queued);
    let v = alloc
        .release_pack_reference(router, &key3, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::Terminal);
    assert_eq!(
        metric(&METRICS.block_free_reclaim_queued) - queued0,
        1,
        "the ROUTER ladder"
    );
    router.reclaim_drain().await;
    assert!(alloc.free_list_contains(offset3 / alloc.chunk_size()));
    // Untracked on the authority: the counted no-op (logged ERROR).
    let noops1 = metric(&METRICS.pack_release_untracked_noops);
    let v = alloc
        .release_pack_reference(router, &key3, PackPublishOutcome::Known)
        .await
        .unwrap();
    assert_eq!(v, PackRelease::UntrackedNoop);
    assert_eq!(metric(&METRICS.pack_release_untracked_noops) - noops1, 1);
    fx.close().await;
}
