//! Small-file PACKING — PR PK3's contracts: the tenant ops and the mover
//! interplay (`docs/design-small-file-packing.md` §5.7, §5.8 note on the
//! two legal share classes, §5.11; PR plan PK3).
//!
//! A packed tenant is one file's stored image in a slot of a shared block,
//! named by the size-carrying mapping `bk:off:len` and owned through ONE
//! durable C8 record per reference. PK2 landed the packer; this suite pins
//! what the ordinary tenant operations do to that block:
//!
//!  1. **Passthrough truncate-shrink is a MAPPING CLIP** — `base:off:len'`
//!     with ZERO data-plane writes (the device write seam stays untouched);
//!     the survivor reads byte-exact at the clipped size; the durable
//!     POPULATION and the RAM refcount are unchanged (a same-key
//!     Delete+Put in the clip's tx — the old mapping is NOT freed); the C8
//!     oracle is clean on a NON-empty ledger (one striped anchor first).
//!  2. **Transformed truncate-shrink lands the clipped image as a NEW
//!     TENANT** through the packed arm (`layout_promoted_packed + 1`), the
//!     old reference partially freed (`pack_partial_frees + 1`), byte-exact.
//!  3. **Truncate-up is size-only**: no promotion, no pack movement, the
//!     implicit-zero tail reads correct.
//!  4. **`move_one` defers an OPEN pack block** (the pack-open ledger /
//!     `inflight_contains(base)`), a drain victim's open pack is sealed by
//!     the deferring pass, and a SEALED pack block moves as a unit with
//!     every tenant's `:off:len` preserved and byte-exact.
//!  5. **A spill escalation under `StorageFull` PACKS** — the staged-clone
//!     spill and the rider-fold spill both dispatch through the packed
//!     arm; a lever-OFF mount takes the block arm byte-identically.
//!  6. **A clone of a PROMOTED packed tenant shares the window**: two inos,
//!     identical `:off:len`, refcount 2, one slot, both byte-exact; delete
//!     one → nonterminal, the survivor byte-exact.
//!  7. **Clone-then-clip and clip-then-clone** of a passthrough tenant yield
//!     two NESTED same-`off` windows, both byte-exact, refcount 2, one slot,
//!     oracle clean.
//!
//! Two venues, the PK2 suite's shape: the mount-class face of contract 1
//! (+ 3's mount face) runs the real daemon on an unprivileged file-backed
//! sandbox and self-skips through the testkit where a mount is not
//! possible; everything else runs the in-process `SqueezefsFilesystem`
//! fixture with the job fabric (the drain of contract 4).

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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The clipped content plus an implicit-zero tail up to `size`.
fn clipped_then_zero(idx: usize, kept: usize, size: usize) -> Vec<u8> {
    let mut want = pattern(idx, kept);
    want.resize(size, 0);
    want
}

// ===========================================================================
// The mount-class venue
// ===========================================================================

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_pk3_{tag}_{}", std::process::id()));
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
/// a short fsck settle). `extra_env` rides on top — the packing lever IS
/// the seam.
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

fn write_fsync(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
}

/// `ftruncate(2)` through the kernel — the SETATTR(size) the daemon's
/// `truncate_layout` answers.
fn truncate_path(path: &Path, size: u64) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} for truncate: {e}", path.display()))
        .set_len(size)
        .unwrap_or_else(|e| panic!("truncate {} to {size}: {e}", path.display()));
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
// Contract 1 (mount face) + contract 3's mount face: the passthrough clip
// through the kernel, the oracle on the remount
// ---------------------------------------------------------------------------

/// Six fsync-promoted 16 KiB tenants share one pack block; `ftruncate(2)`
/// clips three of them (10 000 / 8 192 / 5 000 B): each clip re-describes
/// the SAME reference — `pack_mapping_clips + 3`, no durable re-encode
/// (`staged_truncate_durable_clips` flat), no unaligned device DMA, no
/// tenant release (`pack_partial_frees` flat), no promotion, no new block;
/// the survivors read byte-exact at their new sizes. A truncate-UP of a
/// clipped tenant is size-only and its tail reads zeros. The online fsck
/// reports nothing; a remount with the C8 oracle armed reads drift 0 with
/// the anchor's blocks + SIX records (the population unchanged) and every
/// file byte-exact from the shared backend.
#[test]
fn passthrough_truncate_shrink_of_packed_tenants_clips_the_mapping_with_no_data_io() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("clip");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, &[(LEVER, "1"), (FSYNC_LEVER, "1")]);
    assert_eq!(stats_json(&mnt)["small_file_packing"], true);

    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    const N: usize = 6;
    let len = 16 * KIB;
    for i in 0..N {
        write_fsync(&mnt.join(format!("t{i}.bin")), &pattern(i, len));
    }
    assert_eq!(
        stat_u64(&mnt, "layout_promoted_packed"),
        N as u64,
        "premise: every fsync packed its file; log: {}",
        log.display()
    );
    let used0 = used_chunks(&mnt);
    assert_eq!(used0, ANCHOR_BLOCKS + 1, "the anchor + one pack block");
    let clips0 = stat_u64(&mnt, "pack_mapping_clips");
    let durable0 = stat_u64(&mnt, "staged_truncate_durable_clips");
    let fallbacks0 = stat_u64(&mnt, "nvme_unaligned_write_fallbacks");
    let partial0 = stat_u64(&mnt, "pack_partial_frees");
    let terminal0 = stat_u64(&mnt, "pack_terminal_frees");

    let clips: [(usize, usize); 3] = [(0, 10_000), (1, 8_192), (2, 5_000)];
    for &(i, new_len) in &clips {
        truncate_path(&mnt.join(format!("t{i}.bin")), new_len as u64);
    }
    for &(i, new_len) in &clips {
        let got = std::fs::read(mnt.join(format!("t{i}.bin"))).unwrap();
        assert_eq!(got.len(), new_len, "t{i}: the clipped size");
        assert_eq!(
            got,
            pattern(i, new_len),
            "t{i}: byte-exact at the clipped size"
        );
    }
    for i in 3..N {
        assert_eq!(
            std::fs::read(mnt.join(format!("t{i}.bin"))).unwrap(),
            pattern(i, len),
            "sibling t{i} untouched"
        );
    }
    assert_eq!(
        stat_u64(&mnt, "pack_mapping_clips"),
        clips0 + 3,
        "three passthrough clips re-described their mapping; log: {}",
        log.display()
    );
    assert_eq!(
        stat_u64(&mnt, "staged_truncate_durable_clips"),
        durable0,
        "no durable re-encode: the clip is a MAPPING clip"
    );
    assert_eq!(
        stat_u64(&mnt, "nvme_unaligned_write_fallbacks"),
        fallbacks0,
        "no clipped image was DMA'd (a 10 000 / 5 000 B image would take the unaligned arm)"
    );
    assert_eq!(
        stat_u64(&mnt, "pack_partial_frees"),
        partial0,
        "no tenant reference moved (the old mapping is NOT freed)"
    );
    assert_eq!(stat_u64(&mnt, "pack_terminal_frees"), terminal0);
    assert_eq!(
        stat_u64(&mnt, "layout_promoted_packed"),
        N as u64,
        "a clip is not a promotion"
    );
    assert_eq!(used_chunks(&mnt), used0, "no block allocated");

    // Contract 3's mount face: truncate-UP of a clipped tenant is size-only.
    truncate_path(&mnt.join("t0.bin"), len as u64);
    assert_eq!(
        std::fs::read(mnt.join("t0.bin")).unwrap(),
        clipped_then_zero(0, 10_000, len),
        "t0 re-extended: the kept prefix, then an implicit-zero tail"
    );
    assert_eq!(stat_u64(&mnt, "layout_promoted_packed"), N as u64);
    assert_eq!(stat_u64(&mnt, "pack_mapping_clips"), clips0 + 3);
    assert_eq!(used_chunks(&mnt), used0);

    let report = online_fsck(&mnt);
    assert_eq!(
        report["counters"]["findings"], 0,
        "online fsck after the clips: {report}"
    );
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    mount.umount_timed();

    // Another client, the C8 oracle armed: the population is unchanged.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(
        stat_u64(&mnt2, "meta_kv_block_refs_drift"),
        0,
        "the C8 oracle after three mapping clips; log: {}",
        log2.display()
    );
    assert_eq!(
        stat_u64(&mnt2, "meta_kv_block_refs_recovered"),
        ANCHOR_BLOCKS + N as u64,
        "one durable reference per tenant survives its clip (a same-key Delete+Put)"
    );
    assert_eq!(used_chunks(&mnt2), ANCHOR_BLOCKS + 1);
    assert_eq!(
        std::fs::read(mnt2.join("t0.bin")).unwrap(),
        clipped_then_zero(0, 10_000, len)
    );
    for &(i, new_len) in &clips[1..] {
        assert_eq!(
            std::fs::read(mnt2.join(format!("t{i}.bin"))).unwrap(),
            pattern(i, new_len),
            "t{i} byte-exact elsewhere at its clipped size"
        );
    }
    for i in 3..N {
        assert_eq!(
            std::fs::read(mnt2.join(format!("t{i}.bin"))).unwrap(),
            pattern(i, len)
        );
    }
    assert_eq!(std::fs::read(mnt2.join(ANCHOR)).unwrap(), anchor_bytes());
    assert_eq!(stat_u64(&mnt2, "staged_payload_lost_reads"), 0);
    mount2.umount_timed();
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
        squeezefs::nvme_dev::clear_fail_next_writes();
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

/// The staging ring's write budget: roomy for the tenant-op contracts, and
/// tight ("4MB") for the spill contract, whose premise is a ring that
/// refuses a whole-image stage.
const ROOMY_RING: &str = "128MB";
const TIGHT_RING: &str = "4MB";

/// Mount-shaped fixture: `n` data volumes registered (the first is the
/// default slot), a staging dir, the job fabric with the mover context
/// wired (contract 4's drain), allocator recovery like a mount.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    records: Vec<DataVolumeRecord>,
    _staging: TempDir,
}

/// A fresh format + open: `n` volumes of `dev_bytes` each.
async fn open_fresh(dir: &Path, n: usize, dev_bytes: u64, tag: &str, ring: &str) -> (PathBuf, Fx) {
    let meta = make_dev_file(dir, &format!("meta-{tag}"), 256 * 1024 * 1024);
    let paths: Vec<PathBuf> = (0..n)
        .map(|i| make_dev_file(dir, &format!("oss{}-{tag}", i + 1), dev_bytes))
        .collect();
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    format_meta(&meta, &refs).await;
    let records = base_format_config(&refs).resolved_data_volumes();
    let fx = open_at(&meta, &records, ring).await;
    (meta, fx)
}

async fn open_at(meta: &Path, records: &[DataVolumeRecord], ring: &str) -> Fx {
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
        Some(ring),
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

    /// SETATTR(size) — the truncate the kernel issues.
    async fn truncate(&self, ino: u64, size: u64) {
        self.fs
            .setattr(
                req(),
                ino,
                None,
                fuse3::SetAttr {
                    size: Some(size),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("truncate ino {ino} to {size} failed: {e:?}"));
    }

    fn size(&self, ino: u64) -> u64 {
        self.fs
            .router
            .metadata_cache
            .get(&ino)
            .expect("RAM layout")
            .size
    }

    /// Whole-file `copy_file_range` into an EMPTY destination — the clone
    /// fast path (`DataRouter::clone_file`).
    async fn clone_whole(&self, src: u64, dst: u64, len: usize) {
        let copied = self
            .fs
            .copy_file_range(req(), src, 0, 0, dst, 0, 0, len as u64, 0)
            .await
            .unwrap_or_else(|e| panic!("copy_file_range {src} -> {dst} failed: {e:?}"))
            .copied;
        assert_eq!(copied as usize, len, "the whole file cloned");
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

    /// A staged file fsync-promoted into the open pack.
    async fn packed_file(&self, name: &str, len: usize, tag: usize) -> u64 {
        let (ino, fid) = self.staged_file(name, len, tag).await;
        self.fsync(ino).await;
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_none(),
            "fixture premise: the promotion released the ring entry"
        );
        ino
    }

    /// `block_map[0]` verbatim.
    fn mapping_str(&self, ino: u64) -> String {
        let m = self.fs.router.metadata_cache.get(&ino).expect("layout");
        m.block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[0]: {m:?}"))
    }

    /// The tenant mapping `block_map[0]` of a promoted file, decoded:
    /// `(base key, off, len)`.
    fn mapping(&self, ino: u64) -> (String, u64, usize) {
        let s = self.mapping_str(ino);
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

    /// The open packs' `(count, least occupancy permille)` — the slot
    /// ledger a clone or clip must leave untouched (the age gauge between
    /// them is elided: it moves with the clock).
    fn pack_gauges(&self) -> (u64, u64) {
        let (open, _age_ms, occupancy) = self.fs.router.packer.gauges();
        (open, occupancy)
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

/// A device write attempted between `arm` and `disarm` would consume the
/// fault-injection counter (and fail the op): a still-armed counter after
/// the op is the "zero data-plane writes" witness.
struct NoDeviceWrites;
impl NoDeviceWrites {
    fn arm() -> Self {
        squeezefs::nvme_dev::set_fail_next_writes(1);
        Self
    }
    fn disarm(self, what: &str) {
        assert_eq!(
            squeezefs::nvme_dev::FAIL_NEXT_WRITES.load(Ordering::SeqCst),
            1,
            "{what} issued a data-plane write (the armed fault counter was consumed)"
        );
        squeezefs::nvme_dev::clear_fail_next_writes();
    }
}

// ---------------------------------------------------------------------------
// Contract 1 (in-process face): the passthrough clip — the device write seam
// ---------------------------------------------------------------------------

/// Three tenants in one open pack (`refcount = 3 + pin`); tenant 1 shrinks
/// 16 KiB → 9 000 B with the device write fault counter ARMED: the counter
/// stays untouched (no DMA), the mapping reads `base:off1:9000` (same base,
/// same slot), the refcount and the durable population are unchanged, the
/// oracle is clean, and a mount-style reopen seeds `refcount = 3` from the
/// three surviving records with every tenant byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_shrink_of_a_passthrough_tenant_is_a_mapping_clip_with_zero_device_writes() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "clip", ROOMY_RING).await;
    let len = 16 * KIB;
    let inos = [
        fx.packed_file("c0.bin", len, 100).await,
        fx.packed_file("c1.bin", len, 101).await,
        fx.packed_file("c2.bin", len, 102).await,
    ];
    let (base, off1, len1) = fx.mapping(inos[1]);
    assert_eq!(len1, len);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    assert_eq!(alloc.refcount(offset), Some(4), "3 tenants + the pin");
    let gauges0 = fx.pack_gauges();
    let clips0 = metric(&METRICS.pack_mapping_clips);
    let durable0 = metric(&METRICS.staged_truncate_durable_clips);
    let partial0 = metric(&METRICS.pack_partial_frees);
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let abandoned0 = metric(&METRICS.pack_slots_abandoned);

    const NEW: usize = 9_000;
    let seam = NoDeviceWrites::arm();
    fx.truncate(inos[1], NEW as u64).await;
    seam.disarm("the passthrough clip");

    assert_eq!(
        fx.mapping(inos[1]),
        (base.clone(), off1, NEW),
        "the mapping clip: same base, same slot, len' = new_size"
    );
    assert_eq!(fx.size(inos[1]), NEW as u64);
    assert_eq!(fx.read(inos[1], NEW).await, pattern(101, NEW), "byte-exact");
    assert_eq!(fx.read(inos[0], len).await, pattern(100, len), "sibling 0");
    assert_eq!(fx.read(inos[2], len).await, pattern(102, len), "sibling 2");
    assert_eq!(metric(&METRICS.pack_mapping_clips) - clips0, 1);
    assert_eq!(metric(&METRICS.staged_truncate_durable_clips) - durable0, 0);
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        0,
        "the old mapping is NOT freed — no reference moved"
    );
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 0);
    assert_eq!(metric(&METRICS.pack_slots_abandoned) - abandoned0, 0);
    assert_eq!(
        alloc.refcount(offset),
        Some(4),
        "the RAM refcount is unchanged"
    );
    assert_eq!(
        fx.pack_gauges(),
        gauges0,
        "no slot reserved: one open pack, same occupancy"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the oracle is clean after the clip"
    );

    // The durable population: seal, reopen, seed from the ledger.
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    assert_eq!(alloc.refcount(offset), Some(3));
    let records = fx.records.clone();
    fx.close().await;
    let fx = open_at(&meta, &records, ROOMY_RING).await;
    let alloc = fx.alloc(0);
    assert_eq!(
        alloc.refcount(offset),
        Some(3),
        "after reopen: one record per tenant — the clipped tenant's survived its Delete+Put"
    );
    assert!(fx.drift().await.is_empty());
    assert_eq!(fx.mapping(inos[1]), (base, off1, NEW));
    assert_eq!(fx.read(inos[1], NEW).await, pattern(101, NEW));
    assert_eq!(fx.read(inos[0], len).await, pattern(100, len));
    assert_eq!(fx.read(inos[2], len).await, pattern(102, len));
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 2: the transformed clip lands as a NEW tenant
// ---------------------------------------------------------------------------

/// On an lz4 volume the stored image is a frame, so a shrink re-encodes
/// the clipped plaintext and lands it THROUGH THE PACKED ARM: a fresh slot
/// of the same open pack (`layout_promoted_packed + 1`, a new `off`), the
/// old tenant reference partially freed (`pack_partial_frees + 1`), the
/// RAM refcount and the durable population net unchanged (−1 +1 on the
/// same block), byte-exact at the clipped size; the mapping-clip counter
/// stays flat (a transformed image cannot be re-described). A truncate-UP
/// afterwards is size-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transformed_truncate_shrink_lands_the_clipped_image_as_a_new_tenant() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "lz4clip", ROOMY_RING).await;
    fx.fs
        .router
        .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    assert!(!fx.fs.router.get_crypto().is_passthrough());
    let len = 24 * KIB;
    let a = fx.packed_file("z0.bin", len, 110).await;
    let b = fx.packed_file("z1.bin", len, 111).await;
    let (base, off_a, _) = fx.mapping(a);
    let (base_b, off_b, _) = fx.mapping(b);
    assert_eq!(base, base_b, "one pack");
    assert_ne!(off_a, off_b);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    assert_eq!(alloc.refcount(offset), Some(3), "2 tenants + the pin");
    let clips0 = metric(&METRICS.pack_mapping_clips);
    let durable0 = metric(&METRICS.staged_truncate_durable_clips);
    let partial0 = metric(&METRICS.pack_partial_frees);
    let terminal0 = metric(&METRICS.pack_terminal_frees);
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let block0 = metric(&METRICS.layout_promoted_block);
    let opened0 = metric(&METRICS.pack_blocks_opened);

    const NEW: usize = 10_000;
    fx.truncate(a, NEW as u64).await;

    let (base2, off_a2, len_a2) = fx.mapping(a);
    assert_eq!(
        base2, base,
        "the clipped image landed in the SAME open pack"
    );
    assert_ne!(
        off_a2, off_a,
        "a fresh slot — a transformed image is re-encoded"
    );
    assert_ne!(off_a2, off_b);
    assert!(len_a2 > 0 && len_a2 as u64 <= CHUNK_SIZE);
    assert_eq!(fx.size(a), NEW as u64);
    assert_eq!(
        fx.read(a, NEW).await,
        pattern(110, NEW),
        "byte-exact at the clipped size"
    );
    assert_eq!(fx.read(b, len).await, pattern(111, len), "the sibling");
    assert_eq!(
        metric(&METRICS.layout_promoted_packed) - packed0,
        1,
        "the clipped image is a packed tenant"
    );
    assert_eq!(metric(&METRICS.layout_promoted_block) - block0, 0);
    assert_eq!(
        metric(&METRICS.pack_blocks_opened) - opened0,
        0,
        "no new pack"
    );
    assert_eq!(metric(&METRICS.staged_truncate_durable_clips) - durable0, 1);
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        1,
        "the old tenant reference is partially freed"
    );
    assert_eq!(metric(&METRICS.pack_terminal_frees) - terminal0, 0);
    assert_eq!(metric(&METRICS.pack_mapping_clips) - clips0, 0);
    assert_eq!(
        alloc.refcount(offset),
        Some(3),
        "−1 (old tenant) +1 (new tenant) on the same block"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the oracle: population net unchanged"
    );

    // Truncate-UP is size-only.
    fx.truncate(a, len as u64).await;
    assert_eq!(
        fx.mapping(a),
        (base, off_a2, len_a2),
        "the mapping is untouched"
    );
    assert_eq!(fx.read(a, len).await, clipped_then_zero(110, NEW, len));
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 1);
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 3: truncate-up is size-only
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_up_of_a_packed_tenant_is_size_only() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "up", ROOMY_RING).await;
    let len = 16 * KIB;
    let ino = fx.packed_file("u0.bin", len, 120).await;
    let mapping0 = fx.mapping(ino);
    let offset = fx.offset_of(&mapping0.0);
    let alloc = fx.alloc(0);
    let gauges0 = fx.pack_gauges();
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let opened0 = metric(&METRICS.pack_blocks_opened);
    let clips0 = metric(&METRICS.pack_mapping_clips);
    let durable0 = metric(&METRICS.staged_truncate_durable_clips);
    let partial0 = metric(&METRICS.pack_partial_frees);

    const UP: usize = 40_000;
    let seam = NoDeviceWrites::arm();
    fx.truncate(ino, UP as u64).await;
    seam.disarm("truncate-up");

    assert_eq!(fx.size(ino), UP as u64);
    assert_eq!(fx.mapping(ino), mapping0, "the mapping is untouched");
    assert_eq!(
        fx.read(ino, UP).await,
        clipped_then_zero(120, len, UP),
        "implicit-zero tail"
    );
    assert_eq!(
        metric(&METRICS.layout_promoted_packed) - packed0,
        0,
        "no promotion"
    );
    assert_eq!(metric(&METRICS.pack_blocks_opened) - opened0, 0);
    assert_eq!(metric(&METRICS.pack_mapping_clips) - clips0, 0);
    assert_eq!(metric(&METRICS.staged_truncate_durable_clips) - durable0, 0);
    assert_eq!(metric(&METRICS.pack_partial_frees) - partial0, 0);
    assert_eq!(alloc.refcount(offset), Some(2), "tenant + pin, unchanged");
    assert_eq!(fx.pack_gauges(), gauges0, "no pack movement");
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 4: the mover defers an OPEN pack block; a sealed one moves whole
// ---------------------------------------------------------------------------

/// Four tenants share an OPEN pack on the drain victim. The drain's first
/// pass DEFERS the block (`pack_mover_open_defers`,
/// `evacuate_deferred_staged_blocks`) — and, the source being the victim,
/// seals it (`pack_blocks_sealed_drain + 1`: the pin released, the ledger
/// left) so the next re-plan moves it as ONE block with every tenant's own
/// `:off:len` preserved and byte-exact on the survivor; the oracle is
/// clean; the next promotion opens a fresh pack on the survivor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_drain_defers_an_open_pack_block_seals_it_and_moves_it_as_a_unit() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 2, 4 << 30, "opendrain", ROOMY_RING).await;
    fx.place_only_on(1);
    let len = 32 * KIB;
    let mut inos = Vec::new();
    for i in 0..4 {
        inos.push(fx.packed_file(&format!("od{i}.bin"), len, 130 + i).await);
    }
    let (base, _, _) = fx.mapping(inos[0]);
    let victim_id = fx.records[1].id.clone();
    assert!(
        base.starts_with(&format!("{victim_id}://")),
        "on the victim: {base}"
    );
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(1);
    assert_eq!(alloc.refcount(offset), Some(5), "4 tenants + the pin: OPEN");
    assert!(
        squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "the pack-open ledger names the block"
    );
    assert!(
        alloc.fill_incarnation(offset).is_some(),
        "STABLE word — pin-eligible"
    );

    fx.clear_health_overrides();
    let open_defers0 = metric(&METRICS.pack_mover_open_defers);
    let deferred0 = metric(&METRICS.evacuate_deferred_staged_blocks);
    let sealed_drain0 = metric(&METRICS.pack_blocks_sealed_drain);
    let sealed_full0 = metric(&METRICS.pack_blocks_sealed_full);
    let moved0 = metric(&METRICS.evacuate_blocks_moved);
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
    assert_eq!(end, JobState::Completed, "the drain converges");
    assert!(
        metric(&METRICS.pack_mover_open_defers) - open_defers0 >= 1,
        "the open pack was deferred at least once"
    );
    assert!(metric(&METRICS.evacuate_deferred_staged_blocks) - deferred0 >= 1);
    assert_eq!(
        metric(&METRICS.pack_blocks_sealed_drain) - sealed_drain0,
        1,
        "the deferring pass sealed the victim's open pack"
    );
    assert_eq!(metric(&METRICS.pack_blocks_sealed_full) - sealed_full0, 0);
    assert!(metric(&METRICS.evacuate_blocks_moved) - moved0 >= 1);
    assert!(
        !squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "the sealed pack left the ledger"
    );
    assert_eq!(fx.pack_gauges().0, 0, "no open pack after the drain");

    let mut dst_bases = std::collections::HashSet::new();
    for (i, &ino) in inos.iter().enumerate() {
        let (b, off, l) = fx.mapping(ino);
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
            pattern(130 + i, len),
            "tenant {i} byte-exact"
        );
    }
    assert_eq!(dst_bases.len(), 1, "moved as ONE block");
    let dst_base = dst_bases.into_iter().next().unwrap();
    let survivor_alloc = fx.alloc(0);
    assert_eq!(
        survivor_alloc.refcount(fx.offset_of(&dst_base)),
        Some(4),
        "the destination holds exactly the four tenants (the pin never travels)"
    );
    assert!(fx.drift().await.is_empty());

    // The packer re-opens on the survivor.
    let opened0 = metric(&METRICS.pack_blocks_opened);
    let fresh = fx.packed_file("od_fresh.bin", len, 140).await;
    assert_eq!(
        metric(&METRICS.pack_blocks_opened) - opened0,
        1,
        "a fresh pack"
    );
    let (fresh_base, _, _) = fx.mapping(fresh);
    assert!(!fresh_base.starts_with(&format!("{victim_id}://")));
    assert_ne!(fresh_base, dst_base, "never into the moved (sealed) block");
    assert_eq!(fx.read(fresh, len).await, pattern(140, len));
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 5: a spill escalation under StorageFull packs (lever OFF: block)
// ---------------------------------------------------------------------------

/// Oversubscribe the staging ring with rider-pinned live staged files (the
/// FIND-RW5-A / generic-464 shape: a LIVE rider record defers promotion,
/// so the ring cannot self-drain).
async fn fill_ring(fx: &Fx, files: usize, len: usize, tag: usize) {
    for i in 0..files {
        let ino = fx.create(&format!("filler_{tag}_{i}")).await;
        fx.write_at(ino, 0, &pattern(tag + i, len)).await;
        fx.write_at(ino, 4096, &pattern(tag ^ 0x0F, 2048)).await;
    }
}

/// The composed image of `src`: its base pattern with the rider run laid
/// over it.
fn composed(base_tag: usize, len: usize, rider_off: usize, rider: &[u8]) -> Vec<u8> {
    let mut want = pattern(base_tag, len);
    want[rider_off..rider_off + rider.len()].copy_from_slice(rider);
    want
}

/// One run of contract 5 at a lever setting: a ring-RESIDENT source (a
/// rider pins it) is cloned under a full ring — the staged-clone spill —
/// then its rider is folded under the same full ring — the rider-fold
/// spill. Returns `(dst ino, src ino, dst mapping, src mapping)`.
async fn spill_run(fx: &Fx, tag: usize) -> (u64, u64, (String, u64, usize), (String, u64, usize)) {
    let img_len = 700 * KIB;
    let rider_off = 64 * KIB;
    let rider = pattern(tag + 7, 2048);
    let (src, src_fid) = fx.staged_file("spill_src.bin", img_len, tag).await;
    fx.write_at(src, rider_off as u64, &rider).await;
    assert!(
        fx.fs.router.cache.nvme.read_staged(&src_fid).is_some(),
        "premise: the source stays ring-resident (rider-pinned)"
    );
    fill_ring(fx, 6, 700 * KIB, tag + 50).await;
    let want = composed(tag, img_len, rider_off, &rider);

    // (a) the staged-clone spill.
    let dst = fx.create("spill_dst.bin").await;
    let spills0 = metric(&METRICS.staged_spill_escalations);
    let src_tok = fx.fs.dlm().get_fencing_token_ino(src);
    let dst_tok = fx.fs.dlm().get_fencing_token_ino(dst).max(1);
    fx.fs
        .router
        .clone_file(
            &squeezefs::keys::inode_path(src),
            &squeezefs::keys::inode_path(dst),
            Some(src_tok),
            Some(dst_tok),
        )
        .await
        .expect("a staged clone never surfaces StorageFull");
    assert_eq!(
        metric(&METRICS.staged_spill_escalations) - spills0,
        1,
        "premise: the clone took the durable-spill escalation"
    );
    assert_eq!(
        fx.read(dst, img_len).await,
        want,
        "the clone's composed image"
    );
    let dst_mapping = fx.mapping(dst);

    // (b) the rider-fold spill.
    let spills1 = metric(&METRICS.staged_spill_escalations);
    let folded = fx
        .fs
        .fold_extent_block(src, 0)
        .await
        .expect("a rider fold never surfaces StorageFull");
    assert!(folded, "the rider record was present: the fold engaged");
    assert_eq!(
        metric(&METRICS.staged_spill_escalations) - spills1,
        1,
        "premise: the fold took the durable-spill escalation"
    );
    assert_eq!(fx.read(src, img_len).await, want, "the folded source");
    let src_mapping = fx.mapping(src);
    (dst, src, dst_mapping, src_mapping)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spill_escalation_under_storage_full_packs_and_lever_off_takes_the_block_arm() {
    let _g = serial().await;
    let _l = arm_levers();

    // Lever ON: both escalations dispatch through the packed arm.
    {
        let dir = tempfile::tempdir().unwrap();
        let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "spillon", TIGHT_RING).await;
        let packed0 = metric(&METRICS.layout_promoted_packed);
        let (_dst, _src, (dst_base, dst_off, dst_len), (src_base, src_off, src_len)) =
            spill_run(&fx, 200).await;
        assert_eq!(
            metric(&METRICS.layout_promoted_packed) - packed0,
            2,
            "the clone spill and the fold spill each landed a packed tenant"
        );
        assert_eq!(dst_base, src_base, "both tenants in the one open pack");
        assert_ne!(dst_off, src_off, "distinct slots");
        assert!(dst_len < CHUNK_SIZE as usize && src_len < CHUNK_SIZE as usize);
        assert!(
            squeezefs::jobs::pack_open_ledger()
                .iter()
                .any(|k| k == &dst_base),
            "the pack is open"
        );
        let alloc = fx.alloc(0);
        assert_eq!(
            alloc.refcount(fx.offset_of(&dst_base)),
            Some(3),
            "two tenants + the pin"
        );
        assert!(fx.drift().await.is_empty());
        assert_eq!(metric(&METRICS.block_untracked_free_refusals), 0);
        fx.close().await;
    }

    // Lever OFF: the block arm, byte-identically — `bk:0:len`, own blocks.
    squeezefs::routing::test_set_small_file_packing(Some(false));
    {
        let dir = tempfile::tempdir().unwrap();
        let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "spilloff", TIGHT_RING).await;
        let packed0 = metric(&METRICS.layout_promoted_packed);
        let opened0 = metric(&METRICS.pack_blocks_opened);
        let (_dst, _src, (dst_base, dst_off, _), (src_base, src_off, _)) =
            spill_run(&fx, 300).await;
        assert_eq!(
            metric(&METRICS.layout_promoted_packed) - packed0,
            0,
            "nothing packed"
        );
        assert_eq!(
            metric(&METRICS.pack_blocks_opened) - opened0,
            0,
            "no pack opened"
        );
        assert_eq!((dst_off, src_off), (0, 0), "bk:0:len");
        assert_ne!(dst_base, src_base, "one block each");
        let alloc = fx.alloc(0);
        assert_eq!(alloc.refcount(fx.offset_of(&dst_base)), Some(1));
        assert_eq!(alloc.refcount(fx.offset_of(&src_base)), Some(1));
        assert!(fx.drift().await.is_empty());
        fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// Contract 6: a clone of a PROMOTED packed tenant shares the window
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_of_a_promoted_packed_tenant_shares_its_window() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "clone", ROOMY_RING).await;
    let len = 16 * KIB;
    let src = fx.packed_file("cl_src.bin", len, 150).await;
    let sib = fx.packed_file("cl_sib.bin", len, 151).await;
    let src_mapping = fx.mapping_str(src);
    let (base, _, _) = fx.mapping(src);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    assert_eq!(alloc.refcount(offset), Some(3), "2 tenants + the pin");
    let gauges0 = fx.pack_gauges();
    let packed0 = metric(&METRICS.layout_promoted_packed);

    let dst = fx.create("cl_dst.bin").await;
    fx.clone_whole(src, dst, len).await;
    assert_eq!(
        fx.mapping_str(dst),
        src_mapping,
        "two inos, one identical `bk:off:len` window"
    );
    assert_eq!(
        alloc.refcount(offset),
        Some(4),
        "the clone took its RAM reference (FIND-PK-3): refcount 2 for the shared window"
    );
    assert_eq!(fx.pack_gauges(), gauges0, "one slot: no reservation");
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 0);
    assert_eq!(
        fx.read(dst, len).await,
        pattern(150, len),
        "the clone byte-exact"
    );
    assert_eq!(
        fx.read(src, len).await,
        pattern(150, len),
        "the source byte-exact"
    );
    assert!(
        fx.drift().await.is_empty(),
        "two records for the shared window"
    );

    // Delete one: nonterminal, the survivor byte-exact.
    let partial0 = metric(&METRICS.pack_partial_frees);
    let terminal0 = metric(&METRICS.pack_terminal_frees);
    fx.unlink("cl_src.bin", src).await;
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        1,
        "nonterminal"
    );
    assert_eq!(metric(&METRICS.pack_terminal_frees) - terminal0, 0);
    assert_eq!(alloc.refcount(offset), Some(3));
    assert_eq!(
        fx.read(dst, len).await,
        pattern(150, len),
        "the survivor byte-exact"
    );
    assert_eq!(fx.read(sib, len).await, pattern(151, len));
    assert!(fx.drift().await.is_empty());

    // The durable population on a reopen: clone + sibling.
    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    let records = fx.records.clone();
    fx.close().await;
    let fx = open_at(&meta, &records, ROOMY_RING).await;
    assert_eq!(fx.alloc(0).refcount(offset), Some(2), "clone + sibling");
    assert!(fx.drift().await.is_empty());
    assert_eq!(fx.read(dst, len).await, pattern(150, len));
    assert_eq!(fx.read(sib, len).await, pattern(151, len));
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 7: clone-then-clip and clip-then-clone — nested same-off windows
// ---------------------------------------------------------------------------

/// Tenant A (24 KiB) with a sibling S. Clone-then-clip: B = clone(A), clip
/// B to 10 000 ⇒ A `base:off:24K`, B `base:off:10000` — nested, same `off`.
/// Clip-then-clone: clip A to 15 000, C = clone(A) (`base:off:15000`), clip
/// C to 6 000 ⇒ three nested windows at one `off`. Every window reads its
/// own bytes; the refcount is tenants + pin (one reference per ino, none
/// moved by a clip); the pack reserved no new slot; the oracle is clean;
/// deletes are nonterminal; a reopen seeds the survivors from the ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_then_clip_and_clip_then_clone_yield_nested_same_off_windows() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "nested", ROOMY_RING).await;
    let len = 24 * KIB;
    let a = fx.packed_file("n_a.bin", len, 160).await;
    let s = fx.packed_file("n_s.bin", 8 * KIB, 161).await;
    let (base, off_a, _) = fx.mapping(a);
    let offset = fx.offset_of(&base);
    let alloc = fx.alloc(0);
    assert_eq!(alloc.refcount(offset), Some(3), "A + S + the pin");
    let gauges0 = fx.pack_gauges();
    let clips0 = metric(&METRICS.pack_mapping_clips);
    let partial0 = metric(&METRICS.pack_partial_frees);

    // Clone-then-clip.
    let b = fx.create("n_b.bin").await;
    fx.clone_whole(a, b, len).await;
    assert_eq!(fx.mapping(b), (base.clone(), off_a, len));
    fx.truncate(b, 10_000).await;
    assert_eq!(
        fx.mapping(b),
        (base.clone(), off_a, 10_000),
        "B: nested, same off"
    );
    assert_eq!(
        fx.mapping(a),
        (base.clone(), off_a, len),
        "A: the full window"
    );
    assert_eq!(fx.read(b, 10_000).await, pattern(160, 10_000));
    assert_eq!(fx.read(a, len).await, pattern(160, len));
    assert_eq!(alloc.refcount(offset), Some(4), "A + B + S + pin");
    assert_eq!(metric(&METRICS.pack_mapping_clips) - clips0, 1);
    assert!(fx.drift().await.is_empty(), "clone-then-clip: oracle clean");

    // Clip-then-clone.
    fx.truncate(a, 15_000).await;
    assert_eq!(fx.mapping(a), (base.clone(), off_a, 15_000));
    let c = fx.create("n_c.bin").await;
    fx.clone_whole(a, c, 15_000).await;
    assert_eq!(
        fx.mapping(c),
        (base.clone(), off_a, 15_000),
        "C shares the clipped window"
    );
    fx.truncate(c, 6_000).await;
    assert_eq!(
        fx.mapping(c),
        (base.clone(), off_a, 6_000),
        "C: nested inside A's"
    );
    assert_eq!(fx.read(a, 15_000).await, pattern(160, 15_000));
    assert_eq!(fx.read(b, 10_000).await, pattern(160, 10_000));
    assert_eq!(fx.read(c, 6_000).await, pattern(160, 6_000));
    assert_eq!(fx.read(s, 8 * KIB).await, pattern(161, 8 * KIB));
    assert_eq!(alloc.refcount(offset), Some(5), "A + B + C + S + pin");
    assert_eq!(metric(&METRICS.pack_mapping_clips) - clips0, 3);
    assert_eq!(
        metric(&METRICS.pack_partial_frees) - partial0,
        0,
        "no clip moved a reference"
    );
    assert_eq!(fx.pack_gauges(), gauges0, "one slot for the whole family");
    assert!(fx.drift().await.is_empty(), "clip-then-clone: oracle clean");

    // Deletes: nonterminal; the nested survivors keep their bytes.
    fx.unlink("n_a.bin", a).await;
    assert_eq!(metric(&METRICS.pack_partial_frees) - partial0, 1);
    assert_eq!(alloc.refcount(offset), Some(4));
    assert_eq!(fx.read(b, 10_000).await, pattern(160, 10_000));
    assert_eq!(fx.read(c, 6_000).await, pattern(160, 6_000));
    fx.unlink("n_c.bin", c).await;
    assert_eq!(metric(&METRICS.pack_partial_frees) - partial0, 2);
    assert_eq!(fx.read(b, 10_000).await, pattern(160, 10_000));
    assert!(fx.drift().await.is_empty());

    assert_eq!(fx.fs.router.seal_open_packs().await, 1);
    let records = fx.records.clone();
    fx.close().await;
    let fx = open_at(&meta, &records, ROOMY_RING).await;
    assert_eq!(
        fx.alloc(0).refcount(offset),
        Some(2),
        "B + S from the ledger"
    );
    assert!(fx.drift().await.is_empty());
    assert_eq!(fx.mapping(b), (base, off_a, 10_000));
    assert_eq!(fx.read(b, 10_000).await, pattern(160, 10_000));
    assert_eq!(fx.read(s, 8 * KIB).await, pattern(161, 8 * KIB));
    fx.close().await;
}
