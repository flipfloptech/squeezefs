//! Pre-RC POSIX semantics through a REAL mount (spec §5 POSIX-1/2/3).
//!
//! `tests/posix_semantics_tests.rs` pins these contracts at the FUSE-op
//! boundary in-process; this suite proves the same three reach USERSPACE
//! over `/dev/fuse` — the syscalls the tools actually issue:
//!
//! * `lseek(SEEK_HOLE)` / `lseek(SEEK_DATA)` and `stat.st_blocks` on a
//!   sparse file (`cp --sparse`, `tar -S`, `rsync -S`, `qemu-img`);
//! * `statvfs.f_ffree` across a create/delete loop (`df -i`, and every
//!   installer that gates on `IUse%`);
//! * `lstat(symlink).st_size` AFTER the kernel attr TTL lapses — the
//!   window in-process tests and conformance suites both miss.
//!
//! Skips cleanly where FUSE-over-io_uring is unavailable, exactly like
//! `tests/statfs_tests.rs` (whose mount scaffolding this mirrors).

use squeezefs_testkit::{mount_supported, site};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// The daemon's default striped block size — the lseek/st_blocks grain.
const BLOCK: u64 = 4 * MIB;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_posix_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn format_volume(base: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * MIB)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(8 * GIB)
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

/// `lseek(2)` straight through libc — no std wrapper hides the whence.
fn lseek(fd: libc::c_int, off: i64, whence: libc::c_int) -> Result<i64, i32> {
    // SAFETY: `fd` is a live descriptor owned by the caller; lseek has no
    // memory effects.
    let r = unsafe { libc::lseek(fd, off, whence) };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r)
    }
}

fn statvfs_ffree(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path CString");
    // SAFETY: plain-old-data out-param zeroed before the call; c_path is a
    // valid NUL-terminated C string that outlives the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs failed: {}", std::io::Error::last_os_error());
    st.f_ffree as u64
}

/// POSIX-2 end to end: a sparse file written through the mount answers
/// `SEEK_HOLE`/`SEEK_DATA` at its real block boundaries, and `stat(2)`
/// reports the allocated blocks — the two signals every sparse-aware
/// copier consults.
#[test]
fn mount_exports_holes_to_lseek_and_st_blocks() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("sparse");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    // Blocks 0 and 3 written, 1 and 2 never touched: the `dd seek=` shape.
    let file = mnt.join("sparse.img");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(&file).expect("create sparse file");
        f.write_all(&vec![0xA5u8; BLOCK as usize])
            .expect("write block 0");
        f.seek(SeekFrom::Start(3 * BLOCK)).expect("seek to block 3");
        f.write_all(&vec![0x5Au8; BLOCK as usize])
            .expect("write block 3");
        f.sync_all().expect("fsync");
    }

    let f = std::fs::File::open(&file).expect("reopen sparse file");
    let fd = {
        use std::os::unix::io::AsRawFd;
        f.as_raw_fd()
    };
    let size = f.metadata().expect("stat").len();
    assert_eq!(size, 4 * BLOCK, "sparse write must extend the size");

    assert_eq!(lseek(fd, 0, libc::SEEK_DATA), Ok(0), "block 0 is data");
    assert_eq!(
        lseek(fd, 0, libc::SEEK_HOLE),
        Ok(BLOCK as i64),
        "the hole starts at block 1 — a kernel ENOSYS fallback answers EOF"
    );
    assert_eq!(
        lseek(fd, BLOCK as i64, libc::SEEK_DATA),
        Ok(3 * BLOCK as i64),
        "the next data after the hole is block 3"
    );
    assert_eq!(
        lseek(fd, 3 * BLOCK as i64, libc::SEEK_HOLE),
        Ok(4 * BLOCK as i64),
        "the implicit EOF hole terminates the last data run"
    );
    assert_eq!(
        lseek(fd, 4 * BLOCK as i64, libc::SEEK_DATA),
        Err(libc::ENXIO),
        "at/after EOF is ENXIO"
    );

    let blocks = f.metadata().expect("stat").blocks();
    assert_eq!(
        blocks,
        2 * BLOCK / 512,
        "st_blocks must count the 2 allocated blocks — `cp --sparse=auto` \
         only looks for holes when st_blocks*512 < st_size (size-derived \
         would be {})",
        4 * BLOCK / 512
    );
    drop(f);
    let _ = std::fs::remove_dir_all(&base);
}

/// POSIX-1 end to end: `df -i`'s IUsed must come back down. Before the
/// live gauge it derived from the monotonic ino watermark, so a
/// create/delete loop walked a fresh filesystem to "full".
#[test]
fn mount_statvfs_ifree_recovers_after_create_delete_loop() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("ifree");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    let before = statvfs_ffree(&mnt);
    const N: usize = 64;
    for i in 0..N {
        std::fs::write(mnt.join(format!("churn_{i}")), b"x").expect("create");
    }
    let peak = statvfs_ffree(&mnt);
    assert!(
        peak <= before - N as u64,
        "f_ffree must drop while the files exist (before {before}, peak {peak})"
    );
    for i in 0..N {
        std::fs::remove_file(mnt.join(format!("churn_{i}"))).expect("unlink");
    }

    // Reclaim is FORGET-driven background work: poll a deadline.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut after = statvfs_ffree(&mnt);
    while after < before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        after = statvfs_ffree(&mnt);
    }
    assert_eq!(
        after, before,
        "f_ffree must recover after the deletes are reclaimed \
         (before {before}, peak {peak}, after {after})"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// POSIX-3 end to end: `lstat()` on a symlink still reports the target
/// length once the kernel's 1 s attr TTL has lapsed and the stat reaches
/// the daemon's durable record. This is the window conformance suites
/// miss — they stat inside it.
#[test]
fn mount_symlink_size_is_correct_after_the_attr_ttl_lapses() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("symlink");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let _mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    let target = "/var/lib/squeezefs/a/reasonably/long/target/path/file.dat";
    let link = mnt.join("link");
    std::os::unix::fs::symlink(target, &link).expect("symlink");

    let fresh = std::fs::symlink_metadata(&link).expect("lstat").len();
    assert_eq!(fresh, target.len() as u64, "in-window lstat sizes the link");

    // KEPT sleep — TEST-3 class "product time behavior under test": the
    // 1 s kernel attr TTL expiring is the stimulus (past it the kernel
    // re-asks the daemon, which answers from the durable inode record).
    // Minimal: 1.5 s is the TTL plus a 500 ms scheduling margin.
    std::thread::sleep(Duration::from_millis(1500));
    let aged = std::fs::symlink_metadata(&link).expect("lstat").len();
    assert_eq!(
        aged,
        target.len() as u64,
        "post-TTL lstat must still size the link — a readlink() buffer \
         sized from st_size == 0 records an EMPTY target"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("readlink"),
        PathBuf::from(target)
    );
    let _ = std::fs::remove_dir_all(&base);
}
