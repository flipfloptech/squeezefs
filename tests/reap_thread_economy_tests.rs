//! Reap-thread economy — e2e perf audit **R-4**
//! (`.benchmarks/2026-09-03-r4-reap-thread-economy.md`).
//!
//! R-2 and R-3 converged on one wall: the FUSE-over-io_uring queue
//! worker's per-op serialization, where every remaining large term of the
//! kern 4 KiB random read is a WAKE latency around a parked thread. R-4
//! puts the worker on a CPU diet (sharded engagement counters, a
//! single-read eventfd drain, a period-clocked deadline scan, a reused
//! completion list) and adds a bounded, ADAPTIVE spin-before-park
//! (`SQUEEZEFS_FUSE_IO_URING_SPIN_US`) whose laws this suite pins on a live
//! mount (mount class — self-skips through the testkit ledger; the
//! require-mount gate turns the skip into a failure):
//!
//! 1. **A quiet queue burns nothing.** On an idle mount the `fuse3-ur`
//!    CPU class stays flat and the spin ledger does not move: the spin
//!    engages only while the worker's queues hold ops in flight, and an
//!    idle worker's gap EWMA sits far past the rail.
//! 2. **The ledger closes and engages under pressure.** With an explicit
//!    cap, a concurrent O_DIRECT random-read burst moves
//!    `transport_spin_absorbed + transport_spin_expired` (spins ran) and
//!    `transport_spin_ns` (their cost), while the same burst under the
//!    `0` control leaves every spin word flat — the A/B lever is exact.
//! 3. **The commit-batch and reap-gap instruments keep closing** on the
//!    sharded counters: `transport_commit_batch_commits` accounts for
//!    every reply the burst committed, and `transport_reap_gap_ns.park`
//!    counts fewer parks than enters.
//!
//! RED against 96156b15: no `transport_spin_*` words exist on the stats
//! inode (laws 1–2), and the worker's pool-shared counters are the
//! contended process-globals the ledger convicted.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

struct Mount {
    child: Child,
    mnt: PathBuf,
    base: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn metrics(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.mnt.join(".stats")).expect("read .stats");
        let stats: serde_json::Value = serde_json::from_str(&raw).expect("stats JSON");
        stats["metrics"].clone()
    }

    fn unmount(mut self) {
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mnt)
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait().expect("try_wait mount child") {
                Some(_) => break,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!(
                        "mount daemon did not exit within 30s of unmount; log:\n{}",
                        std::fs::read_to_string(&self.log).unwrap_or_default()
                    );
                }
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn mount_fs(tag: &str, envs: &[(&str, &str)]) -> Mount {
    use std::process::{Command, Stdio};
    let bin = env!("CARGO_BIN_EXE_squeezefs");
    let base = std::env::temp_dir().join(format!("sqfs_r4_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    let staging = base.join("staging");
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&mnt).unwrap();
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(2 * 1024 * 1024 * 1024)
        .unwrap();
    let fmt = Command::new(bin)
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging)
        .output()
        .expect("run squeezefs format");
    assert!(
        fmt.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&fmt.stdout),
        String::from_utf8_lossy(&fmt.stderr)
    );
    let logf = std::fs::File::create(&log).unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string());
    cmd.env_remove("SQUEEZEFS_FUSE_IO_URING_SPIN_US");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd
        .stdout(Stdio::from(logf.try_clone().unwrap()))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mount = Mount {
        child,
        mnt,
        base,
        log,
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready in 90s; log:\n{}",
            std::fs::read_to_string(&mount.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    mount
}

fn write_file(dir: &Path, name: &str, size: usize) -> (PathBuf, Vec<u8>) {
    use std::io::Write;
    let path = dir.join(name);
    let payload: Vec<u8> = (0..size as u32)
        .map(|i| ((i * 31 + 7) % 251) as u8)
        .collect();
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(&payload).expect("write");
    f.sync_all().expect("fsync");
    (path, payload)
}

/// `n` 4 KiB O_DIRECT reads at scattered 4 KiB-aligned offsets (one
/// FUSE_READ each), content-checked. `seed` de-correlates threads.
fn odirect_reads_4k(path: &Path, payload: &[u8], n: usize, seed: usize) -> usize {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .expect("open O_DIRECT");
    let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
    // SAFETY: a fresh aligned 4 KiB allocation, written by pread before read.
    let buf = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!buf.is_null());
    let blocks = payload.len() / 4096;
    for i in 0..n {
        let b = (i * 7919 + 13 + seed * 104_729) % blocks;
        let off = (b * 4096) as i64;
        // SAFETY: buf is 4096 B aligned; the fd is open for read.
        let got = unsafe { libc::pread(f.as_raw_fd(), buf as *mut _, 4096, off) };
        assert_eq!(got, 4096, "pread at {off} returned {got}");
        let slice = unsafe { std::slice::from_raw_parts(buf, 4096) };
        assert_eq!(
            slice,
            &payload[b * 4096..(b + 1) * 4096],
            "block {b} byte-exact"
        );
    }
    // SAFETY: matches the alloc above.
    unsafe { std::alloc::dealloc(buf, layout) };
    n
}

fn u(m: &serde_json::Value, k: &str) -> u64 {
    m[k].as_u64()
        .unwrap_or_else(|| panic!("metric {k} missing/not-u64"))
}

fn spin_words(m: &serde_json::Value) -> (u64, u64, u64) {
    (
        u(m, "transport_spin_absorbed"),
        u(m, "transport_spin_expired"),
        u(m, "transport_spin_ns"),
    )
}

/// Laws 1 and 3 on the default posture: an idle mount's workers stay
/// flat and never spin; then a concurrent cold random-read burst keeps
/// the commit-batch ledger closing on the sharded counters.
#[test]
fn idle_mount_queue_workers_stay_flat_and_never_spin_then_the_ledgers_close_under_load() {
    use squeezefs_testkit::{mount_supported, site};
    if !mount_supported(site!()) {
        return;
    }
    let m = mount_fs("idle", &[]);
    // Settle: registration bursts and the first `.stats` read are over.
    std::thread::sleep(Duration::from_secs(1));
    let m0 = m.metrics();
    let ur0 = u(&m0["daemon_cpu_ns_by_class"], "fuse3-ur");
    let (a0, e0, n0) = spin_words(&m0);
    std::thread::sleep(Duration::from_secs(3));
    let m1 = m.metrics();
    let ur1 = u(&m1["daemon_cpu_ns_by_class"], "fuse3-ur");
    let (a1, e1, n1) = spin_words(&m1);
    // The two `.stats` READs bracketing the window are the only traffic:
    // every worker parked unbounded (or on its 100 ms backstop) the whole
    // time. 32 workers × 3 s of idle must not show as CPU.
    assert!(
        ur1 - ur0 < 20_000_000,
        "idle mount: fuse3-ur burned {} µs over 3 s — a quiet queue must not spin",
        (ur1 - ur0) / 1_000
    );
    assert_eq!(
        (a1 - a0, e1 - e0, n1 - n0),
        (0, 0, 0),
        "idle mount: the spin ledger must not move (no ops in flight ⇒ no window)"
    );
    // Law 3: a concurrent cold burst — every reply is one COMMIT_AND_FETCH,
    // and the sharded flush accounting must still account for each.
    let (path, payload) = write_file(&m.mnt, "cold.bin", 8 << 20);
    let m0 = m.metrics();
    let n = burst(&path, &payload, 4, 96);
    let m1 = m.metrics();
    let commits =
        u(&m1, "transport_commit_batch_commits") - u(&m0, "transport_commit_batch_commits");
    let flushes =
        u(&m1, "transport_commit_batch_flushes") - u(&m0, "transport_commit_batch_flushes");
    assert!(
        commits >= n,
        "every READ of the burst committed once: commits {commits} < reads {n}"
    );
    assert!(
        flushes >= 1 && flushes <= commits,
        "flushes {flushes} bracket commits {commits} (a flush carries ≥ 1 commit)"
    );
    let rg0 = &m0["transport_reap_gap_ns"];
    let rg1 = &m1["transport_reap_gap_ns"];
    let enters = rg1["blind"]["count"].as_u64().unwrap() - rg0["blind"]["count"].as_u64().unwrap();
    let parks = rg1["park"]["count"].as_u64().unwrap() - rg0["park"]["count"].as_u64().unwrap();
    assert!(
        enters >= parks && enters > 0,
        "reap cadence: enters {enters} ≥ parks {parks} (a park is a blocking enter)"
    );
    m.unmount();
}

/// Run `threads` concurrent O_DIRECT random-read streams of `per` reads
/// each; returns the total READ count.
fn burst(path: &Path, payload: &[u8], threads: usize, per: usize) -> u64 {
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let path = path.to_path_buf();
            let payload = payload.to_vec();
            std::thread::spawn(move || odirect_reads_4k(&path, &payload, per, t))
        })
        .collect();
    hs.into_iter().map(|h| h.join().unwrap() as u64).sum()
}

/// Law 2: an explicit cap engages the spin under a concurrent burst (the
/// operator's verbatim posture — the box gauge does not refuse it), the
/// ledger closes (`absorbed + expired` ≡ spins, `spin_ns` their cost),
/// and the `0` control leaves every spin word flat under the same burst.
#[test]
fn explicit_cap_engages_under_a_burst_and_the_zero_control_stays_flat() {
    use squeezefs_testkit::{mount_supported, site};
    if !mount_supported(site!()) {
        return;
    }
    // Explicit 200 µs cap: any short-gap park under the burst is a
    // candidate; the EWMA seeds from the burst's own parks.
    let m = mount_fs("cap", &[("SQUEEZEFS_FUSE_IO_URING_SPIN_US", "200")]);
    let (path, payload) = write_file(&m.mnt, "cold.bin", 8 << 20);
    let (a0, e0, n0) = spin_words(&m.metrics());
    // Two rounds: the first seeds every worker's gap EWMA, the second
    // runs with the window open.
    burst(&path, &payload, 8, 128);
    burst(&path, &payload, 8, 128);
    let (a1, e1, n1) = spin_words(&m.metrics());
    let spins = (a1 - a0) + (e1 - e0);
    assert!(
        spins > 0,
        "explicit cap under an 8-stream cold burst: no spin ran (absorbed {} expired {})",
        a1 - a0,
        e1 - e0
    );
    assert!(
        n1 - n0 > 0,
        "spins ran but transport_spin_ns did not move — the cost column must account for them"
    );
    assert!(
        (n1 - n0) / spins <= 200_000 + 50_000,
        "mean spin {} ns exceeds the 200 µs cap (+ one clock quantum of slack)",
        (n1 - n0) / spins
    );
    m.unmount();

    let m = mount_fs("off", &[("SQUEEZEFS_FUSE_IO_URING_SPIN_US", "0")]);
    let (path, payload) = write_file(&m.mnt, "cold.bin", 8 << 20);
    let (a0, e0, n0) = spin_words(&m.metrics());
    burst(&path, &payload, 8, 128);
    burst(&path, &payload, 8, 128);
    let (a1, e1, n1) = spin_words(&m.metrics());
    assert_eq!(
        (a1 - a0, e1 - e0, n1 - n0),
        (0, 0, 0),
        "SQUEEZEFS_FUSE_IO_URING_SPIN_US=0: the A/B control never spins"
    );
    m.unmount();
}
