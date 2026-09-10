//! The **overlay-vs-growth** data-loss race (2026-09-10, P0 on the shipped
//! default write path).
//!
//! The tape: a buffered `4 MiB + 64 KiB` file (`write_all` → `sync_all`),
//! which the kernel writes back as FIVE kernel-split 1 MiB FUSE WRITEs
//! delivered concurrently and out of order, on a volume formatted WITH a
//! staging dir, read back ZEROS for its bytes `0x300000..0x400000` on the
//! remount while fsync had returned success — and the writer's log
//! carried exactly one line at the moment of the write:
//! `INVARIANT TRIPWIRE 'overlay_foreign_merge': a foreign un-marked Merge
//! reached an open-overlay index — a one-authority-screen escape
//! (contained: superseded)`. ≈ 1 in 80–100 runs
//! (`tests/packed_mapping_wire_tests.rs`'s anchor, batch gate `4fc4aa59`).
//!
//! The mechanism, verified against the code (not the promotion's own
//! publish — that is a whole-layout SAVE under the block-0 guard and never
//! reaches the `Merge` hook):
//!
//! 1. Segments A (0–1 MiB) … D (3–4 MiB) classify STAGED-within-block at
//!    the WRITE handler (`inode_write_lock_scope` → `MetaPrepOnly`: the
//!    inode guard DROPS before `DataRouter::write_file`); the tail segment
//!    E (4 MiB–4 MiB + 64 KiB) classifies as the staged→striped growth
//!    (`EntireOp`) and, inside `write_file` under `BLOCK_FLUSH_LOCKS(ino, 0)`,
//!    promotes the staged image to striped `{0: k0, 1: k1}`.
//! 2. A sibling C that classified staged BEFORE the flip reaches
//!    `write_file` AFTER it (parked on the block-0 guard, or simply late):
//!    the router's "layout flipped striped while we waited" arm DROPPED the
//!    guard and ran `write_striped` — a guard-LESS whole-block RMW that
//!    seeded from k0 and published `Merge{0: k0'}` holding no
//!    `BLOCK_FLUSH_LOCKS`.
//! 3. A sibling D that classified AFTER the flip rode `write_file_staged`
//!    → the device overlay's OVERWRITE arm: block 0 is mapped (k0), the
//!    1 MiB aligned segment is above the finding-47 floor, so it installed
//!    an overlay record (old_binding = k0) under the guard and ACKed its
//!    bytes into a fresh destination.
//! 4. C's un-marked `Merge` reached `overlay_screen_merge`, which read it as
//!    a FOREIGN merge (KD-B4-11 exempts only the settle's own publish),
//!    fired the tripwire and "contained" it by superseding D's record — the
//!    `Superseded` teardown frees D's destination: acked custody dropped
//!    without a D0 fence (the FIND-M11-A never-lossy law broken), and the
//!    durable map names k0' = [A, B, C, zeros]. The remount's map was
//!    CONSISTENT (drift 0, no rebinds) — the bytes were simply gone.
//!
//! The fix is structural, not an exemption: the router owns no striped
//! write route any more. `DataRouter::write_file` answers
//! `WriteFileOutcome::LayoutStriped` (nothing written) whenever the layout
//! is striped, and the handler re-dispatches the same payload through the
//! ONE striped write path (`write_file_staged`) — under the block guard,
//! where an open overlay record on the block is JOINED (the segment's bytes
//! land in the same destination; the settle seeds the gaps from k0) or
//! settled first, and the coverage union accumulates the rest. Exempting
//! `write_striped`'s merge instead would have lost C's bytes to D's later
//! feed: two unserialized whole-block RMWs of one block cannot both win.
//!
//! Determinism: `SQUEEZEFS_TEST_ROUTER_DISPATCH_STALL_MS` parks a
//! staged-within-block router-route write between its handler
//! classification and `DataRouter::write_file` — the exact
//! stale-classification window — and logs the park, so the test drives the
//! tape's order with `sync_file_range`-initiated writebacks: C parks (the
//! log line is the sequencing point), E promotes, D overlays, C resumes.
//! The writer-side oracle reads through O_DIRECT: under the FUSE writeback
//! cache a buffered read of just-written pages is the kernel's page cache,
//! not the daemon's map.
//!
//! Contract: after fsync, the file is byte-exact on the writer and on a
//! remount at a second mount point with the C8 oracle armed;
//! `invariant_tripwires == 0`, `overlay_superseded_by_merge == 0`,
//! `meta_kv_block_refs_drift == 0`; and the row is VALID only if D's
//! overlay install engaged inside C's park (else it panics as an invalid
//! row rather than passing vacuously).
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate
//! (`tests/run_require_mount_gate.sh`).

use squeezefs_testkit::{mount_supported, site};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;
/// The tape's file: one whole block plus a 64 KiB tail in block 1.
const FILE_LEN: usize = 4 * MIB + 64 * 1024;
/// The kernel-split segment (`max_write` = 1 MiB on the zc-off posture).
const SEG: usize = MIB;
/// The park: E's promotion (two block DMAs + one meta commit) and D's
/// overlay store must both land inside it; the row is invalid otherwise.
const DISPATCH_STALL_MS: u64 = 4000;
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
const TRIPWIRE_LINE: &str = "INVARIANT TRIPWIRE 'overlay_foreign_merge'";
/// The seam's park line (`src/fuse_client.rs`) — the sequencing point.
const PARK_LINE: &str = "TEST SEAM: router-dispatch stall parked at offset";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic content (a rolling byte pattern salted by the index, so
/// a zeros read or a cross-segment mix-up is caught).
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

/// Scratch under the system temp dir, CANONICALIZED before anything is
/// mounted under it (`/proc/self/mountinfo` compares paths verbatim).
fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_ovgrowth_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + one data volume WITH a staging dir (the staged
/// layout exists only on such a volume — the tape's shape).
fn format_volume(base: &Path, staging: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * 1024 * 1024)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(1024 * 1024 * 1024)
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
    /// `squeezefs umount`, then wait for the daemon's clean exit (a
    /// teardown panic is a bug an `ok` must not absorb).
    fn umount(&mut self) {
        let out = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .output()
            .expect("run squeezefs umount");
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 3;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                break status;
            }
            if Instant::now() > deadline {
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
        assert!(
            status.success(),
            "daemon exited {status} on unmount (teardown crash); umount said:\n{}{}\nlog:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
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

/// Spawn the real daemon on the tape's posture (zc OFF, the one-page
/// inline ceiling pinned, a staging ring) and wait for the stats inode.
/// `extra_env` rides on top.
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

fn metric(mnt: &Path, key: &str) -> u64 {
    stats_json(mnt)["metrics"][key]
        .as_u64()
        .unwrap_or_else(|| panic!("metrics.{key} must export on the stats inode"))
}

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

/// Wait for the seam to log the park of the write at `off` (the write has
/// classified, dropped its inode guard and is parked ahead of the router).
fn wait_parked(log: &Path, off: usize, bound: Duration) {
    let needle = format!("{PARK_LINE} {off} ");
    let deadline = Instant::now() + bound;
    while !log_contains(log, &needle) {
        assert!(
            Instant::now() < deadline,
            "the write at offset {off} never reached the dispatch-stall seam within {bound:?} \
             (is the writeback in flight? log: {})",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The writer-side oracle: an O_DIRECT read bypasses the kernel page cache
/// (which holds the just-written pages under the writeback cache) and asks
/// the daemon's map for every byte.
fn odirect_read(path: &Path, len: usize) -> Vec<u8> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} O_DIRECT: {e}", path.display()));
    let mut out = vec![0u8; len];
    f.read_exact_at(&mut out, 0)
        .unwrap_or_else(|e| panic!("O_DIRECT read {}: {e}", path.display()));
    out
}

/// Buffered `pwrite` of one segment, then `sync_file_range(WRITE)` on
/// exactly that range: the kernel INITIATES the segment's writeback — one
/// `FUSE_WRITE_CACHE` WRITE of `SEG` bytes, the tape's vehicle — and
/// returns without waiting for it. `wait` adds `WAIT_AFTER`, returning
/// only once that WRITE completed. (Direct I/O cannot drive this
/// interleave: an EXTENDING O_DIRECT write takes `i_rwsem` exclusive in
/// the kernel, serializing the segments before the daemon sees them.)
fn write_segment(f: &std::fs::File, off: usize, bytes: &[u8], wait: bool) {
    f.write_all_at(bytes, off as u64)
        .unwrap_or_else(|e| panic!("pwrite {} @ {off}: {e}", bytes.len()));
    let mut flags = libc::SYNC_FILE_RANGE_WRITE;
    if wait {
        flags |= libc::SYNC_FILE_RANGE_WAIT_AFTER;
    }
    // SAFETY: a live fd, an in-file range, checked return.
    let rc = unsafe { libc::sync_file_range(f.as_raw_fd(), off as i64, bytes.len() as i64, flags) };
    assert_eq!(
        rc,
        0,
        "sync_file_range @ {off}: {}",
        std::io::Error::last_os_error()
    );
}

/// The first and last mismatching offsets, and whether every mismatched
/// byte read as zero — the tape's signature (`[0x300000, 0x3fffff]`, all
/// zeros).
fn describe_mismatch(got: &[u8], want: &[u8]) -> String {
    if got.len() != want.len() {
        return format!("length {} != {}", got.len(), want.len());
    }
    let mut first = None;
    let mut last = 0usize;
    let mut all_zero = true;
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        if g != w {
            first.get_or_insert(i);
            last = i;
            if *g != 0 {
                all_zero = false;
            }
        }
    }
    match first {
        None => "byte-exact".to_string(),
        Some(f) => format!(
            "mismatch [{f:#x}, {last:#x}] ({} bytes){}",
            last - f + 1,
            if all_zero { ", all ZEROS" } else { "" }
        ),
    }
}

/// The race, driven deterministically through the dispatch-stall seam.
#[test]
fn a_segment_redirected_by_a_sibling_promotion_never_displaces_the_overlays_acked_bytes() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("race");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let stall = DISPATCH_STALL_MS.to_string();
    let mut mount = spawn_mount(
        &meta,
        &mnt,
        &log,
        &[("SQUEEZEFS_TEST_ROUTER_DISPATCH_STALL_MS", stall.as_str())],
    );

    let want = pattern(usize::MAX, FILE_LEN);
    let path = mnt.join("anchor_striped.bin");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create the anchor");

    // A, B: the staged image grows to 2 MiB (A is the inline→staged
    // promotion under the held inode guard; B pays one park — the seam
    // stalls every staged-within-block router write).
    write_segment(&f, 0, &want[..SEG], true);
    write_segment(&f, SEG, &want[SEG..2 * SEG], true);

    // C classifies staged-within-block, drops its inode guard and PARKS
    // in the seam. Its writeback is in flight from here; the park line is
    // the sequencing point (E's handler must not win the inode guard
    // before C has classified — that is the healthy order).
    let installs_before = metric(&mnt, "overlay_overwrite_installs");
    write_segment(&f, 2 * SEG, &want[2 * SEG..3 * SEG], false);
    wait_parked(&log, 2 * SEG, Duration::from_secs(10));
    let parked_at = Instant::now();

    // E: the growth segment — staged→striped promotion of the 2 MiB image
    // under the block-0 guard: {0: k0 = [A, B, 0, 0], 1: k1 = [E]}.
    write_segment(&f, 4 * SEG, &want[4 * SEG..], true);
    // D: classified AFTER the flip → the striped path → block 0 is mapped
    // → the device overlay's overwrite arm installs a record on block 0
    // (old_binding = k0) and lands D's bytes in a fresh destination.
    write_segment(&f, 3 * SEG, &want[3 * SEG..4 * SEG], true);
    let d_done = parked_at.elapsed();
    let installs_after = metric(&mnt, "overlay_overwrite_installs");

    // Row validity (charter rule 4): the interleave must have been REACHED
    // — E and D landed inside C's park and D's overlay engaged. A slow
    // venue makes the row INVALID, never a vacuous green.
    let park = Duration::from_millis(DISPATCH_STALL_MS);
    assert!(
        d_done < park.mul_f64(0.8),
        "INVALID ROW: E's promotion + D's overlay store took {d_done:?}, not inside C's \
         {park:?} park — the interleave was not reached (venue too slow; raise \
         DISPATCH_STALL_MS)"
    );
    assert!(
        installs_after > installs_before,
        "INVALID ROW: D never installed an overwrite overlay on the promoted block 0 \
         (overlay_overwrite_installs {installs_before} → {installs_after}) — the \
         race's overlay arm did not engage; log: {}",
        log.display()
    );

    // C resumes: the router finds the layout striped. Pre-fix it ran the
    // guard-less `write_striped` RMW whose `Merge{0}` superseded D's record.
    f.sync_all().expect("fsync the anchor");

    // 1. The writer's own view after fsync, asked of the daemon (O_DIRECT).
    let got = odirect_read(&path, FILE_LEN);
    let tripwires = metric(&mnt, "invariant_tripwires");
    let superseded = metric(&mnt, "overlay_superseded_by_merge");
    assert!(
        got == want,
        "the anchor read wrong on the WRITER after fsync: {} (invariant_tripwires = \
         {tripwires}, overlay_superseded_by_merge = {superseded}); log: {}",
        describe_mismatch(&got, &want),
        log.display()
    );
    assert_eq!(
        tripwires,
        0,
        "invariant_tripwires must stay 0 — a same-file segment's publish is never a \
         foreign merge; log: {}",
        log.display()
    );
    assert_eq!(
        superseded,
        0,
        "overlay_superseded_by_merge must stay 0 — acked overlay custody was dropped \
         without a D0 fence (never-lossy, FIND-M11-A); log: {}",
        log.display()
    );
    assert!(
        !log_contains(&log, TRIPWIRE_LINE),
        "the writer log carries \"{TRIPWIRE_LINE}\"; log: {}",
        log.display()
    );
    // The record reached a PUBLISHING terminal state (the epoch feed or
    // the durable-merge degenerate) — the composition, not a supersede.
    let settled = metric(&mnt, "overlay_epoch_feeds") + metric(&mnt, "overlay_publishes");
    assert!(
        settled >= 1,
        "D's overlay record never settled through a publish (overlay_epoch_feeds + \
         overlay_publishes = {settled}); log: {}",
        log.display()
    );
    drop(f);
    mount.umount();

    // 2. Another client's view: a remount at a different mount point with
    // the C8 oracle armed — the durable map, the durable bytes, the ledger.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    let got2 = std::fs::read(mnt2.join("anchor_striped.bin")).expect("read the anchor on mnt2");
    assert!(
        got2 == want,
        "the anchor read wrong on the REMOUNT: {} (the tape: [0x300000, 0x3fffff] all \
         zeros — the overlay segment's acked bytes never reached the durable map); \
         writer log: {}, reader log: {}",
        describe_mismatch(&got2, &want),
        log.display(),
        log2.display()
    );
    assert_eq!(
        metric(&mnt2, "meta_kv_block_refs_drift"),
        0,
        "meta_kv_block_refs_drift must be 0 on the remount; log: {}",
        log2.display()
    );
    assert_eq!(
        metric(&mnt2, "staged_payload_lost_reads"),
        0,
        "the reader degraded reads to zeros (staged_payload_lost_reads); log: {}",
        log2.display()
    );
    mount2.umount();
    let _ = std::fs::remove_dir_all(&base);
}
