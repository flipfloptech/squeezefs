//! **Handler/worker fusion for small armed FUSE WRITEs** (zc-write-fusion
//! campaign, 2026-08-07 — ruling D16's second half: the fast-tracked fix
//! for the documented `SQUEEZEFS_FUSE_ZC` rand-4k caveat).
//!
//! The term (D14 write-side note + zcws-10 re-bracket): an armed small
//! WRITE dispatches to a foreign handler lane and then pays a
//! handler→worker→CQE→oneshot→handler round trip per slot-source
//! consumption (store OR materialize) — two cross-thread wakes and two
//! schedules per 4 KiB op, on top of the dispatch spawn itself. At
//! 99.5 % direct engagement the rand4kow row still lost ~7.6 % and the
//! hole-regime rand4k row 20.4 % (0.796×). The prior "structural"
//! verdict covered dispatching the DMA from a foreign thread
//! (SINGLE_ISSUER + per-ring bvec — still true); it never covered
//! moving the HANDLER to the ring's own thread.
//!
//! The fusion contract this suite pins (red-first):
//!
//! 1. **Engagement** — on a zc-armed mount, a small (≤ fusion ceiling)
//!    hold-candidate WRITE runs its handler ON the queue worker's fused
//!    lane: `fuse3_zc_write_fusions`/`_bytes` account it, and the
//!    write's vehicle ledger (direct or extraction) still accounts the
//!    payload exactly as before — fusion changes the VENUE, never the
//!    vehicle.
//! 2. **Correctness** — fused writes are byte-exact, buffered and
//!    O_DIRECT, hole-regime (extraction vehicle) and overwrite-regime
//!    (direct DMA vehicle), with durability via fsync (P0 smoke).
//! 3. **The ceiling** — a held write ABOVE the fusion ceiling keeps the
//!    classic handler-lane dispatch (the drain loop's inline work stays
//!    bounded: the write-bracket note's warning about moving 1 MiB
//!    memcpys onto the queue worker). Explicit
//!    `SQUEEZEFS_FUSE_ZC_FUSION_MAX` wins verbatim.
//! 4. **The lever** — `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0` keeps the
//!    pre-campaign dispatch byte-identically (`fusions` stays 0); it is
//!    the acceptance bracket's A/B control.
//! 5. **Bounded outcomes compose** — a fused write whose bridge CQE is
//!    lost (the zcws-9 seam) still resolves through the deadline ladder:
//!    the waiter is now a fused future on the SAME worker that runs the
//!    ladder, and nothing may deadlock or strand
//!    (`zc_bridge_cqe_wedge_tests`' law extended to the fused lane).

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

/// Spawn the real daemon zc-armed with per-test fusion envs.
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
        // Pin the transport payload geometry: the hold bound (payload/2)
        // and the derived fusion ceiling (payload/8) key on it, and the
        // host's fs.fuse.max_pages_limit would otherwise vary the
        // negotiated size across venues (256 ⇒ 1 MiB, sqz-host 1024 ⇒
        // 4 MiB). 1 MiB ⇒ hold < 512 KiB, derived ceiling = 128 KiB.
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

/// A test payload. The HOLD-candidate predicate keys on the FILE
/// offset/length (LBA-aligned), never the user buffer's address — FUSE
/// O_DIRECT carries byte-addressed iovecs to the daemon — so a plain
/// Vec is the right instrument here (the standing instrument-alignment
/// lesson applies to SIZES near max_pages, which this suite stays
/// far under).
fn fill_buf(len: usize, byte: u8) -> Vec<u8> {
    vec![byte; len]
}

/// pwrite `buf` at `off` through O_DIRECT (a single kernel-lane FUSE
/// WRITE with LBA-aligned offset+len — the hold-candidate shape).
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

struct Venue {
    _base: PathBuf,
    mount: Mount,
    log: PathBuf,
}

/// Format + zc-armed mount with `envs`, or skip when zc cannot arm.
/// Returns None ⇔ the test must skip (the skip is already ledgered
/// through the testkit against the CALLER's site, honouring the
/// require-mount promotion exactly like the `skip!` macro).
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

/// Contract 1+2: small hold-candidate WRITEs engage the fused lane on
/// both vehicles — the hole-regime extraction leg and the overwrite
/// direct-DMA leg — byte-exact, with the vehicle ledger unchanged.
#[test]
fn fused_small_writes_engage_on_both_vehicles_byte_exact() {
    let Some(v) = armed_venue("engage", &[], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;

    let fusions0 = metric(mnt, "fuse3_zc_write_fusions");
    let fusion_bytes0 = metric(mnt, "fuse3_zc_write_fusion_bytes");
    let extractions0 = metric(mnt, "fuse3_zc_write_extractions");

    // Leg 1 — hole regime (the 0.796× row's shape): an aligned 4 KiB
    // O_DIRECT write into a fresh file. Hold candidate → fused dispatch;
    // patch-ineligible (unmapped) → the extraction vehicle, now consumed
    // by a fused future on the worker's own lane.
    let hole = mnt.join("hole.bin");
    let payload = fill_buf(4096, 0xA5);
    odirect_pwrite(&hole, 0, &payload).expect("hole-regime O_DIRECT write");
    assert_eq!(
        read_back(&hole, 0, 4096),
        payload,
        "hole-regime fused write must read back byte-exact"
    );

    let fusions1 = metric(mnt, "fuse3_zc_write_fusions");
    assert!(
        fusions1 > fusions0,
        "a small hold-candidate WRITE must dispatch on the fused lane \
         (fuse3_zc_write_fusions {fusions0} → {fusions1}; log: {})",
        v.log.display()
    );
    assert!(
        metric(mnt, "fuse3_zc_write_extractions") > extractions0,
        "fusion changes the VENUE, never the vehicle: the hole-regime \
         payload still arrives via the extraction ledger"
    );

    // Leg 2 — overwrite regime (the rand4kow row's shape): publish a
    // striped mapping durably, then patch one aligned 4 KiB window.
    // The W1 sole-owner direct DMA must still engage — from the fused
    // venue (the D14 write-side live proof's shape, fused edition).
    let owfile = mnt.join("overwrite.bin");
    {
        let mut f = std::fs::File::create(&owfile).expect("create overwrite file");
        let body = vec![0x11u8; 16 * 1024 * 1024];
        f.write_all(&body).expect("prewrite");
        f.sync_all().expect("fsync prewrite");
    }
    let directs0 = metric(mnt, "fuse3_zc_write_directs");
    let fusions2 = metric(mnt, "fuse3_zc_write_fusions");
    let patch = fill_buf(4096, 0x5C);
    odirect_pwrite(&owfile, 4096, &patch).expect("overwrite-regime O_DIRECT patch");
    assert_eq!(
        read_back(&owfile, 4096, 4096),
        patch,
        "direct-leg fused write must read back byte-exact"
    );
    assert_eq!(
        read_back(&owfile, 0, 4096),
        vec![0x11u8; 4096],
        "the patched block's neighbor bytes must be untouched"
    );
    assert!(
        metric(mnt, "fuse3_zc_write_fusions") > fusions2,
        "the overwrite-regime small WRITE must also ride the fused lane"
    );
    assert!(
        metric(mnt, "fuse3_zc_write_directs") > directs0,
        "the W1 direct DMA must still engage from the fused venue \
         (fuse3_zc_write_directs {directs0} → {}; log: {})",
        metric(mnt, "fuse3_zc_write_directs"),
        v.log.display()
    );

    // The byte face accounts the fused payloads.
    let fusion_bytes = metric(mnt, "fuse3_zc_write_fusion_bytes");
    assert!(
        fusion_bytes >= fusion_bytes0 + 8192,
        "fuse3_zc_write_fusion_bytes must account both fused 4 KiB \
         payloads ({fusion_bytes0} → {fusion_bytes})"
    );

    // P0 smoke: durable round trip through the fused path.
    let p0 = mnt.join("p0.bin");
    let p0_payload = fill_buf(8192, 0x3D);
    odirect_pwrite(&p0, 0, &p0_payload).expect("p0 write");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&p0)
        .expect("open p0");
    f.sync_all().expect("p0 fsync");
    drop(f);
    assert_eq!(read_back(&p0, 0, 8192), p0_payload, "p0 durable read-back");
}

/// Contract 3: the fusion ceiling bounds the worker's inline work — a
/// held write ABOVE the ceiling keeps the classic handler-lane dispatch
/// (fusions unchanged), and an explicit `SQUEEZEFS_FUSE_ZC_FUSION_MAX`
/// wins verbatim (the same shape fuses when the operator raises it).
#[test]
fn fusion_ceiling_bounds_inline_work_and_explicit_wins() {
    // Default ceiling (derived payload/8 = 128 KiB at the shipped 1 MiB
    // geometry): a 256 KiB held write must NOT fuse.
    {
        let Some(v) = armed_venue("ceiling", &[], site!()) else {
            return;
        };
        let mnt = &v.mount.mnt;
        let fusions0 = metric(mnt, "fuse3_zc_write_fusions");
        let big = mnt.join("above_ceiling.bin");
        let payload = fill_buf(256 * 1024, 0x77);
        odirect_pwrite(&big, 0, &payload).expect("above-ceiling write");
        assert_eq!(
            read_back(&big, 0, 256 * 1024),
            payload,
            "above-ceiling write byte-exact"
        );
        assert_eq!(
            metric(mnt, "fuse3_zc_write_fusions"),
            fusions0,
            "a held write above the fusion ceiling must keep the classic \
             handler-lane dispatch (bounded inline work on the drain loop)"
        );
    }
    // Explicit ceiling raised to 256 KiB: the same shape fuses.
    {
        let Some(v) = armed_venue(
            "ceilraise",
            &[("SQUEEZEFS_FUSE_ZC_FUSION_MAX", "262144")],
            site!(),
        ) else {
            return;
        };
        let mnt = &v.mount.mnt;
        let fusions0 = metric(mnt, "fuse3_zc_write_fusions");
        let big = mnt.join("at_explicit_ceiling.bin");
        let payload = fill_buf(256 * 1024, 0x88);
        odirect_pwrite(&big, 0, &payload).expect("at-explicit-ceiling write");
        assert_eq!(
            read_back(&big, 0, 256 * 1024),
            payload,
            "explicit-ceiling write byte-exact"
        );
        assert!(
            metric(mnt, "fuse3_zc_write_fusions") > fusions0,
            "an explicit SQUEEZEFS_FUSE_ZC_FUSION_MAX wins verbatim \
             (256 KiB ≤ 262144 must fuse; log: {})",
            v.log.display()
        );
    }
}

/// Contract 4: `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0` is the A/B control —
/// the classic dispatch runs byte-identically and the fused ledger
/// never moves.
#[test]
fn fusion_lever_off_keeps_the_classic_dispatch() {
    let Some(v) = armed_venue("lever", &[("SQUEEZEFS_FUSE_ZC_WRITE_FUSION", "0")], site!()) else {
        return;
    };
    let mnt = &v.mount.mnt;
    let extractions0 = metric(mnt, "fuse3_zc_write_extractions");

    let f = mnt.join("classic.bin");
    let payload = fill_buf(4096, 0xE1);
    odirect_pwrite(&f, 0, &payload).expect("lever-off write");
    assert_eq!(
        read_back(&f, 0, 4096),
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
        metric(mnt, "fuse3_zc_write_extractions") > extractions0,
        "the classic dispatch still consumes the payload via extraction"
    );
}

/// Contract 5: bounded outcomes compose with fusion — a fused write
/// whose bridge CQE is lost (the zcws-9 drop seam) resolves through the
/// deadline ladder on the SAME worker that polls the fused future:
/// loud error or completed fallback within the bound, tripwires account
/// it, the mount stays serviceable. Never a self-deadlock.
#[test]
fn fused_write_lost_bridge_cqe_resolves_through_the_ladder() {
    let Some(v) = armed_venue(
        "ladder",
        &[
            ("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES", "1"),
            ("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS", "1000"),
        ],
        site!(),
    ) else {
        return;
    };
    let mnt = &v.mount.mnt;
    let log = &v.log;

    let cancels0 = metric(mnt, "fuse3_zc_bridge_cancels");

    // A fused-eligible write whose first bridge CQE the seam eats: the
    // fused future parks on the worker's own lane while the SAME worker
    // must run the deadline ladder that unparks it.
    let file = mnt.join("fused_lost_cqe.bin");
    let payload = fill_buf(4096, 0x9B);
    let writer = {
        let file = file.clone();
        std::thread::spawn(move || -> std::io::Result<()> {
            odirect_pwrite(&file, 0, &payload)?;
            let f = std::fs::OpenOptions::new().write(true).open(&file)?;
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

    // Post-recovery serviceability through the fused lane.
    let probe = mnt.join("post_recovery.bin");
    let p = fill_buf(4096, 0x44);
    odirect_pwrite(&probe, 0, &p).expect("post-recovery fused write");
    assert_eq!(
        read_back(&probe, 0, 4096),
        p,
        "post-recovery mount must serve fused writes byte-exact"
    );
}
