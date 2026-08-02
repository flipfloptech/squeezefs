//! Transport-lease watchdog vs the data plane (2026-07-28, ingest-economy
//! campaign — BLOCKING dev finding; evidence
//! `.benchmarks/2026-07-28-ingest-economy.md` §7).
//!
//! Field/gate capture (preserved: hung suite pid 179220): under a
//! saturated buffered-writeback storm, kernel-lane FUSE_WRITE handler
//! invocations legitimately exceed 1 s (write-pipeline admission waits
//! are honest backpressure; debug builds and slow substrates stretch
//! them). The §5.4 severance watchdog in `EntPayloadLease::drop`
//! (`crates/fuse3/.../fuse_over_uring.rs`) `debug_assert!`s the lease
//! age < 1 s — and a firing PANICS the handler task mid-flight:
//!
//! 1. the FUSE reply for that WRITE is never sent → the kernel's
//!    writeback folio never completes → `fsync(2)` parks in
//!    `folio_wait_writeback` in **uninterruptible D-state, forever**
//!    (the captured connection showed `waiting=28` lost requests);
//! 2. the panic unwinds PAST `state.release()` → that ring ent's
//!    COMMIT_AND_FETCH re-arm parks forever → permanent queue-depth
//!    loss on top of the wedge.
//!
//! "Passes when idle" was evidence of the race, not of health: load
//! selects the losing schedule. This test selects it deterministically
//! via the documented `SQUEEZEFS_TEST_WRITE_STALL_MS` seam (stalls the
//! write handler while the payload lease is held — the same shape a
//! parked pipeline admission produces under load).
//!
//! Contract: an overlong lease is a LOUD TRIPWIRE
//! (`transport_lease_overlong` on the stats inode + an error log), never
//! a lost reply — the write completes, fsync completes, the mount stays
//! serviceable. Red pre-fix: the daemon (debug build, so the
//! `debug_assert` is armed) panics the handler and fsync hangs to this
//! test's deadline.

use squeezefs_testkit::{mount_supported, site};
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
        .join(format!("sqfs_lease_{tag}_{}", std::process::id()));
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

struct Mount {
    child: Child,
    mnt: PathBuf,
}

/// Abort every FUSE connection with waiting (lost-reply) requests — the
/// unwedge primitive. MUST run BEFORE any umount attempt: `umount(2)`
/// syncs the superblock and parks behind the same stuck writeback the
/// lost reply created (the pre-fix hang recurses into teardown).
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
        // Post-teardown sweep: a dying pre-fix daemon can strand a fresh
        // batch of waiting requests between the first sweep and its exit.
        abort_waiting_fuse_connections();
    }
}

/// Spawn the real daemon with the write-stall seam armed: every WRITE
/// handler invocation holds its transport payload lease > 1 s — the
/// exact schedule saturated-writeback load selects.
fn spawn_stalled_mount(meta: &Path, mnt: &Path, log: &Path, stall_ms: u64) -> Mount {
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
        .env("SQUEEZEFS_TEST_WRITE_STALL_MS", stall_ms.to_string())
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

fn stats_metric(mnt: &Path, key: &str) -> Option<u64> {
    let raw = std::fs::read_to_string(mnt.join(".stats")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("metrics")?.get(key)?.as_u64()
}

/// An overlong write-handler lease must be a loud tripwire, never a lost
/// reply: buffered write + fsync against a >1 s-stalled handler completes
/// (bounded), the mount stays serviceable afterwards, and the
/// `transport_lease_overlong` tripwire accounts the firing.
#[test]
fn overlong_write_lease_is_a_tripwire_not_a_lost_reply() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("overlong");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    // 1200 ms > the watchdog's 1 s bound; the debug-build daemon
    // (debug_assertions armed) fires it on EVERY write pre-fix.
    let mount = spawn_stalled_mount(&meta, &mnt, &log, 1200);

    let file = mnt.join("probe.bin");
    // Buffered write + fsync on a worker thread with a hard deadline —
    // pre-fix this parks in fuse_fsync/folio_wait_writeback forever
    // (the captured D-state shape); the deadline turns "forever" into a
    // red assertion instead of a hung gate.
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::File::create(&file).expect("create probe file");
        f.write_all(&vec![0xC3u8; 256 * 1024])
            .expect("buffered write");
        f.sync_all().expect("fsync must complete");
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut timed_out = false;
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if timed_out {
        // Unwedge BEFORE asserting: the writer thread is in
        // uninterruptible D-state (`folio_wait_writeback`) — the test
        // process cannot even exit while it lives. Tear the mount down
        // (Mount::drop kills the daemon and aborts any waiting FUSE
        // connection), which errors the parked fsync out.
        drop(mount);
        let _ = writer.join(); // panics with the fsync EIO — expected here
        panic!(
            "write+fsync did not complete within 60 s against a stalled \
             (>1 s lease) write handler — the transport-lease watchdog \
             killed the handler and lost the FUSE reply (daemon log: {})",
            log.display()
        );
    }
    writer.join().expect("write+fsync thread");

    // The tripwire accounted the overlong lease (loud, non-fatal).
    let overlong = stats_metric(&mnt, "transport_lease_overlong").unwrap_or(0);
    assert!(
        overlong >= 1,
        "an overlong lease completed without tripping the \
         transport_lease_overlong counter — the watchdog lost its voice"
    );

    // The mount is still fully serviceable: a second write + fsync and a
    // read-back all succeed (no dead ents, no dead lanes).
    let file2 = mnt.join("probe2.bin");
    {
        let mut f = std::fs::File::create(&file2).expect("create second file");
        f.write_all(b"still alive").expect("second write");
        f.sync_all().expect("second fsync");
    }
    assert_eq!(
        std::fs::read(&file2).expect("read back"),
        b"still alive",
        "post-tripwire mount must serve reads"
    );

    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}
