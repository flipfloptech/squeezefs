//! **Handler/worker fusion for small armed FUSE WRITEs** — the
//! CORRECTED predicate contracts (fused-lane-predicate campaign,
//! 2026-08-08, superseding the 2026-08-07 zc-write-fusion contracts).
//!
//! The field falsification (`bd6413f9`, squeeze-test 2026-08-08): at
//! ~300 µs fabric RTT the fused lane collapsed armed rand-4k writes to
//! 0.45× of the fusion-off posture (386k unarmed / 300k fusion-off /
//! 175k fused). Team adjudication: the fusion PREDICATE was wrong —
//! `zc::hold_candidate` (shape-only: aligned ∧ < payload/2) held/fused
//! shapes the W1 sole-owner patch will never consume. A W1-INELIGIBLE
//! shape (growth / unmapped / overlay / shared — the
//! `patch_ineligible_*` classes) paid hold + fused poll + LATE
//! extraction, serialized on the worker at fabric RTT — **ops paying
//! BOTH vehicles** (`fuse3_zc_write_fusions` ≈ ops ∧
//! `fuse3_zc_write_extractions` ≈ ops is the smoking-gun signature).
//!
//! The corrected routing this suite pins (red-first):
//!
//! 1. **Hold/fuse ONLY what a slot→device vehicle will consume**: the
//!    delivery-time hold gate composes the shape predicate with the
//!    ROOT's W1-eligibility probe (the `try_sole_owner_patch` ladder's
//!    cheap read-only mirror — `src/fuse_client.rs`), reached through
//!    the `Filesystem::zc_write_hold_eligible` seam. No gate (or a
//!    `false`) ⇒ never held.
//! 2. **Ineligible shapes extract AT DELIVERY** on the worker's batched
//!    pass (the streaming arm) and dispatch on the classic handler
//!    lanes — never after a fused handler poll, and never fused (their
//!    handler path parks on fabric-RTT FS state; the multi-lane venue
//!    owns that concurrency).
//! 3. **Staleness is bounded and counted**: a hint that turns
//!    ineligible between delivery and handler extracts LATE exactly
//!    once (`fuse3_zc_write_lazy_extractions` — the staleness gauge,
//!    ≈ 0 in steady state).
//!
//! Every mount here sets `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=1` explicitly:
//! the contracts are default-agnostic (the default flip rides its own
//! acceptance-gated commit).

use squeezefs_testkit::{mount_supported, site};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
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
        .join(format!("sqfs_zcfuse_{tag}_{}", std::process::id()));
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

/// Spawn the real daemon zc-armed, fusion lever ON (default-agnostic),
/// with per-test extra envs.
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
        .env("SQUEEZEFS_FUSE_ZC_WRITE_FUSION", "1")
        // Pin the transport payload geometry: the hold bound (payload/2)
        // and the derived fusion ceiling (payload/8) key on it, and the
        // host's fs.fuse.max_pages_limit would otherwise vary the
        // negotiated size across venues (256 ⇒ 1 MiB, sqz-host 1024 ⇒
        // 4 MiB). 1 MiB ⇒ hold < 512 KiB, derived ceiling = 128 KiB.
        .env("SQUEEZEFS_FUSE_MAX_WRITE", "1048576")
        // Fusion suite pins W1 HOLD/fuse, not overlay streaming-hold.
        .env("SQUEEZEFS_DEVICE_OVERLAY", "0");
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

/// Did the session actually arm FUSE_URING_ZERO_COPY? (Requires the sqz
/// kernel series + CAP_SYS_ADMIN — the arm declines loud elsewhere and
/// this suite must skip, not fail, on stock kernels/unprivileged runs.)
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
        panic!("metrics.{key} must export on an armed mount (the fusion engagement ledger)")
    })
}

/// The vehicle/venue ledger snapshot the corrected-routing asserts diff.
fn ledger(mnt: &Path) -> (u64, u64, u64, u64) {
    (
        metric(mnt, "fuse3_zc_write_fusions"),
        metric(mnt, "fuse3_zc_write_extractions"),
        metric(mnt, "fuse3_zc_write_directs"),
        metric(mnt, "fuse3_zc_write_lazy_extractions"),
    )
}

/// A test payload. The HOLD-candidate predicate keys on the FILE
/// offset/length (LBA-aligned), never the user buffer's address — FUSE
/// O_DIRECT carries byte-addressed iovecs to the daemon.
fn fill_buf(len: usize, byte: u8) -> Vec<u8> {
    vec![byte; len]
}

/// pwrite `buf` at `off` through O_DIRECT (a single kernel-lane FUSE
/// WRITE with LBA-aligned offset+len).
fn odirect_pwrite(path: &Path, off: u64, buf: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    f.write_all_at(buf, off)?;
    Ok(())
}

fn read_back(path: &Path, off: u64, len: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open for read-back");
    f.seek(SeekFrom::Start(off)).expect("seek");
    let mut out = vec![0u8; len];
    f.read_exact(&mut out).expect("read back");
    out
}

/// A durably-published striped file: buffered write + fsync so the
/// whole-block mappings exist (the W1-ELIGIBLE substrate).
fn publish_file(mnt: &Path, name: &str, len: usize, byte: u8) -> PathBuf {
    let p = mnt.join(name);
    let mut f = std::fs::File::create(&p).expect("create publish file");
    f.write_all(&vec![byte; len]).expect("prewrite");
    f.sync_all().expect("fsync prewrite");
    drop(f);
    p
}

struct Venue {
    _base: PathBuf,
    mount: Mount,
    log: PathBuf,
}

/// Format + zc-armed mount with `envs`, or skip when zc cannot arm.
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

/// Rule 1 (the eligible half): a W1-ELIGIBLE aligned small overwrite of
/// a durably-published whole-block-mapped file HOLDS, FUSES, and rides
/// the direct slot→device DMA — with the extraction vehicle NEVER
/// paying (the double-pay signature must be structurally absent on this
/// shape).
#[test]
fn w1_eligible_overwrites_fuse_without_double_pay() {
    let Some(v) = armed_venue("eligible", &[], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;

    let owfile = publish_file(mnt, "eligible.bin", 16 * 1024 * 1024, 0x11);
    let (fu0, wx0, wd0, lz0) = ledger(mnt);

    let patch = fill_buf(4096, 0x5C);
    odirect_pwrite(&owfile, 4096, &patch).expect("eligible O_DIRECT overwrite");
    assert_eq!(
        read_back(&owfile, 4096, 4096),
        patch,
        "eligible fused write must read back byte-exact"
    );
    assert_eq!(
        read_back(&owfile, 0, 4096),
        vec![0x11u8; 4096],
        "the patched block's neighbor bytes must be untouched"
    );

    let (fu1, wx1, wd1, lz1) = ledger(mnt);
    assert!(
        fu1 > fu0,
        "the W1-eligible overwrite must ride the fused lane \
         (fusions {fu0} → {fu1}; log: {})",
        v.log.display()
    );
    assert!(
        wd1 > wd0,
        "the W1 direct DMA must consume the held slot (directs {wd0} → {wd1})"
    );
    assert_eq!(
        wx1, wx0,
        "the eligible shape must never ALSO pay the extraction vehicle \
         (the double-pay signature: extractions {wx0} → {wx1})"
    );
    assert_eq!(
        lz1, lz0,
        "a fresh eligible hint must not extract late (lazy {lz0} → {lz1})"
    );
}

/// Rule 2 (the ineligible half — THE field-falsification repro): a
/// W1-INELIGIBLE small write (growth into a fresh file — the
/// `patch_ineligible_unmapped`/oversize-extend class) must extract AT
/// DELIVERY on the worker's batched pass and dispatch on the classic
/// handler lanes: extraction vehicle exactly once, fused lane NEVER.
///
/// Pre-fix this is the smoking gun in-process: the shape-only
/// `hold_candidate` holds it, the fused handler discovers ineligibility
/// and extracts LATE — fusions +1 AND extractions +1 for ONE op.
#[test]
fn ineligible_growth_writes_extract_at_delivery_and_never_fuse() {
    let Some(v) = armed_venue("ineligible", &[], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;

    let (fu0, wx0, _wd0, _lz0) = ledger(mnt);

    // Growth shape: first-touch aligned 4 KiB O_DIRECT write into a
    // fresh file — unmapped AND extending, refused by two W1 clauses.
    let hole = mnt.join("growth.bin");
    let payload = fill_buf(4096, 0xA5);
    odirect_pwrite(&hole, 0, &payload).expect("growth O_DIRECT write");
    assert_eq!(
        read_back(&hole, 0, 4096),
        payload,
        "growth write must read back byte-exact"
    );

    let (fu1, wx1, _wd1, _lz1) = ledger(mnt);
    assert!(
        wx1 > wx0,
        "the ineligible payload arrives via the at-delivery extraction \
         vehicle (extractions {wx0} → {wx1}; log: {})",
        v.log.display()
    );
    assert_eq!(
        fu1,
        fu0,
        "a W1-ineligible shape must NEVER hold/fuse — hold + fused poll + \
         late extraction is the field's 0.45× collapse (fusions {fu0} → \
         {fu1}, extractions {wx0} → {wx1}: both moving for one op IS the \
         double-pay signature; log: {})",
        v.log.display()
    );

    // The same law under load shape: a short burst of growth writes
    // keeps the ledger single-vehicle.
    let (fu2, wx2, _, _) = ledger(mnt);
    for i in 0..16u64 {
        odirect_pwrite(&hole, (i + 1) * 4096, &payload).expect("growth burst");
    }
    let (fu3, wx3, _, _) = ledger(mnt);
    assert!(
        wx3 >= wx2 + 16,
        "burst: every growth op pays the extraction vehicle once"
    );
    assert_eq!(
        fu3, fu2,
        "burst: the fused ledger stays flat on the ineligible shape"
    );
}

/// Contract 3: the fusion ceiling bounds the worker's inline work ON
/// THE ELIGIBLE POPULATION — an eligible held write above the ceiling
/// keeps the classic dispatch (still held, still direct-consumed), and
/// an explicit `SQUEEZEFS_FUSE_ZC_FUSION_MAX` wins verbatim.
#[test]
fn fusion_ceiling_bounds_inline_work_and_explicit_wins() {
    // Default ceiling (derived payload/8 = 128 KiB at the pinned 1 MiB
    // geometry): an ELIGIBLE 256 KiB overwrite holds but must NOT fuse.
    {
        let Some(v) = armed_venue("ceiling", &[], site!()) else {
            return;
        };
        let mnt = &v.mount.mnt;
        let owfile = publish_file(mnt, "ceiling.bin", 16 * 1024 * 1024, 0x22);
        let (fu0, _wx0, wd0, _lz0) = ledger(mnt);
        let payload = fill_buf(256 * 1024, 0x77);
        odirect_pwrite(&owfile, 0, &payload).expect("above-ceiling eligible write");
        assert_eq!(
            read_back(&owfile, 0, 256 * 1024),
            payload,
            "above-ceiling write byte-exact"
        );
        let (fu1, _wx1, wd1, _lz1) = ledger(mnt);
        assert_eq!(
            fu1, fu0,
            "an eligible write above the fusion ceiling keeps the classic \
             handler-lane dispatch (bounded inline work on the drain loop)"
        );
        assert!(
            wd1 > wd0,
            "…while still consuming the held slot via the direct DMA \
             (directs {wd0} → {wd1}; log: {})",
            v.log.display()
        );
    }
    // Explicit ceiling raised to 256 KiB: the same eligible shape fuses.
    {
        let Some(v) = armed_venue(
            "ceilraise",
            &[("SQUEEZEFS_FUSE_ZC_FUSION_MAX", "262144")],
            site!(),
        ) else {
            return;
        };
        let mnt = &v.mount.mnt;
        let owfile = publish_file(mnt, "ceilraise.bin", 16 * 1024 * 1024, 0x33);
        let (fu0, _, _, _) = ledger(mnt);
        let payload = fill_buf(256 * 1024, 0x88);
        odirect_pwrite(&owfile, 0, &payload).expect("at-explicit-ceiling write");
        assert_eq!(
            read_back(&owfile, 0, 256 * 1024),
            payload,
            "explicit-ceiling write byte-exact"
        );
        assert!(
            metric(mnt, "fuse3_zc_write_fusions") > fu0,
            "an explicit SQUEEZEFS_FUSE_ZC_FUSION_MAX wins verbatim \
             (256 KiB ≤ 262144 on the eligible shape must fuse; log: {})",
            v.log.display()
        );
    }
}

/// Contract 4: `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0` is the A/B control —
/// the eligible shape still holds (the D14 held path) and
/// direct-consumes, but the fused ledger never moves.
#[test]
fn fusion_lever_off_keeps_the_classic_dispatch() {
    let Some(v) = armed_venue("lever", &[("SQUEEZEFS_FUSE_ZC_WRITE_FUSION", "0")], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;
    let owfile = publish_file(mnt, "lever.bin", 16 * 1024 * 1024, 0x44);
    let (_, _, wd0, _) = ledger(mnt);

    let payload = fill_buf(4096, 0xE1);
    odirect_pwrite(&owfile, 4096, &payload).expect("lever-off eligible write");
    assert_eq!(
        read_back(&owfile, 4096, 4096),
        payload,
        "lever-off write byte-exact"
    );
    assert_eq!(
        metric(mnt, "fuse3_zc_write_fusions"),
        0,
        "SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0 must keep the fused ledger at 0 \
         (the A/B control's engagement proof)"
    );
    assert!(
        metric(mnt, "fuse3_zc_write_directs") > wd0,
        "the held slot still direct-consumes on the classic dispatch"
    );
}

/// Contract 5: bounded outcomes compose with fusion on the ELIGIBLE
/// shape — a fused write whose direct-store bridge CQE is lost (the
/// zcws-9 drop seam) resolves through the deadline ladder on the SAME
/// worker that polls the fused future. Never a self-deadlock.
#[test]
fn fused_write_lost_bridge_cqe_resolves_through_the_ladder() {
    // Two mounts over one volume: the PUBLISH mount runs seam-free (a
    // buffered prewrite's writeback rides WRITE-class extraction
    // bridges, and the drop seam would eat ITS CQE instead of the fused
    // store's); the SEAM mount then warms the metadata cache with a
    // READ (READ bridges are not WRITE-class — the seam ignores them,
    // and the hold gate needs the cached striped layout) before the
    // fused overwrite whose store CQE the seam eats.
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("ladder");
    let meta = format_volume(&base);
    let mnt_path = base.join("mnt");
    let log0 = base.join("mount-publish.log");
    {
        let mount = spawn_zc_mount(&meta, &mnt_path, &log0, &[]);
        if !zc_armed(&log0) {
            drop(mount);
            let _ = std::fs::remove_dir_all(&base);
            let _ = squeezefs_testkit::declare(
                site!(),
                squeezefs_testkit::SkipClass::Capability,
                "FUSE_URING_ZERO_COPY did not arm (sqz kernel + CAP_SYS_ADMIN required)",
            );
            return;
        }
        publish_file(&mnt_path, "ladder.bin", 16 * 1024 * 1024, 0x55);
    }
    let log = base.join("mount-seam.log");
    let mount = spawn_zc_mount(
        &meta,
        &mnt_path,
        &log,
        &[
            ("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES", "1"),
            ("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS", "1000"),
        ],
    );
    if !zc_armed(&log) {
        drop(mount);
        let _ = std::fs::remove_dir_all(&base);
        let _ = squeezefs_testkit::declare(
            site!(),
            squeezefs_testkit::SkipClass::Capability,
            "FUSE_URING_ZERO_COPY did not arm on the seam mount",
        );
        return;
    }
    let mnt = &mnt_path;
    let owfile = mnt.join("ladder.bin");
    // Warm the layout cache (the hold gate's clause-1 source) — READ
    // bridges are not WRITE-class, so the seam's drop budget survives.
    assert_eq!(
        read_back(&owfile, 0, 4096),
        vec![0x55u8; 4096],
        "published file must read back on the seam mount"
    );
    let cancels0 = metric(mnt, "fuse3_zc_bridge_cancels");

    let payload = fill_buf(4096, 0x9B);
    let writer = {
        let owfile = owfile.clone();
        std::thread::spawn(move || -> std::io::Result<()> {
            odirect_pwrite(&owfile, 4096, &payload)?;
            let f = std::fs::OpenOptions::new().write(true).open(&owfile)?;
            f.sync_all()?;
            Ok(())
        })
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !writer.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a fused write with a dropped bridge CQE did not resolve \
             within 60 s — the deadline ladder cannot reach a fused \
             waiter (self-deadlock on the worker lane; log: {})",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    match writer.join().expect("writer thread") {
        Ok(()) => {}
        Err(e) => {
            eprintln!("bounded outcome: fused write failed loud ({e}) — legal (never acked)");
        }
    }

    assert!(
        metric(mnt, "fuse3_zc_write_fusions") >= 1,
        "the lost-CQE leg must actually have ridden the fused lane"
    );
    assert!(
        metric(mnt, "fuse3_zc_bridge_cancels") > cancels0,
        "the deadline ladder must have fired for the fused waiter \
         (fuse3_zc_bridge_cancels {cancels0} → {}; log: {})",
        metric(mnt, "fuse3_zc_bridge_cancels"),
        log.display()
    );

    // Post-recovery serviceability through both routes.
    let probe = mnt.join("post_recovery.bin");
    let p = fill_buf(4096, 0x44);
    odirect_pwrite(&probe, 0, &p).expect("post-recovery write");
    assert_eq!(
        read_back(&probe, 0, 4096),
        p,
        "post-recovery mount must serve byte-exact"
    );
}
