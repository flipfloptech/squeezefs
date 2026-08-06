//! Transport write-geometry negotiation (fuse3 geometry + zc adoption
//! campaign, 2026-08-04; `docker/kernel-sqz/V2-CANDIDATES.md` candidate 1).
//!
//! The daemon's INIT desire is the VOLUME BLOCK SIZE (floored at the
//! 1 MiB pre-campaign shape), and the fuse3 transport negotiates it
//! against the kernel's `fs.fuse.max_pages_limit` sysctl and the payload
//! budget ladder (`TransportGeometry::plan` — the kernel REGISTER-bound
//! mirror lives in the fuse3 suite). What this file pins root-side:
//!
//! - the negotiated pair is GAUGED on the stats inode
//!   (`transport_max_write` / `transport_max_pages`) — the field 4 MiB
//!   row's engagement instrument;
//! - a default mount on a default-sysctl box lands EXACTLY today's
//!   1 MiB shape (the byte-identical contract);
//! - `SQUEEZEFS_FUSE_MAX_WRITE` overrides the desire verbatim (A/B
//!   lever for the field bracket), still sysctl-gated;
//! - THE BUG repro (privileged only): with `fs.fuse.max_pages_limit`
//!   raised past 256, the mount must SUCCEED with correctly-sized ents
//!   (pre-fix: every REGISTER refused, mount failed) and whole-block
//!   4 MiB WRITEs ride single payload leases.

use squeezefs_testkit::{mount_supported, site, skip};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const SYSCTL: &str = "/proc/sys/fs/fuse/max_pages_limit";

fn page_size() -> u64 {
    let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    assert!(sz > 0);
    sz as u64
}

/// The kernel's advertisable ceiling (256 fallback = the compiled-in
/// default of kernels predating the sysctl).
fn max_pages_limit() -> u64 {
    std::fs::read_to_string(SYSCTL)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(256)
        .clamp(1, u16::MAX as u64)
}

/// The normative negotiation mirror (fuse3 `TransportGeometry::plan`
/// composed with the daemon's desire law): desired = max(block_size,
/// 1 MiB) [env override verbatim], gated to the sysctl ceiling. The
/// budget ladder's payload leg is not exercised here (tests pass ample
/// budgets), so payload == negotiated max_write throughout.
fn expected_max_write(block_size: u64, env_desire: Option<u64>, limit: u64) -> u64 {
    let desired = env_desire.unwrap_or_else(|| block_size.max(MIB));
    let page = page_size();
    desired.clamp(page.max(4096), limit * page)
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
                    "stats inode missing `{name}` — the negotiated write-geometry \
                     gauges are not plumbed (transport keys = {:?})",
                    stats["metrics"].as_object().map(|o| o
                        .keys()
                        .filter(|k| k.starts_with("transport"))
                        .collect::<Vec<_>>())
                )
            })
            .as_u64()
            .unwrap_or_else(|| panic!("`{name}` not a u64"))
    }

    fn unmount(&mut self) {
        let _ = Command::new("timeout")
            .arg("30")
            .arg("sync")
            .arg("-f")
            .arg(&self.mnt)
            .status();
        for _ in 0..10 {
            let st = Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .status()
                .expect("run fusermount3 -u");
            if st.success() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
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
        // Drop-time double-unmount: already-unmounted is the EXPECTED case —
        // silence the mtab noise; the explicit unmount path stays loud.
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Format + mount a sandbox volume (default 4M block size unless
/// `block_size` says otherwise) with per-child env.
fn try_mount_fs(
    tag: &str,
    envs: &[(&str, &str)],
    block_size: Option<&str>,
) -> Result<Mount, String> {
    let bin = env!("CARGO_BIN_EXE_squeezefs");
    let base = std::env::temp_dir().join(format!("sqfs_geom_{tag}_{}", std::process::id()));
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

    let mut fmt_cmd = Command::new(bin);
    fmt_cmd
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging);
    if let Some(bs) = block_size {
        fmt_cmd.arg("--block-size").arg(bs);
    }
    let fmt = fmt_cmd.output().expect("run squeezefs format");
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
    cmd.env_remove("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
        .env_remove("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES")
        .env_remove("SQUEEZEFS_FUSE_MAX_WRITE")
        .env_remove("SQUEEZEFS_MEM_BUDGET_MB");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd
        .stdout(Stdio::from(logf.try_clone().unwrap()))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");

    let mut mount = Mount {
        child,
        mnt,
        base,
        log,
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            return Ok(mount);
        }
        if let Some(status) = mount.child.try_wait().ok().flatten().map(|s| s.to_string()) {
            return Err(format!(
                "mount daemon exited ({status}) before ready; log:\n{}",
                std::fs::read_to_string(&mount.log).unwrap_or_default()
            ));
        }
        if Instant::now() > deadline {
            return Err(format!(
                "mount did not become ready in 90s; log:\n{}",
                std::fs::read_to_string(&mount.log).unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn mount_fs(tag: &str, envs: &[(&str, &str)], block_size: Option<&str>) -> Mount {
    try_mount_fs(tag, envs, block_size).unwrap_or_else(|e| panic!("{e}"))
}

/// Default mount (4 MiB block size): the daemon desires block-size
/// max_write, the transport gates it by the live sysctl, and the
/// NEGOTIATED pair is gauged on the stats inode. On a default-sysctl
/// (256) box this lands exactly today's 1 MiB shape — the
/// byte-identical contract, now observable.
#[test]
fn test_default_mount_gauges_negotiated_write_geometry() {
    if !mount_supported(site!()) {
        return;
    }
    let mut mount = mount_fs("default", &[("SQUEEZEFS_MEM_BUDGET_MB", "65536")], None);

    let limit = max_pages_limit();
    let mw = expected_max_write(4 * MIB, None, limit);
    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_max_write"),
        mw,
        "negotiated max_write must be gauged (desired = block size 4 MiB, \
         gated by fs.fuse.max_pages_limit = {limit})"
    );
    assert_eq!(
        mount.metric(&stats, "transport_max_pages"),
        mw.div_ceil(page_size()),
        "advertised max_pages must describe the negotiated max_write exactly"
    );
    assert_eq!(
        mount.metric(&stats, "transport_payload_buffer_bytes"),
        mount.metric(&stats, "transport_queues") * mount.metric(&stats, "transport_q_depth") * mw,
        "ent payload size must equal the negotiated max_write (ample budget)"
    );

    // kmbuf/zc adoption gauges (2026-08-04): surfaced UNGATED. On stock
    // kernels the capability probe is Absent ⇒ negotiated must be 0 (the
    // byte-identical contract's observable); on kmbuf kernels it is 1.
    // zc_replies is structurally 0 until the staged zc arm lands.
    let kmbuf = mount.metric(&stats, "fuse3_kmbuf_negotiated");
    assert!(kmbuf <= 1, "fuse3_kmbuf_negotiated is a 0/1 level");
    assert_eq!(
        mount.metric(&stats, "fuse3_zc_replies"),
        0,
        "fuse3_zc_replies must stay 0 until the zc serve integration lands"
    );
    // zc WRITE-extraction engagement pair (write-bracket campaign,
    // 2026-08-06): the slot→memfd extraction is the armed-mount WRITE
    // vehicle; the pair must be PLUMBED (a write row without its
    // engagement face is invalid by repo law) and identically 0 on a
    // session that never armed zc.
    assert_eq!(
        mount.metric(&stats, "fuse3_zc_write_extractions"),
        0,
        "fuse3_zc_write_extractions must exist and stay 0 on an unarmed session"
    );
    assert_eq!(
        mount.metric(&stats, "fuse3_zc_write_extract_bytes"),
        0,
        "fuse3_zc_write_extract_bytes must exist and stay 0 on an unarmed session"
    );

    let p = mount.mnt.join("geom.txt");
    std::fs::write(&p, b"negotiated geometry").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"negotiated geometry");

    mount.unmount();
}

/// `SQUEEZEFS_FUSE_MAX_WRITE` overrides the desire verbatim (the field
/// bracket's A/B lever), still sysctl-gated — an explicit 1 MiB on a
/// 4 MiB-block volume restores the pre-campaign wire shape exactly.
#[test]
fn test_env_max_write_override_wins_verbatim() {
    if !mount_supported(site!()) {
        return;
    }
    let mut mount = mount_fs(
        "envmw",
        &[
            ("SQUEEZEFS_MEM_BUDGET_MB", "65536"),
            ("SQUEEZEFS_FUSE_MAX_WRITE", "1048576"),
        ],
        None,
    );

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_max_write"),
        MIB,
        "explicit SQUEEZEFS_FUSE_MAX_WRITE must override the block-size desire"
    );
    assert_eq!(
        mount.metric(&stats, "transport_max_pages"),
        MIB / page_size()
    );

    mount.unmount();
}

/// A small-block-size volume never regresses max_write below the 1 MiB
/// pre-campaign floor ("up to block size" opens the ceiling, never
/// lowers the floor).
#[test]
fn test_small_block_size_keeps_the_1mib_floor() {
    if !mount_supported(site!()) {
        return;
    }
    let mut mount = mount_fs(
        "smallbs",
        &[("SQUEEZEFS_MEM_BUDGET_MB", "65536")],
        Some("1M"),
    );

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_max_write"),
        expected_max_write(MIB, None, max_pages_limit()),
        "1 MiB-block volume keeps the 1 MiB max_write floor"
    );

    mount.unmount();
}

/// THE geometry-bug repro-port (privileged only — writing the sysctl
/// needs root; unprivileged runs SKIP):
/// `fs.fuse.max_pages_limit=1024` + a 4 MiB-block volume must mount
/// with 4 MiB ents (pre-fix: the planner kept 1 MiB ents while
/// advertising max_pages=65535, the kernel bound became 4 MiB, every
/// REGISTER refused, and this mount FAILED), and an 8 MiB whole-block
/// write must ride ≤ 4 payload leases (1 MiB max_write pays 8).
#[test]
fn test_raised_sysctl_mounts_with_4mib_ents() {
    if !mount_supported(site!()) {
        return;
    }
    let saved = std::fs::read_to_string(SYSCTL)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if saved.is_empty() {
        skip!(Capability, "{SYSCTL} not present on this kernel");
    }
    if std::fs::write(SYSCTL, "1024").is_err() {
        skip!(
            Root,
            "cannot write {SYSCTL} (need root) — run this binary under sudo"
        );
    }
    /// Restore the sysctl even on assertion panics.
    struct Restore(String);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::write(SYSCTL, &self.0);
        }
    }
    let _restore = Restore(saved);

    let mut mount = mount_fs("sysctl1024", &[("SQUEEZEFS_MEM_BUDGET_MB", "65536")], None);

    let stats = mount.stats();
    assert_eq!(
        mount.metric(&stats, "transport_max_write"),
        4 * MIB,
        "raised sysctl must open the block-size max_write"
    );
    assert_eq!(mount.metric(&stats, "transport_max_pages"), 1024);
    let q = mount.metric(&stats, "transport_queues");
    let d = mount.metric(&stats, "transport_q_depth");
    assert_eq!(
        mount.metric(&stats, "transport_payload_buffer_bytes"),
        q * d * 4 * MIB,
        "ents must be 4 MiB (the kernel REGISTER bound at limit 1024)"
    );

    // Whole-block engagement: 8 MiB written + fsynced arrives in ≤ 4
    // FUSE_WRITEs (kernel writeback gathers to max_pages) — the 1 MiB
    // pre-campaign shape pays 8. Leases count exactly the FUSE_WRITE
    // deliveries on the armed transport.
    let leases_before = mount.metric(&mount.stats(), "transport_payload_leases");
    let p = mount.mnt.join("block.bin");
    let payload = vec![0xa5u8; 8 * MIB as usize];
    std::fs::write(&p, &payload).unwrap();
    let f = std::fs::File::open(&p).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let leases_after = mount.metric(&mount.stats(), "transport_payload_leases");
    let writes = leases_after - leases_before;
    assert!(
        (2..=4).contains(&writes),
        "8 MiB must arrive as 2–4 whole/near-whole-block WRITEs, got {writes} \
         (8 = the 1 MiB max_write shape — negotiation not engaged)"
    );
    assert_eq!(std::fs::read(&p).unwrap(), payload, "round-trip integrity");

    mount.unmount();
}
