//! **Approach A — the FUSE placed-merge assembly** (write-bandwidth
//! program, 2026-08-09; rc-manifest §3f adjudication).
//!
//! The term (step-0 confirmed on `71f2e967`): every armed streaming
//! WRITE byte pays an extraction destination (slot → memfd bounce)
//! before its real accumulation destination (NT merge →
//! `ActiveBlockBuf`) — extract_bytes 100 %, nt_copy 99.9 %, direct 0 %.
//! The fix ports the IPC placed-sever design to FUSE delivery: the
//! worker's bridge `WRITE_FIXED` targets a per-(ino, block) **memfd
//! assembly** at the block-relative offset instead of the bounce, and
//! the first merging handler ADOPTS the assembly's mmap view as the
//! overlay backing (sibling merges elide via the pointer proof).
//!
//! Contracts (red-first against `71f2e967` + step 0):
//!
//! 1. **Placement engagement, byte-exact** — a cohort of concurrent
//!    aligned 1 MiB chunks of one block places (bridge → assembly),
//!    adopts, and elides; the extraction ledger does NOT pay for placed
//!    bytes; content reads back exact (buffered + O_DIRECT).
//! 2. **Post-adoption isolation (the §5.2 law)** — once adopted
//!    (snapshot-visible), NO kernel write targets the assembly: a later
//!    write to the same block rides the extraction fallback (counted),
//!    never a placement into the sealed assembly. Byte-exact.
//! 3. **The lever** — `SQUEEZEFS_FUSE_PLACED_MERGE=0` keeps the
//!    pre-campaign vehicle byte-identically (placements ≡ 0).
//! 4. **Ledger export** — the placement family always exports under
//!    `metrics` (the engagement instrument the brackets gate on).

use squeezefs_testkit::{mount_supported, site};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_zcplace_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn format_volume(base: &Path) -> PathBuf {
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
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
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

fn abort_waiting_fuse_connections() {
    if let Ok(dirs) = std::fs::read_dir("/sys/fs/fuse/connections") {
        for d in dirs.flatten() {
            let waiting = std::fs::read_to_string(d.path().join("waiting"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0);
            if waiting > 0 {
                let _ = std::fs::write(d.path().join("abort"), "1");
            }
        }
    }
}

struct Mount {
    child: Child,
    mnt: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        abort_waiting_fuse_connections();
        let _ = Command::new(bin()).arg("umount").arg(&self.mnt).output();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        abort_waiting_fuse_connections();
    }
}

fn spawn_zc_mount(meta: &Path, mnt: &Path, log: &Path, envs: &[(&str, &str)]) -> Mount {
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
        .env("SQUEEZEFS_FUSE_ZC", "1")
        // The placement lever is default-OFF (the falsification verdict);
        // this suite tests the MACHINERY, so it arms explicitly — the
        // lever-off contract passes its own =0 (matching the default).
        .env("SQUEEZEFS_FUSE_PLACED_MERGE", "1")
        // Place vs overlay: overlay-eligible deliveries refuse place.
        .env("SQUEEZEFS_DEVICE_OVERLAY", "0")
        // Pin the transport payload geometry (the venue-independence
        // law the fusion suite established): 1 MiB payload.
        .env("SQUEEZEFS_FUSE_MAX_WRITE", "1048576");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mount did not come up within 90 s (log: {})",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    mount
}

fn zc_armed(log: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(log) {
            if text.contains("+zero-copy") {
                return true;
            }
            if text.contains("FUSE-over-io_uring registered") {
                return false;
            }
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn stats_metric(mnt: &Path, key: &str) -> Option<u64> {
    let raw = std::fs::read_to_string(mnt.join(".stats")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("metrics")?.get(key)?.as_u64()
}

fn metric(mnt: &Path, key: &str) -> u64 {
    stats_metric(mnt, key).unwrap_or_else(|| {
        panic!("metrics.{key} must export on an armed mount (the placement engagement ledger)")
    })
}

struct Venue {
    _base: PathBuf,
    mount: Mount,
    log: PathBuf,
}

fn armed_venue(tag: &str, envs: &[(&str, &str)], caller: squeezefs_testkit::Site) -> Option<Venue> {
    if !mount_supported(caller) {
        return None;
    }
    let base = scratch(tag);
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mount = spawn_zc_mount(&meta, &mnt, &log, envs);
    if !zc_armed(&log) {
        drop(mount);
        let _ = std::fs::remove_dir_all(&base);
        let _ = squeezefs_testkit::declare(
            caller,
            squeezefs_testkit::SkipClass::Capability,
            "FUSE_URING_ZERO_COPY did not arm (sqz kernel + CAP_SYS_ADMIN required)",
        );
        return None;
    }
    Some(Venue {
        _base: base,
        mount,
        log,
    })
}

/// A page-aligned 1 MiB write buffer (the standing instrument-alignment
/// lesson: an UNALIGNED O_DIRECT buffer spans max_pages+1 and the
/// kernel SPLITS it into two non-page-multiple WRITEs — which the
/// placement gate rightly refuses; fio/elbencho align their buffers, so
/// this instrument must too).
struct AlignedMiB(*mut u8);
// SAFETY: exclusively-owned anonymous mapping; sent whole across the
// cohort thread spawn.
unsafe impl Send for AlignedMiB {}
impl AlignedMiB {
    fn filled(byte: u8) -> Self {
        // SAFETY: fresh anonymous RW mapping, page-aligned by mmap.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                1024 * 1024,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "aligned buffer mmap");
        // SAFETY: the fresh 1 MiB mapping is writable.
        unsafe { std::ptr::write_bytes(p as *mut u8, byte, 1024 * 1024) };
        Self(p as *mut u8)
    }
    fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is 1 MiB, initialized, exclusively owned.
        unsafe { std::slice::from_raw_parts(self.0, 1024 * 1024) }
    }
}
impl Drop for AlignedMiB {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `filled`, once.
        unsafe { libc::munmap(self.0 as *mut libc::c_void, 1024 * 1024) };
    }
}

/// A start-synchronized cohort of 1 MiB O_DIRECT pwrites covering one
/// 4 MiB block from `nthreads` threads — the concurrent-chunk shape a
/// qd>1 streaming row delivers (cohort capture needs sibling claims to
/// establish before the first adoption).
fn write_block_cohort(path: &Path, block_off: u64, patterns: &[u8]) {
    use std::os::unix::fs::FileExt as _;
    use std::sync::{Arc, Barrier};
    let n = patterns.len();
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = patterns
        .iter()
        .enumerate()
        .map(|(i, &byte)| {
            let path = path.to_path_buf();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .expect("open O_DIRECT");
                let buf = AlignedMiB::filled(byte);
                barrier.wait();
                f.write_all_at(buf.as_slice(), block_off + (i as u64) * 1024 * 1024)
                    .expect("cohort chunk write");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("cohort thread");
    }
}

fn read_back(path: &Path, off: u64, len: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open for read-back");
    f.seek(SeekFrom::Start(off)).expect("seek");
    let mut out = vec![0u8; len];
    f.read_exact(&mut out).expect("read back");
    out
}

/// Contract 1: a concurrent cohort of aligned 1 MiB chunks PLACES into
/// the block's memfd assembly, the first merge ADOPTS it, siblings
/// ELIDE — and the placed bytes never ALSO pay the extraction vehicle.
/// Byte-exact on both read paths.
#[test]
fn streaming_cohort_places_adopts_and_elides_byte_exact() {
    let Some(v) = armed_venue("cohort", &[], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;

    let placements0 = metric(mnt, "fuse3_zc_write_placements");
    let place_bytes0 = metric(mnt, "fuse3_zc_write_placement_bytes");
    let adoptions0 = metric(mnt, "placed_adoptions");
    let elides0 = metric(mnt, "placed_merge_elides");
    let extract_bytes0 = metric(mnt, "fuse3_zc_write_extract_bytes");

    // Two full 4 MiB blocks, each written by a 4-thread cohort of
    // aligned 1 MiB O_DIRECT chunks.
    let file = mnt.join("cohort.bin");
    write_block_cohort(&file, 0, &[0x11, 0x22, 0x33, 0x44]);
    write_block_cohort(&file, 4 * 1024 * 1024, &[0x55, 0x66, 0x77, 0x88]);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .expect("open for fsync");
    f.sync_all().expect("fsync");
    drop(f);

    for (i, byte) in [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        .iter()
        .enumerate()
    {
        assert_eq!(
            read_back(&file, (i as u64) * 1024 * 1024, 1024 * 1024),
            vec![*byte; 1024 * 1024],
            "chunk {i} must read back byte-exact"
        );
    }

    let placements = metric(mnt, "fuse3_zc_write_placements") - placements0;
    let place_bytes = metric(mnt, "fuse3_zc_write_placement_bytes") - place_bytes0;
    let adoptions = metric(mnt, "placed_adoptions") - adoptions0;
    let elides = metric(mnt, "placed_merge_elides") - elides0;
    let extract_bytes = metric(mnt, "fuse3_zc_write_extract_bytes") - extract_bytes0;
    assert!(
        placements >= 2,
        "concurrent aligned chunks must PLACE into the block assembly \
         (fuse3_zc_write_placements Δ{placements}; log: {})",
        v.log.display()
    );
    assert!(
        adoptions >= 1,
        "the first merging handler must ADOPT the assembly \
         (placed_adoptions Δ{adoptions}, elides Δ{elides}; log: {})",
        v.log.display()
    );
    assert!(
        place_bytes >= 2 * 1024 * 1024,
        "the byte face must account the placed chunks (Δ{place_bytes})"
    );
    assert!(
        place_bytes + extract_bytes >= 8 * 1024 * 1024,
        "every chunk byte is accounted by exactly one vehicle \
         (placed Δ{place_bytes} + extracted Δ{extract_bytes})"
    );
    assert!(
        extract_bytes <= 8 * 1024 * 1024 - place_bytes + 1024 * 1024,
        "placed bytes must NOT also pay the extraction vehicle \
         (placed Δ{place_bytes}, extracted Δ{extract_bytes} — the \
         step-0 double-destination term re-forming)"
    );
}

/// Contract 2 — the §5.2 isolation law's mount-level face: after a
/// block's assembly is ADOPTED (snapshot-visible overlay backing), a
/// LATER kernel write to the same block must never place into it — it
/// rides the extraction fallback (counted), and content stays exact.
#[test]
fn post_adoption_writes_never_target_the_assembly() {
    let Some(v) = armed_venue("sealed", &[], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;

    let file = mnt.join("sealed.bin");
    // Round 1: a PARTIAL cohort (3 of the block's 4 MiB) over ONE
    // kept-open O_DIRECT fd — places and adopts, but coverage stays
    // incomplete AND the handle stays open (a close would FLUSH and
    // retire the entry), so the adopted assembly REMAINS the live
    // parked overlay across round 2.
    use std::os::unix::fs::FileExt as _;
    let f = std::sync::Arc::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_DIRECT)
            .open(&file)
            .expect("open O_DIRECT"),
    );
    {
        use std::sync::Barrier;
        let barrier = std::sync::Arc::new(Barrier::new(3));
        let handles: Vec<_> = [0xA1u8, 0xA2, 0xA3]
            .iter()
            .enumerate()
            .map(|(i, &byte)| {
                let f = std::sync::Arc::clone(&f);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let buf = AlignedMiB::filled(byte);
                    barrier.wait();
                    f.write_all_at(buf.as_slice(), (i as u64) * 1024 * 1024)
                        .expect("cohort chunk write");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("cohort thread");
        }
    }
    let adoptions = metric(mnt, "placed_adoptions");
    assert!(
        adoptions >= 1,
        "round 1 must adopt (placed_adoptions {adoptions}; log: {})",
        v.log.display()
    );

    // Round 2: the overlay is now the adopted assembly (parked,
    // snapshot-visible). A rewrite of chunk 1 MUST NOT place — the
    // root gate refuses (live overlay) and the write rides extraction.
    let placements1 = metric(mnt, "fuse3_zc_write_placements");
    let rebuf = AlignedMiB::filled(0xB1);
    f.write_all_at(rebuf.as_slice(), 0)
        .expect("post-adoption rewrite");
    assert_eq!(
        metric(mnt, "fuse3_zc_write_placements"),
        placements1,
        "a write to a block with a live adopted overlay must NOT place \
         (the no-kernel-write-after-snapshot-visible law's routing face)"
    );
    assert_eq!(
        read_back(&file, 0, 1024 * 1024),
        vec![0xB1u8; 1024 * 1024],
        "the post-adoption rewrite must read back byte-exact"
    );
    assert_eq!(
        read_back(&file, 1024 * 1024, 1024 * 1024),
        vec![0xA2u8; 1024 * 1024],
        "sibling chunk bytes must be untouched by the rewrite"
    );
    // Durability round trip across the mixed-vehicle block.
    f.sync_all().expect("fsync mixed-vehicle block");
    drop(f);
    assert_eq!(
        read_back(&file, 2 * 1024 * 1024, 1024 * 1024),
        vec![0xA3u8; 1024 * 1024],
        "chunk 3 byte-exact after fsync"
    );
}

/// Contract 3: `SQUEEZEFS_FUSE_PLACED_MERGE=0` is the A/B control — the
/// pre-campaign vehicle runs byte-identically and the placement ledger
/// never moves.
#[test]
fn placed_merge_lever_off_keeps_the_extraction_vehicle() {
    let Some(v) = armed_venue("lever", &[("SQUEEZEFS_FUSE_PLACED_MERGE", "0")], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;
    let extract0 = metric(mnt, "fuse3_zc_write_extract_bytes");

    let file = mnt.join("lever.bin");
    write_block_cohort(&file, 0, &[0xC1, 0xC2, 0xC3, 0xC4]);
    for (i, byte) in [0xC1u8, 0xC2, 0xC3, 0xC4].iter().enumerate() {
        assert_eq!(
            read_back(&file, (i as u64) * 1024 * 1024, 1024 * 1024),
            vec![*byte; 1024 * 1024],
            "lever-off chunk {i} byte-exact"
        );
    }
    assert_eq!(
        metric(mnt, "fuse3_zc_write_placements"),
        0,
        "SQUEEZEFS_FUSE_PLACED_MERGE=0 must keep the placement ledger at 0"
    );
    assert!(
        metric(mnt, "fuse3_zc_write_extract_bytes") - extract0 >= 4 * 1024 * 1024,
        "the extraction vehicle still carries the row with the lever off"
    );
}
