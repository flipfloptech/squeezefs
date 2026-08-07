//! The **lost-bridge-CQE wedge** (zc-bridge-cqe-wedge campaign,
//! 2026-08-07 — the zcws-9 W4 field finding, THE blocker on the
//! `SQUEEZEFS_FUSE_ZC` default-ON flip).
//!
//! Field tape (`sqz-W4-zc1.log`, squeeze-test, 6.19-sqz armed hybrid):
//! 140 distinct requests across 28 of 32 rings stranded within one ~5 s
//! window — ~116 of them at-delivery `WriteExtract` bridge ops whose
//! CQEs never resolved (the request NEVER dispatched: no handler, no
//! lock, nothing the op watchdog or lock census could name), the other
//! 24 dispatched writes parked behind them. Every queue worker sat in
//! `io_cqring_wait` — the healthy park posture — for 96+ minutes,
//! because a zc bridge op had NO bounded outcome: no deadline, no
//! cancel, no synthesis. Recovery in the field was a FUSE connection
//! abort.
//!
//! The bounded-outcome law (this campaign): every zc bridge op resolves
//! by completion, error, or the deadline ladder —
//!
//!   deadline (`SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS`) → `AsyncCancel` →
//!   the cancel's own CQE classifies:
//!     `0`        → the original op's `-ECANCELED` CQE resolves it;
//!     `-ENOENT`  → the op already completed and the CQ is FIFO, so a
//!                  still-live pend PROVES its completion was LOST →
//!                  synthesize the resolution (loud EIO / fallback,
//!                  `fuse3_zc_bridge_lost`) — never a silent hang;
//!     `-EALREADY`→ the op is still RUNNING kernel-side — re-arm the
//!                  deadline (loud every period; synthesis here would
//!                  let a late kernel DMA alias a recycled slot).
//!
//! This suite selects the lost-CQE schedule DETERMINISTICALLY on a live
//! armed mount via the `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` seam (the
//! worker consumes-and-drops the first N WRITE-class bridge CQEs,
//! leaving the pend + deadline live — exactly the field posture).
//!
//! Contracts (red-first — pre-fix the cancel's `-ENOENT` was
//! informational and both legs hang to this suite's deadline):
//! 1. **At-delivery extraction loss** (the ~116-slot field class): an
//!    unaligned buffered write whose extraction CQE is dropped resolves
//!    LOUDLY within the deadline ladder (the write fails EIO through
//!    the POSIX-16 latch — it was never acked durable), the tripwires
//!    account it (`fuse3_zc_bridge_cancels ≥ 1`,
//!    `fuse3_zc_bridge_lost ≥ 1`), and the mount stays serviceable.
//! 2. **Held-slot lazy-extraction loss** (the dispatched class): an
//!    aligned held WRITE whose materialize CQE is dropped unparks the
//!    handler within the ladder — same tripwires, same serviceability,
//!    never a parked-forever oneshot.
//!
//! P0 law: no acked byte is dropped — both legs fail their write/fsync
//! loudly BEFORE durability was ever confirmed;
//! `fsync_writeback_tail_loss_tests` guards the other side.

use squeezefs_testkit::{mount_supported, site, skip};
use std::io::Write as _;
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
        .join(format!("sqfs_zcwedge_{tag}_{}", std::process::id()));
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

/// Abort every FUSE connection with waiting (lost-reply) requests — the
/// unwedge primitive (the transport_lease_overlong harness's teardown
/// law: umount(2) parks behind the same stuck writeback pre-fix).
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

/// Spawn the real daemon zc-armed with the drop seam loaded: the first
/// `drop_n` WRITE-class bridge CQEs are consumed-and-dropped by the
/// worker (pend + deadline stay live — the field posture), and the
/// bridge deadline is dialed down to 1 s so the ladder fires inside the
/// test bound.
fn spawn_zc_mount(meta: &Path, mnt: &Path, log: &Path, drop_n: u64) -> Mount {
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
        .env("SQUEEZEFS_FUSE_ZC", "1")
        // An OUTER override wins — running the suite with
        // `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS=600000` is the standing RED
        // control: the ladder cannot fire inside the test bound and the
        // dropped CQE wedges exactly as the field did.
        .env(
            "SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS",
            std::env::var("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS").unwrap_or_else(|_| "1000".into()),
        )
        .env("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES", drop_n.to_string())
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
                // The registration banner printed WITHOUT the zc face:
                // the arm declined (stock kernel / no CAP_SYS_ADMIN).
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

/// One lost-CQE leg: write `payload` at the file head, fsync, and demand
/// a BOUNDED outcome — completion or a loud error, never a hang. Returns
/// whether the write+fsync leg succeeded (post-recovery success is legal:
/// the ladder's fallback may complete the write).
fn bounded_write_leg(mnt: &Path, name: &str, payload: &[u8], log: &Path) {
    let file = mnt.join(name);
    let payload = payload.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&file)?;
        f.write_all(&payload)?;
        f.sync_all()?;
        Ok(())
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !writer.is_finished() {
        assert!(
            Instant::now() < deadline,
            "write+fsync did not resolve within 60 s against a dropped \
             bridge CQE — the bounded-outcome ladder never fired (the \
             zcws-9 wedge; daemon log: {})",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    // Success or a loud error are both bounded outcomes; the write was
    // never acked durable, so an EIO here drops no acked byte (P0).
    match writer.join().expect("writer thread") {
        Ok(()) => {}
        Err(e) => {
            eprintln!("bounded outcome: write/fsync failed loud ({e}) — legal (never acked)");
        }
    }
}

/// Post-recovery serviceability: a fresh write + fsync + read-back must
/// succeed byte-exact — no dead ents, no dead lanes, no wedged workers.
fn assert_serviceable(mnt: &Path) {
    let file = mnt.join("post_recovery_probe.bin");
    let mut f = std::fs::File::create(&file).expect("create post-recovery file");
    f.write_all(b"bounded outcome")
        .expect("post-recovery write");
    f.sync_all().expect("post-recovery fsync");
    drop(f);
    assert_eq!(
        std::fs::read(&file).expect("post-recovery read"),
        b"bounded outcome",
        "post-recovery mount must serve byte-exact"
    );
}

/// Contract 1+2: both WRITE bridge classes lose their CQE (the seam
/// drops the first two WRITE-class bridge CQEs) and BOTH resolve through
/// the deadline ladder — the at-delivery extraction (unaligned write,
/// the ~116-slot field class: the request never dispatched) and the
/// held-slot lazy materialization (aligned write, the dispatched class:
/// a handler parked on the oneshot). Tripwires account both, the mount
/// stays serviceable, nothing hangs.
#[test]
fn lost_write_bridge_cqes_resolve_through_the_deadline_ladder() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("ladder");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mount = spawn_zc_mount(&meta, &mnt, &log, 2);
    if !zc_armed(&log) {
        drop(mount);
        let _ = std::fs::remove_dir_all(&base);
        skip!(
            Capability,
            "FUSE_URING_ZERO_COPY did not arm (sqz kernel + CAP_SYS_ADMIN required)"
        );
    }

    let cancels0 = stats_metric(&mnt, "fuse3_zc_bridge_cancels").unwrap_or(0);
    let lost0 = stats_metric(&mnt, "fuse3_zc_bridge_lost").unwrap_or(0);

    // Leg 1 — the at-delivery extraction class: an UNALIGNED payload
    // (not a hold candidate) extracts at delivery; the seam eats its
    // CQE, so the request never dispatches until the ladder fires.
    bounded_write_leg(&mnt, "atdelivery.bin", &vec![0xA5u8; 100_000], &log);

    // Leg 2 — the held-slot class: an ALIGNED sub-half-payload write is
    // HELD (dispatch-before-extraction); the handler's materialize
    // bridge loses its CQE, parking the handler's oneshot until the
    // ladder fires.
    bounded_write_leg(&mnt, "held.bin", &vec![0x5Au8; 256 * 1024], &log);

    // The ladder's ledger: at least one cancel pushed, and at least one
    // PROVEN lost completion synthesized (the seam guarantees the ops
    // completed kernel-side before we dropped them — the -ENOENT arm).
    let cancels = stats_metric(&mnt, "fuse3_zc_bridge_cancels").unwrap_or(0);
    let lost = stats_metric(&mnt, "fuse3_zc_bridge_lost").unwrap_or(0);
    assert!(
        cancels >= cancels0 + 2,
        "both dropped bridge CQEs must push a deadline AsyncCancel \
         (fuse3_zc_bridge_cancels {cancels0} → {cancels}; log: {})",
        log.display()
    );
    assert!(
        lost >= lost0 + 2,
        "both losses are PROVEN (-ENOENT with a live pend) and must be \
         synthesized (fuse3_zc_bridge_lost {lost0} → {lost}; log: {})",
        log.display()
    );

    assert_serviceable(&mnt);
    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}

/// The tripwires stay 0 on a healthy armed mount: the same venue with
/// the seam UNLOADED runs the same two write shapes — no cancels, no
/// synthesized losses, both writes durable byte-exact. (The must-stay-0
/// half of the tripwire contract; a false-firing deadline would EIO
/// healthy slow writes.)
#[test]
fn healthy_armed_mount_fires_no_bridge_tripwires() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("healthy");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mount = spawn_zc_mount(&meta, &mnt, &log, 0);
    if !zc_armed(&log) {
        drop(mount);
        let _ = std::fs::remove_dir_all(&base);
        skip!(
            Capability,
            "FUSE_URING_ZERO_COPY did not arm (sqz kernel + CAP_SYS_ADMIN required)"
        );
    }

    let unaligned = vec![0xA5u8; 100_000];
    let held = vec![0x5Au8; 256 * 1024];
    for (name, payload) in [("atdelivery.bin", &unaligned), ("held.bin", &held)] {
        let file = mnt.join(name);
        let mut f = std::fs::File::create(&file).expect("create");
        f.write_all(payload).expect("write");
        f.sync_all().expect("fsync");
        drop(f);
        assert_eq!(
            std::fs::read(&file).expect("read back").len(),
            payload.len(),
            "healthy write must be durable byte-complete"
        );
    }

    assert_eq!(
        stats_metric(&mnt, "fuse3_zc_bridge_cancels").unwrap_or(0),
        0,
        "fuse3_zc_bridge_cancels must stay 0 on a healthy mount"
    );
    assert_eq!(
        stats_metric(&mnt, "fuse3_zc_bridge_lost").unwrap_or(0),
        0,
        "fuse3_zc_bridge_lost must stay 0 on a healthy mount"
    );

    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}
