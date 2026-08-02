//! L1 — transport in-flight concurrency defaults (IOPS-parity program,
//! `.benchmarks/2026-07-15-iops-parity-decomposition.md`).
//!
//! The decomposition proved two kernel-side gates multiply on iodepth
//! workloads: the FUSE-over-io_uring per-queue depth (default 4) and the
//! classical INIT reply's `max_background` (vendored-fuse3 constant 12).
//! Opening BOTH (QD32 + mb256) took the user's exact rand-4k line from
//! 44k to 316k IOPS, device-true. L1 makes that class the DEFAULT mount
//! behavior — no env knobs — under a payload-buffer sizing policy so
//! small-RAM boxes degrade gracefully instead of pinning RAM.
//!
//! Normative default policy (encoded here, implemented in
//! `crates/fuse3/src/raw/connection/fuse_over_uring.rs` +
//! `src/mem_budget.rs::transport_buffer_cap`):
//!
//! - queues = kernel possible CPUs (unchanged; kernel readiness requires
//!   every queue registered — `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` stays
//!   a testing-only override).
//! - payload buffer cap = mem_budget / 8 (no fixed ceiling — derivation
//!   sweep 2026-08-04; the structural bound is the geometry's demand cap),
//!   mem_budget resolved
//!   at mount by the §5.7 order (flag → env → cgroup×0.8 → 70 % RAM).
//! - per-queue depth: env `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` wins
//!   verbatim (clamped 1..32, bypasses the cap — explicit operator
//!   intent); otherwise depth = clamp(cap / (queues × payload_sz), 4, 32)
//!   — desired 32 (the measured 316k config), floor 4 (the pre-L1 shipped
//!   default: no box regresses below today's footprint).
//! - INIT `max_background` = `-o max_background=N` override (> 0) else
//!   clamp(queues × depth, 64, u16::MAX); `congestion_threshold` =
//!   `-o congestion_threshold=N` override (> 0) else max_background × 3/4.
//!
//! Suites (all real unprivileged mounts, kernel FUSE-over-io_uring):
//! geometry + INIT limits materialize by DEFAULT; the budget degrades
//! depth on small-RAM (simulated via `SQUEEZEFS_MEM_BUDGET_MB`); env and
//! `-o` overrides still win; the payload arena is gauged on the stats
//! inode and registered with the R5 memory authority.
//!
//! RED today: the `transport_{queues,q_depth,payload_buffer_bytes,
//! max_background}` stats fields do not exist, INIT replies
//! max_background=12 regardless of options, and depth defaults to 4.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;

/// Runtime page size (the kernel PAGE_SIZE the max_pages math uses).
fn page_size() -> u64 {
    let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    assert!(sz > 0);
    sz as u64
}

/// The kernel's advertisable max_pages ceiling — `fs.fuse.max_pages_limit`
/// (256 fallback on kernels predating the sysctl).
fn max_pages_limit() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/fuse/max_pages_limit")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(256)
        .clamp(1, u16::MAX as u64)
}

/// The normative variable-ent geometry mirror (2026-08-04 campaign —
/// fuse3 `TransportGeometry::plan`): the sandbox volumes are 4 MiB-block,
/// so the daemon desires 4 MiB max_write, gated by the live sysctl; the
/// budget ladder degrades depth first (32→4), then the payload leg
/// degrades max_write toward the 1 MiB base (yesterday's ent) only when
/// the floor-4 arena still exceeds the cap. Returns
/// `(payload_sz, depth)`.
fn expected_geometry(queues: u64, cap_bytes: u64, env_depth: Option<u64>) -> (u64, u64) {
    let limit = max_pages_limit();
    let page = page_size();
    let target = (4 * MIB).clamp(page.max(4096), limit * page);
    let pages_for = |mw: u64| mw.div_ceil(page).clamp(1, limit);
    let payload_for = |mw: u64| 8192u64.max(mw).max(pages_for(mw) * page);
    match env_depth {
        Some(d) => (payload_for(target), d.clamp(1, 32)),
        None => {
            let depth_at = |mw: u64| (cap_bytes / (queues * payload_for(mw))).clamp(4, 32);
            let depth = depth_at(target);
            let base = target.min(MIB);
            let floor_arena = queues * 4 * payload_for(target);
            if depth > 4 || floor_arena <= cap_bytes || target <= base {
                (payload_for(target), depth)
            } else {
                let fit = cap_bytes / (queues * 4);
                let mw = (fit / page * page).clamp(base, target);
                (payload_for(mw), depth_at(mw))
            }
        }
    }
}

fn transport_supported() -> bool {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("[SKIP] /dev/fuse not present");
        return false;
    }
    match std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring") {
        Ok(v)
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "y" | "1" | "yes" | "true" | "on"
            ) => {}
        other => {
            eprintln!("[SKIP] kernel fuse.enable_uring not enabled ({other:?})");
            return false;
        }
    }
    if Command::new("fusermount3").arg("-V").output().is_err() {
        eprintln!("[SKIP] fusermount3 not available");
        return false;
    }
    if !Path::new("/sys/fs/fuse/connections").is_dir() {
        eprintln!("[SKIP] fusectl not mounted at /sys/fs/fuse/connections");
        return false;
    }
    true
}

/// Kernel possible CPUs — the queue count the daemon must register
/// (affinity-independent, matches `_SC_NPROCESSORS_CONF` in the resolver).
fn possible_cpus() -> u64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    assert!(n > 0, "sysconf(_SC_NPROCESSORS_CONF) failed");
    n as u64
}

/// The normative L1 buffer-cap policy: budget / 8 — the fixed 2 GiB
/// ceiling was retired by the 2026-08-04 derivation sweep (the arena
/// bound is the geometry's structural demand cap).
fn expected_cap(budget_bytes: u64) -> u64 {
    budget_bytes / 8
}

/// The normative L1 background policy: clamp(queues × depth, 64,
/// u16::MAX) — the ceiling is the INIT-reply wire format, not a policy
/// constant (derivation sweep 2026-08-04; `-o max_background` is the
/// override); congestion threshold ¾ of it.
fn expected_background(queues: u64, depth: u64) -> (u64, u64) {
    let mb = (queues * depth).clamp(64, u16::MAX as u64);
    (mb, mb * 3 / 4)
}

struct Mount {
    child: Child,
    mnt: PathBuf,
    base: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn stats(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.mnt.join(".stats"))
            .expect("read .stats from the mounted volume");
        serde_json::from_str(&raw).expect(".stats must be valid JSON")
    }

    fn metric(&self, stats: &serde_json::Value, name: &str) -> u64 {
        stats["metrics"]
            .get(name)
            .unwrap_or_else(|| {
                panic!(
                    "stats inode missing `{name}` — the L1 transport geometry \
                     gauges are not plumbed (metrics keys = {:?})",
                    stats["metrics"].as_object().map(|o| o
                        .keys()
                        .filter(|k| k.starts_with("transport"))
                        .collect::<Vec<_>>())
                )
            })
            .as_u64()
            .unwrap_or_else(|| panic!("`{name}` not a u64"))
    }

    /// The mount's fusectl connection directory (minor of the FUSE
    /// superblock's anonymous device).
    fn fusectl_dir(&self) -> PathBuf {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(&self.mnt).expect("stat mountpoint").dev();
        let minor = libc::minor(dev);
        PathBuf::from(format!("/sys/fs/fuse/connections/{minor}"))
    }

    fn fusectl_u64(&self, name: &str) -> u64 {
        let p = self.fusectl_dir().join(name);
        std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("read {p:?}: {e}"))
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("parse {p:?}: {e}"))
    }

    fn unmount(&mut self) {
        let _ = Command::new("timeout")
            .arg("30")
            .arg("sync")
            .arg("-f")
            .arg(&self.mnt)
            .status();
        let mut clean = false;
        for _ in 0..10 {
            let st = Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .status()
                .expect("run fusermount3 -u");
            if st.success() {
                clean = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !clean {
            eprintln!(
                "[WEDGE] unmount EBUSY — detaching lazily; log tail:\n{}",
                std::fs::read_to_string(&self.log)
                    .unwrap_or_default()
                    .lines()
                    .rev()
                    .take(15)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait().expect("try_wait mount child") {
                Some(_) => break,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!("mount daemon did not exit within 30s of unmount");
                }
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Format + mount a sandbox volume with the given daemon env and extra
/// mount CLI args. Every test drives its OWN daemon process — env is
/// per-child, never process-global in the test runner.
fn mount_fs(tag: &str, envs: &[(&str, &str)], extra_args: &[&str]) -> Mount {
    let bin = env!("CARGO_BIN_EXE_squeezefs");
    let base = std::env::temp_dir().join(format!("sqfs_l1_{tag}_{}", std::process::id()));
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
    for a in extra_args {
        cmd.arg(a);
    }
    // Hygiene: the policy knobs under test must come from `envs` alone.
    cmd.env_remove("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
        .env_remove("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES")
        .env_remove("SQUEEZEFS_MEM_BUDGET_MB");
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

/// Default mount on an ample budget (64 GiB simulated): the 316k-class
/// geometry must materialize with NO env knobs — depth 32 on ≤ 64-CPU
/// boxes, INIT max_background 256 / congestion_threshold 192 — and the
/// payload arena must be gauged on the stats inode and registered with
/// the R5 memory authority as `transport_payload_buffers`.
#[test]
fn test_default_mount_transport_geometry_and_init_limits() {
    if !transport_supported() {
        return;
    }
    let budget = 64 * 1024 * MIB; // SQUEEZEFS_MEM_BUDGET_MB=65536
    let mut mount = mount_fs("default", &[("SQUEEZEFS_MEM_BUDGET_MB", "65536")], &[]);

    let q = possible_cpus();
    let (payload_sz, depth) = expected_geometry(q, expected_cap(budget), None);
    let (mb, ct) = expected_background(q, depth);

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_queues"),
        q,
        "queues must stay = kernel possible CPUs (kernel readiness)"
    );
    assert_eq!(
        mount.metric(&stats, "transport_q_depth"),
        depth,
        "default per-queue depth must follow the L1 policy (desired 32, budget-degraded)"
    );
    assert_eq!(
        mount.metric(&stats, "transport_payload_buffer_bytes"),
        q * depth * payload_sz,
        "payload arena gauge must equal queues × depth × payload_sz"
    );
    assert_eq!(
        mount.metric(&stats, "transport_max_background"),
        mb,
        "INIT max_background must follow clamp(queues × depth, 64, u16::MAX)"
    );

    // The kernel's own view — the INIT reply actually landed.
    assert_eq!(
        mount.fusectl_u64("max_background"),
        mb,
        "fusectl max_background must show the raised INIT default"
    );
    assert_eq!(
        mount.fusectl_u64("congestion_threshold"),
        ct,
        "fusectl congestion_threshold must be ¾ of max_background"
    );

    // R5 discipline: the arena rides the budget as an attribution
    // component (non-sheddable — session-lifetime registered buffers).
    let comp = &stats["metrics"]["mem_budget_components"]["transport_payload_buffers"];
    assert!(
        comp.is_object(),
        "transport_payload_buffers must be registered with the memory authority \
         (components = {})",
        stats["metrics"]["mem_budget_components"]
    );
    assert_eq!(
        comp["current"].as_u64().unwrap(),
        q * depth * payload_sz,
        "component gauge must match the arena bytes"
    );

    // Sanity: the opened-up transport still round-trips data.
    let p = mount.mnt.join("l1.txt");
    std::fs::write(&p, b"l1 transport defaults").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"l1 transport defaults");

    mount.unmount();
}

/// Small-RAM simulation (1 GiB budget): the cap (128 MiB) degrades depth
/// to the pre-L1 floor of 4 — today's shipped footprint, mount SUCCEEDS,
/// max_background scales down with delivered capacity (but never below
/// 64), and the gauge tells the truth.
#[test]
fn test_small_budget_mount_degrades_q_depth_gracefully() {
    if !transport_supported() {
        return;
    }
    let budget = 1024 * MIB; // SQUEEZEFS_MEM_BUDGET_MB=1024
    let mut mount = mount_fs("smallbox", &[("SQUEEZEFS_MEM_BUDGET_MB", "1024")], &[]);

    let q = possible_cpus();
    let cap = expected_cap(budget);
    let (payload_sz, depth) = expected_geometry(q, cap, None);
    let (mb, ct) = expected_background(q, depth);

    let stats = mount.stats();
    assert_eq!(mount.metric(&stats, "transport_q_depth"), depth);
    let arena = mount.metric(&stats, "transport_payload_buffer_bytes");
    assert_eq!(arena, q * depth * payload_sz);
    // The degradation contract (variable-ent ladder): never above the cap
    // unless the floor-of-4 × 1 MiB-base arena (yesterday's posture) IS
    // the cap violation, and never below floor 4.
    assert!(
        arena <= cap.max(q * 4 * MIB),
        "arena {arena} B exceeds the budget cap {cap} B beyond the depth-4 × base floor"
    );
    assert!(
        depth >= 4,
        "depth must never degrade below the pre-L1 default"
    );
    assert_eq!(mount.fusectl_u64("max_background"), mb);
    assert_eq!(mount.fusectl_u64("congestion_threshold"), ct);

    let p = mount.mnt.join("smallbox.txt");
    std::fs::write(&p, b"degraded but alive").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"degraded but alive");

    mount.unmount();
}

/// Existing env knob semantics are unchanged: an explicit
/// `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` wins verbatim over the budget
/// cap (operator intent), and max_background follows the delivered
/// geometry.
#[test]
fn test_env_q_depth_override_wins_over_budget() {
    if !transport_supported() {
        return;
    }
    let mut mount = mount_fs(
        "envwins",
        &[
            ("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "6"),
            ("SQUEEZEFS_MEM_BUDGET_MB", "512"), // cap 64 MiB — would force floor 4
        ],
        &[],
    );

    let q = possible_cpus();
    let (mb, ct) = expected_background(q, 6);

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_q_depth"),
        6,
        "explicit Q_DEPTH env must win over the budget cap"
    );
    assert_eq!(
        mount.metric(&stats, "transport_payload_buffer_bytes"),
        q * 6 * expected_geometry(q, 0, Some(6)).0,
        "env depth bypasses the budget; payload stays at the sysctl-gated target"
    );
    assert_eq!(mount.fusectl_u64("max_background"), mb);
    assert_eq!(mount.fusectl_u64("congestion_threshold"), ct);

    mount.unmount();
}

/// `-o max_background=` / `-o congestion_threshold=` become LIVE mount
/// overrides of the INIT reply (they were dead letters pre-L1: filtered
/// from the kernel option string and never reaching the INIT reply).
#[test]
fn test_mount_option_max_background_override() {
    if !transport_supported() {
        return;
    }
    let mut mount = mount_fs(
        "mbopt",
        &[("SQUEEZEFS_MEM_BUDGET_MB", "65536")],
        &["-o", "max_background=96,congestion_threshold=80"],
    );

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_max_background"),
        96,
        "-o max_background must override the policy default in the INIT reply"
    );
    assert_eq!(mount.fusectl_u64("max_background"), 96);
    assert_eq!(mount.fusectl_u64("congestion_threshold"), 80);

    mount.unmount();
}
