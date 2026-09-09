//! The **inline raise** — phase A of the small-file program the fsync-
//! promotion pricing decided (`.benchmarks/2026-09-09-fsync-promote-staged-ab.md`
//! §3–4: promoting a small staged file into a WHOLE 4 MiB striped block is
//! a 64× space law; the owner's answer is "what does a small file promote
//! INTO" — the inline layout, whose ceiling was a free 4 KiB constant).
//!
//! The inline ceiling is now DERIVED from the KV geometry
//! (`routing::derived_inline_max_bytes`: the volume's xattr value cap
//! `min(64 KiB, node_size/4)` minus the layout wire's framing headroom —
//! the largest payload whose `layout` record still fits ONE KV value; 60 KiB
//! at the shipped 256 KiB node), with the explicit override
//! `SQUEEZEFS_INLINE_MAX_BYTES` (4096 = the shipped ceiling, the A/B
//! control). A file up to the ceiling is INLINE — its bytes ride the
//! layout commit, visible to every client of the set at that commit, no
//! block and no promotion step — and `promote_staged_file` dispatches on
//! size: a staged file at or under the ceiling promotes INTO INLINE (no
//! block allocated), a larger one takes the block path exactly as before.
//!
//! Contracts (red-first, live mounts):
//! (a) a 16 KiB file written + fsync'd is inline (`layout_inline_writes`
//!     moves, `nvme_staged_write_file_count` stays 0) and a LIVE read-only
//!     mount of the same set — the other client, beside the writer — reads
//!     it byte-exact with no promotion having happened. RED today: staged,
//!     the reader serves zeros.
//! (b) `SQUEEZEFS_INLINE_MAX_BYTES=4096`: the same file is staged (the
//!     shipped behavior, pinned as the A/B control) and the stats inode
//!     publishes the ceiling in force.
//! (c) growth: the inline file appended past the ceiling becomes staged,
//!     appended past the block becomes striped — durably (contents intact
//!     across a kill-9 remount elsewhere, the C8 oracle's drift 0).
//! (d) the dismount promotion of N small staged files (staged under the
//!     4 KiB control, recovered by a default-ceiling mount, then cleanly
//!     unmounted) allocates ZERO blocks and counts them inline; the next
//!     mount reads them.
//! (e) the fsync lever on + small staged files → the inline dispatch: no
//!     block, no `StorageFull` possible, `fsync_promote_failures` 0.
//! (f) `SQUEEZEFS_INLINE_MAX_BYTES` above the KV cap refuses the process
//!     at startup naming the range.
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate.

use squeezefs_testkit::{mount_supported, site};
use std::io::{Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const CEILING_KNOB: &str = "SQUEEZEFS_INLINE_MAX_BYTES";
const LEVER_KNOB: &str = "SQUEEZEFS_FSYNC_PROMOTE_STAGED";
/// The shipped inline ceiling — the A/B control the override reaches.
const SHIPPED_CEILING: usize = 4096;
/// The Linux `XATTR_SIZE_MAX` = the KV record-value cap's ceiling; the
/// registered range's top is this minus the layout framing headroom.
const KV_VALUE_CAP_CEILING: usize = 65_536;
const LAYOUT_INLINE_HEADROOM: usize = 4096;
/// The small file every contract writes first: above the shipped 4 KiB,
/// under the derived ceiling at every admissible node size ≥ 128 KiB
/// (12 KiB at the 64 KiB floor would make it staged — the format default
/// is 256 KiB).
const SMALL: usize = 16 * 1024;
/// The default format block size (the striped-transition boundary).
const BLOCK: u64 = 4 * 1024 * 1024;
/// Files per dismount/fsync contract.
const FILES: usize = 40;
const SIZES_KIB: [usize; 3] = [8, 16, 32];
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

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
    format!("small_{idx:04}.bin")
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_inline_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

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

fn is_mounted(mnt: &Path) -> bool {
    let want = mnt.to_string_lossy();
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| {
            t.lines()
                .any(|l| l.split(' ').nth(4).is_some_and(|p| p == want))
        })
        .unwrap_or(false)
}

/// Spawn the daemon (zc OFF — the fstests runner's posture; a modest ring
/// far from its high-water mark so pool pressure never promotes) with
/// `envs` set and the two knobs this suite drives otherwise UNSET.
fn spawn_mount(
    meta: &Path,
    mnt: &Path,
    log: &Path,
    envs: &[(&str, &str)],
    read_only: bool,
) -> Mount {
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
        .env_remove(CEILING_KNOB)
        .env_remove(LEVER_KNOB)
        .env_remove("SQUEEZEFS_BLOCK_REFS_VERIFY")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf));
    if read_only {
        cmd.arg("--read-only");
    }
    for (k, v) in envs {
        cmd.env(k, v);
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

/// Bytes the set has allocated to striped blocks: `statvfs` used, which
/// the daemon serves from the allocators (inline payloads live in the
/// metadata volume and count nothing here).
fn allocated_bytes(mnt: &Path) -> u64 {
    let c = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()).expect("path");
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and an out-struct of the right type.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs({}) failed", mnt.display());
    (st.f_blocks - st.f_bfree) * st.f_frsize
}

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

fn write_and_fsync(path: &Path, data: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(data)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
}

/// Poll a reader's view of `path` until it matches `want` (a reader
/// follows the writer's checkpoints on a derived cadence; the bound is
/// its own published staleness bound plus margin). Returns the bytes
/// read last on a mismatch.
fn wait_reader_matches(reader: &Path, path: &Path, want: &[u8]) -> Result<(), Vec<u8>> {
    let bound_ms = stats_json(reader)["metrics"]["reader_staleness_bound_ms"]
        .as_u64()
        .unwrap_or(2_000);
    let deadline = Instant::now() + Duration::from_millis(bound_ms * 3) + Duration::from_secs(10);
    let mut last = Vec::new();
    loop {
        if let Ok(got) = std::fs::read(path) {
            if got == want {
                return Ok(());
            }
            last = got;
        }
        if Instant::now() > deadline {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Write `FILES` small files under the SHIPPED ceiling (so they are
/// staged), then SIGKILL — the population survives in the mount point's
/// staging slot for the next mount at the same point to recover. Returns
/// the paths (relative names are the same on every mount point).
fn stage_small_files_under_shipped_ceiling(meta: &Path, mnt: &Path, log: &Path) {
    let mut a = spawn_mount(meta, mnt, log, &[(CEILING_KNOB, "4096")], false);
    for idx in 0..FILES {
        let path = mnt.join(file_name(idx));
        std::fs::File::create(&path)
            .unwrap_or_else(|e| panic!("create {}: {e}", path.display()))
            .write_all(&pattern(idx, file_len(idx)))
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    {
        let root = std::fs::File::open(mnt).expect("open mount root");
        use std::os::fd::AsRawFd;
        // SAFETY: syncfs on a live fd; the return is checked.
        let rc = unsafe { libc::syncfs(root.as_raw_fd()) };
        assert_eq!(rc, 0, "syncfs failed");
    }
    // The layouts must be DURABLE before the kill (release persists them
    // in the background): fsync each file — under the 4 KiB control the
    // fsync syncs the ring shard and commits the staged layout, nothing
    // more.
    for idx in 0..FILES {
        std::fs::File::open(mnt.join(file_name(idx)))
            .expect("open")
            .sync_all()
            .expect("fsync");
    }
    assert_eq!(
        staged_count(&stats_json(mnt)),
        FILES as u64,
        "fixture premise: under the shipped ceiling the population is staged"
    );
    a.kill9();
}

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

// ---------------------------------------------------------------------------
// (a) the raise: a small fsync'd file is inline and visible to a live reader
// ---------------------------------------------------------------------------

#[test]
fn a_small_fsynced_file_is_inline_and_a_live_reader_mount_sees_it() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("raise");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut writer = spawn_mount(&meta, &mnt, &log, &[], false);

    let before = stats_json(&mnt);
    let ceiling = before["inline_max_bytes"]
        .as_u64()
        .expect("the stats inode publishes inline_max_bytes") as usize;
    assert!(
        ceiling >= SMALL && ceiling > SHIPPED_CEILING,
        "the derived ceiling must admit a {SMALL}-byte file (got {ceiling})"
    );
    assert!(
        ceiling <= KV_VALUE_CAP_CEILING - LAYOUT_INLINE_HEADROOM,
        "the ceiling must leave the layout framing inside one KV value (got {ceiling})"
    );

    let want = pattern(1, SMALL);
    write_and_fsync(&mnt.join("small.bin"), &want);
    let after = stats_json(&mnt);
    assert_eq!(
        staged_count(&after),
        0,
        "a {SMALL}-byte file must be INLINE, not staged (ceiling {ceiling}); log: {}",
        log.display()
    );
    assert!(
        metric(&after, "layout_inline_writes").unwrap_or(0)
            > metric(&before, "layout_inline_writes").unwrap_or(0),
        "layout_inline_writes must move for the small file"
    );
    assert_eq!(
        metric(&after, "layout_staged_writes").unwrap_or(0),
        metric(&before, "layout_staged_writes").unwrap_or(0),
        "no staged write for a file under the ceiling"
    );

    // The other client: a LIVE reader beside the writer (no unmount, no
    // promotion) — the inline bytes ride the layout the fsync committed.
    let ro = base.join("ro");
    let ro_log = base.join("ro.log");
    let mut reader = spawn_mount(&meta, &ro, &ro_log, &[], true);
    if let Err(got) = wait_reader_matches(&ro, &ro.join("small.bin"), &want) {
        let zeros = got.len() == want.len() && got.iter().all(|&b| b == 0);
        panic!(
            "the reader mount never saw the small file's bytes (last read {} B{}) — the file is \
             not inline, the reader has only the writer's staging root to miss; reader log: {}",
            got.len(),
            if zeros { ", all ZEROS" } else { "" },
            ro_log.display()
        );
    }
    assert_eq!(
        metric(&stats_json(&ro), "staged_payload_lost_reads").unwrap_or(u64::MAX),
        0,
        "the reader must never have degraded a read to zeros; log: {}",
        ro_log.display()
    );

    reader.umount_clean();
    writer.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// (b) the A/B control: the override at the shipped ceiling
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_ceiling_override_keeps_the_small_file_staged() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("control");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut writer = spawn_mount(&meta, &mnt, &log, &[(CEILING_KNOB, "4096")], false);

    let stats = stats_json(&mnt);
    assert_eq!(
        stats["inline_max_bytes"].as_u64(),
        Some(SHIPPED_CEILING as u64),
        "the override must be published verbatim as the ceiling in force"
    );
    write_and_fsync(&mnt.join("small.bin"), &pattern(2, SMALL));
    let after = stats_json(&mnt);
    assert_eq!(
        staged_count(&after),
        1,
        "under the shipped ceiling a {SMALL}-byte file is STAGED (the A/B control); log: {}",
        log.display()
    );
    // And a file at the ceiling itself is still inline.
    write_and_fsync(&mnt.join("tiny.bin"), &pattern(3, SHIPPED_CEILING));
    assert_eq!(
        staged_count(&stats_json(&mnt)),
        1,
        "a file exactly at the ceiling is inline"
    );

    writer.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// (c) growth past the ceiling promotes durably
// ---------------------------------------------------------------------------

#[test]
fn growth_past_the_inline_ceiling_promotes_durably() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("growth");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut writer = spawn_mount(&meta, &mnt, &log, &[], false);
    let ceiling = stats_json(&mnt)["inline_max_bytes"]
        .as_u64()
        .expect("inline_max_bytes") as usize;

    // Inline first.
    let head = pattern(4, SMALL);
    let path = mnt.join("grow.bin");
    write_and_fsync(&path, &head);
    assert_eq!(staged_count(&stats_json(&mnt)), 0, "premise: inline");

    // Past the ceiling, under the block: the staged layout (the ring).
    let mid_off = SMALL;
    let mid_len = ceiling; // end = SMALL + ceiling > ceiling
    let mid = pattern(5, mid_len);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        f.seek(SeekFrom::Start(mid_off as u64)).expect("seek");
        f.write_all(&mid).expect("append past the ceiling");
        f.sync_all().expect("fsync");
    }
    let s = stats_json(&mnt);
    assert_eq!(
        staged_count(&s),
        1,
        "past the ceiling (but under the block) the file is staged; log: {}",
        log.display()
    );

    // Past the block: striped — the P0 durable promotion (block I/O
    // before the type flip).
    let tail_off = BLOCK - 8 * 1024;
    let tail = pattern(6, 16 * 1024);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        f.seek(SeekFrom::Start(tail_off)).expect("seek");
        f.write_all(&tail).expect("write across the block boundary");
        f.sync_all().expect("fsync");
    }
    let s2 = stats_json(&mnt);
    assert_eq!(
        staged_count(&s2),
        0,
        "the striped promotion releases the staged entry; log: {}",
        log.display()
    );
    assert!(
        metric(&s2, "layout_striped_writes").unwrap_or(0)
            > metric(&s, "layout_striped_writes").unwrap_or(0),
        "layout_striped_writes must move"
    );

    // The expected image.
    let total = tail_off as usize + tail.len();
    let mut want = vec![0u8; total];
    want[..head.len()].copy_from_slice(&head);
    want[mid_off..mid_off + mid.len()].copy_from_slice(&mid);
    want[tail_off as usize..].copy_from_slice(&tail);
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        want,
        "writer's own view"
    );

    // Kill-9 (no teardown), remount ELSEWHERE with the C8 oracle armed.
    writer.kill9();
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut m2 = spawn_mount(
        &meta,
        &mnt2,
        &log2,
        &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")],
        false,
    );
    let got = std::fs::read(mnt2.join("grow.bin")).expect("read after remount");
    assert_eq!(
        got.len(),
        want.len(),
        "size intact across the kill-9 remount"
    );
    assert!(got == want, "contents intact across the kill-9 remount");
    let s3 = stats_json(&mnt2);
    assert_eq!(
        metric(&s3, "meta_kv_block_refs_drift").expect("meta_kv_block_refs_drift"),
        0,
        "the C8 oracle must find no drift after the growth promotions; log: {}",
        log2.display()
    );
    assert_eq!(
        metric(&s3, "staged_payload_lost_reads").unwrap_or(u64::MAX),
        0
    );

    m2.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// (d) the dismount promotion dispatches small staged files inline
// ---------------------------------------------------------------------------

#[test]
fn dismount_promotes_small_staged_files_inline_without_blocks() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("dismount");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    stage_small_files_under_shipped_ceiling(&meta, &mnt, &base.join("mountA.log"));

    // The default-ceiling mount at the SAME point recovers the population
    // staged, and its clean unmount promotes it.
    let log_b = base.join("mountB.log");
    let mut b = spawn_mount(&meta, &mnt, &log_b, &[], false);
    assert_eq!(
        staged_count(&stats_json(&mnt)),
        FILES as u64,
        "premise: the recovered population is staged; log: {}",
        log_b.display()
    );
    let allocated_before = allocated_bytes(&mnt);
    b.umount_clean();
    assert!(
        log_contains(&log_b, &format!("promoted {FILES} staged-layout file(s)")),
        "the dismount must promote all {FILES}; log: {}",
        log_b.display()
    );
    assert!(
        log_contains(&log_b, &format!("{FILES} inline")),
        "the dismount report must count the {FILES} inline promotions; log: {}",
        log_b.display()
    );

    // The next mount: ZERO blocks were allocated, every file reads back.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut c = spawn_mount(
        &meta,
        &mnt2,
        &log2,
        &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")],
        false,
    );
    let allocated_after = allocated_bytes(&mnt2);
    assert_eq!(
        allocated_after,
        allocated_before,
        "promoting {FILES} small staged files inline must allocate NO blocks \
         (+{} bytes of blocks = the whole-block promotion, the 64× space law)",
        allocated_after.saturating_sub(allocated_before)
    );
    let (mismatches, zero_reads) = verify_files(&mnt2);
    assert_eq!(
        mismatches,
        0,
        "{mismatches} of {FILES} files read wrong from the next mount ({zero_reads} as zeros); \
         log: {}",
        log2.display()
    );
    let s = stats_json(&mnt2);
    assert_eq!(
        metric(&s, "staged_payload_lost_reads").unwrap_or(u64::MAX),
        0
    );
    assert_eq!(
        metric(&s, "meta_kv_block_refs_drift").expect("drift"),
        0,
        "C8 drift after the inline promotions; log: {}",
        log2.display()
    );

    c.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// (e) the fsync lever dispatches small staged files inline
// ---------------------------------------------------------------------------

#[test]
fn the_fsync_lever_dispatches_small_staged_files_inline() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("fsync");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    stage_small_files_under_shipped_ceiling(&meta, &mnt, &base.join("mountA.log"));

    let log_b = base.join("mountB.log");
    let mut b = spawn_mount(&meta, &mnt, &log_b, &[(LEVER_KNOB, "1")], false);
    assert_eq!(
        staged_count(&stats_json(&mnt)),
        FILES as u64,
        "premise: the recovered population is staged"
    );
    let allocated_before = allocated_bytes(&mnt);
    for idx in 0..FILES {
        std::fs::File::open(mnt.join(file_name(idx)))
            .expect("open")
            .sync_all()
            .expect("fsync");
    }
    let s = stats_json(&mnt);
    assert_eq!(
        staged_count(&s),
        0,
        "every fsync'd staged file must be promoted; log: {}",
        log_b.display()
    );
    assert_eq!(
        metric(&s, "fsync_promoted_files").expect("fsync_promoted_files"),
        FILES as u64
    );
    assert_eq!(
        metric(&s, "fsync_promoted_inline_files").expect("fsync_promoted_inline_files"),
        FILES as u64,
        "small files promote INTO INLINE"
    );
    assert_eq!(
        metric(&s, "fsync_promote_failures").expect("fsync_promote_failures"),
        0,
        "no StorageFull is possible on the inline dispatch; log: {}",
        log_b.display()
    );
    assert_eq!(
        allocated_bytes(&mnt),
        allocated_before,
        "the inline dispatch allocates no block"
    );

    // The other client after a crash of the promoter: the inline layouts
    // are what the fsync committed.
    b.kill9();
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut c = spawn_mount(&meta, &mnt2, &log2, &[], false);
    let (mismatches, zero_reads) = verify_files(&mnt2);
    assert_eq!(
        mismatches,
        0,
        "{mismatches} of {FILES} files read wrong from the next mount ({zero_reads} as zeros); \
         log: {}",
        log2.display()
    );
    assert_eq!(
        metric(&stats_json(&mnt2), "staged_payload_lost_reads").unwrap_or(u64::MAX),
        0
    );

    c.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// (f) the knob's range
// ---------------------------------------------------------------------------

/// Above the KV value cap (minus the framing headroom) the value cannot
/// be stored in one record — refused at startup, naming the range, before
/// anything is opened (the ONE parsing convention).
#[test]
fn an_inline_ceiling_above_the_kv_cap_refuses_the_process() {
    let too_big = (KV_VALUE_CAP_CEILING - LAYOUT_INLINE_HEADROOM + 1).to_string();
    let out = Command::new(bin())
        .arg("--version")
        .env(CEILING_KNOB, &too_big)
        .output()
        .expect("spawn squeezefs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "{CEILING_KNOB}={too_big} must refuse the process; stderr: {stderr}"
    );
    assert!(stderr.contains(CEILING_KNOB), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{}..={}",
            SHIPPED_CEILING,
            KV_VALUE_CAP_CEILING - LAYOUT_INLINE_HEADROOM
        )),
        "the refusal must name the admissible range: {stderr}"
    );
    // Below the shipped floor is refused too.
    let out = Command::new(bin())
        .arg("--version")
        .env(CEILING_KNOB, "512")
        .output()
        .expect("spawn squeezefs");
    assert!(!out.status.success(), "512 is below the shipped floor");
    // The top of the range and the control are accepted.
    for v in [
        SHIPPED_CEILING,
        KV_VALUE_CAP_CEILING - LAYOUT_INLINE_HEADROOM,
    ] {
        let out = Command::new(bin())
            .arg("--version")
            .env(CEILING_KNOB, v.to_string())
            .output()
            .expect("spawn squeezefs");
        assert!(out.status.success(), "{CEILING_KNOB}={v} must be accepted");
    }
}
