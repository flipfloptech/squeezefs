//! statfs honesty contracts (2026-07-12, forward-only directive).
//!
//! The FUSE `statfs` reply was a hardcoded lie — ~1 PiB total/free on
//! every mount — so `df` was useless and free-space-derived logic (the
//! bench's 25% auto-sizing cap, any operator monitoring) could never
//! bind on SqueezeFS's own mounts. These tests pin the honest semantics:
//!
//! * **total** = the formatted capacity (`FormatConfig.capacity` — the
//!   sum of the data-backend capacities, or the lower explicit
//!   `--capacity` quota chosen at format). Never the fake constant.
//! * **free/avail** = total − allocated striped-block bytes, served from
//!   allocator atomics maintained at alloc/free time — statfs performs
//!   no metadata transactions and no device I/O.
//! * free **decreases** when data is written (write-through promotes
//!   complete blocks promptly) and **increases** again after
//!   delete + async reclaim.
//! * **files/ffree** = the format inode quota and the remaining headroom
//!   under the v3 monotonic (no-reuse) ino watermark — not the fake
//!   1e9 constant.
//!
//! All tests drive the real binary + a real FUSE mount and skip cleanly
//! where FUSE-over-io_uring is unavailable.

use squeezefs_testkit::{mount_supported, site};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_statfs_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn format_volume(base: &Path, data_gib: u64) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * 1024 * 1024)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(data_gib * GIB)
        .expect("size data file");
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .arg("--disk-cache-paths")
        .arg(base.join("staging"))
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

impl Drop for Mount {
    fn drop(&mut self) {
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
    }
}

fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
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
            "mount did not become ready in 90s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    mount
}

#[derive(Debug, Clone, Copy)]
struct Statvfs {
    total_bytes: u64,
    free_bytes: u64,
    avail_bytes: u64,
    files: u64,
    ffree: u64,
}

fn statvfs_of(path: &Path) -> Statvfs {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path CString");
    // SAFETY: plain-old-data out-param zeroed before the call; c_path is a
    // valid NUL-terminated C string that outlives the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    assert_eq!(
        rc,
        0,
        "statvfs({path:?}) failed: {}",
        std::io::Error::last_os_error()
    );
    let frsize = st.f_frsize as u64;
    Statvfs {
        total_bytes: (st.f_blocks as u64).saturating_mul(frsize),
        free_bytes: (st.f_bfree as u64).saturating_mul(frsize),
        avail_bytes: (st.f_bavail as u64).saturating_mul(frsize),
        files: st.f_files as u64,
        ffree: st.f_ffree as u64,
    }
}

/// Poll until `pred(statvfs)` holds or the deadline passes; returns the
/// final sample either way.
fn poll_statvfs(path: &Path, deadline: Duration, pred: impl Fn(&Statvfs) -> bool) -> Statvfs {
    let end = Instant::now() + deadline;
    loop {
        let s = statvfs_of(path);
        if pred(&s) || Instant::now() > end {
            return s;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

const FAKE_PIB: u64 = 1024 * 1024 * GIB; // the old hardcoded constant

#[test]
fn test_statfs_reports_formatted_capacity_not_fake_petabyte() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("capacity");
    let meta = format_volume(&base, 8); // 8 GiB data volume, default quotas
    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    let s = statvfs_of(&mnt);
    assert!(
        s.total_bytes < FAKE_PIB / 100,
        "total must be the formatted capacity, not the fake ~1 PiB constant \
         (got {} bytes)",
        s.total_bytes
    );
    assert!(
        s.total_bytes >= 7 * GIB && s.total_bytes <= 9 * GIB,
        "total must approximate the 8 GiB formatted capacity (got {} bytes)",
        s.total_bytes
    );
    assert!(
        s.free_bytes <= s.total_bytes && s.avail_bytes <= s.total_bytes,
        "free/avail must be bounded by total: {s:?}"
    );
    assert!(
        s.free_bytes >= s.total_bytes / 2,
        "a fresh volume must report most of its capacity free: {s:?}"
    );

    // Inode numbers come from the format quota (default 1,000,000), not
    // the old 1e9 constant.
    assert_eq!(
        s.files, 1_000_000,
        "f_files must be the format inode quota: {s:?}"
    );
    assert!(
        s.ffree > 0 && s.ffree <= s.files,
        "f_ffree must be a plausible remainder under the quota: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_statfs_free_tracks_write_then_delete_reclaim() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("tracks");
    let meta = format_volume(&base, 8);
    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    let before = statvfs_of(&mnt);

    // Write 1 GiB, durable. Complete blocks write through past staging,
    // so allocation shows up promptly (poll a deadline anyway — the tail
    // may promote via writeback).
    let file = mnt.join("statfs_probe.bin");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&file).expect("create probe file");
        let chunk = vec![0xA5u8; 4 * 1024 * 1024];
        for _ in 0..256 {
            f.write_all(&chunk).expect("write probe chunk");
        }
        f.sync_all().expect("fsync probe file");
    }
    let after_write = poll_statvfs(&mnt, Duration::from_secs(60), |s| {
        s.free_bytes + 9 * GIB / 10 <= before.free_bytes
    });
    assert!(
        after_write.free_bytes + 9 * GIB / 10 <= before.free_bytes,
        "free must drop by ~1 GiB after a durable 1 GiB write: before={} after={}",
        before.free_bytes,
        after_write.free_bytes
    );
    assert!(
        after_write.total_bytes == before.total_bytes,
        "total must not move on writes: {before:?} vs {after_write:?}"
    );

    // Delete + async reclaim: free must climb back (poll — reclaim is
    // background work).
    std::fs::remove_file(&file).expect("unlink probe file");
    let after_del = poll_statvfs(&mnt, Duration::from_secs(120), |s| {
        s.free_bytes + GIB / 10 >= before.free_bytes
    });
    assert!(
        after_del.free_bytes + GIB / 10 >= before.free_bytes,
        "free must recover after delete+reclaim: before={} after_del={}",
        before.free_bytes,
        after_del.free_bytes
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_statfs_respects_explicit_capacity_quota() {
    if !mount_supported(site!()) {
        return;
    }
    // Format with --capacity 2G on an 8 GiB physical volume: statfs must
    // reflect the EFFECTIVE limit the user experiences (the quota), not
    // the physical size.
    let base = scratch("quota");
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * 1024 * 1024)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(8 * GIB)
        .expect("size data file");
    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .arg("--capacity")
        .arg("2G")
        .arg("--disk-cache-paths")
        .arg(base.join("staging"))
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format --capacity 2G failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));
    let s = statvfs_of(&mnt);
    assert!(
        s.total_bytes >= 2 * GIB - 64 * 1024 * 1024 && s.total_bytes <= 2 * GIB,
        "total must reflect the --capacity 2G quota, not the 8 GiB physical size: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
