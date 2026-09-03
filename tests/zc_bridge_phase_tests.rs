//! The zc direct-leg bridge decomposition on a LIVE zc-armed mount
//! (e2e audit R-3, `docs/design-e2e-perf-audit.md` §3.3 read #3 in its
//! zc-leg form; `.benchmarks/2026-09-03-4k-random-attribution.md` §8
//! instrument gap 2). The kern rand-4k READ's `keys_resolved →
//! block_fetched` was ONE opaque stage of 166 µs with the device at 40:
//! the four cross-thread hops around the DMA had no per-op stamps and no
//! histogram. This suite pins the instrument on the real bridge — the
//! fuse3 queue worker's `READ_FIXED(device → slot)` — which only the
//! sqz kernel (FUSE_URING_ZERO_COPY, CAP_SYS_ADMIN) can arm, so it is a
//! capability-class suite (`tests/run_zc_capability_gate.sh`).
//!
//! Contracts:
//! 1. **Engagement + count law**: every cold 4 KiB-aligned O_DIRECT read
//!    rides the zc leg (`fuse3_zc_replies` Δ = reads) and moves EVERY
//!    `zc_bridge_phase_ns` phase by exactly one sample.
//! 2. **Exact containment**: `Σ sum_ns(msg_hop, sq_wait, device_cq,
//!    wake_hop) == sum_ns(total)` to the ns — the four hops share one
//!    clock read at each boundary, so the decomposition is a partition,
//!    never an approximation.
//! 3. **The per-op chain**: a traced read's `.trace` chain carries
//!    `bridge_sent ≤ bridge_taken ≤ dev_submit ≤ dev_complete ≤
//!    block_fetched` in that order between `keys_resolved` and
//!    `read_validated`, and the chain's spans agree with the histogram
//!    means (the stitch tool's containment law, checked here in-process).

use squeezefs_testkit::{mount_supported, site};
use std::io::Write as _;
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
        .join(format!("sqfs_zcbridge_{tag}_{}", std::process::id()));
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

/// Spawn the real daemon zc-armed with per-test extra envs.
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

/// Did the session arm FUSE_URING_ZERO_COPY? (sqz kernel + CAP_SYS_ADMIN;
/// elsewhere this suite skips, class=capability.)
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

fn stats(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".stats")).expect("read .stats");
    let v: serde_json::Value = serde_json::from_str(&raw).expect(".stats JSON");
    v["metrics"].clone()
}

fn word(m: &serde_json::Value, key: &str) -> u64 {
    m[key]
        .as_u64()
        .unwrap_or_else(|| panic!("metrics.{key} must export as a u64"))
}

/// `(count, sum_ns)` of one phase of an exact-sum family.
fn phase(m: &serde_json::Value, family: &str, phase: &str) -> (u64, u64) {
    let h = &m[family][phase];
    (
        h["count"]
            .as_u64()
            .unwrap_or_else(|| panic!("{family}.{phase}.count")),
        h["sum_ns"]
            .as_u64()
            .unwrap_or_else(|| panic!("{family}.{phase}.sum_ns")),
    )
}

const BRIDGE_PHASES: [&str; 4] = ["msg_hop", "sq_wait", "device_cq", "wake_hop"];

/// A durably-published file of `len` bytes with a per-4 KiB pattern.
fn publish_file(mnt: &Path, name: &str, len: usize) -> PathBuf {
    let p = mnt.join(name);
    let mut f = std::fs::File::create(&p).expect("create publish file");
    let mut buf = vec![0u8; len];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((i / 4096) as u8) ^ 0x5A;
    }
    f.write_all(&buf).expect("prewrite");
    f.sync_all().expect("fsync prewrite");
    drop(f);
    p
}

/// One 4 KiB O_DIRECT pread at `off` (a single kernel-lane FUSE READ
/// whose window is 4 KiB-aligned: the zc leg's geometry gate).
fn odirect_pread_4k(path: &Path, off: u64) -> Vec<u8> {
    use std::os::unix::fs::FileExt as _;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .expect("open O_DIRECT");
    // Page-aligned user buffer (the O_DIRECT contract).
    let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
    // SAFETY: a fresh 4 KiB page-aligned allocation, fully written by the
    // read below before it is copied out; freed at the end of this fn.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!ptr.is_null());
    let out = {
        // SAFETY: `ptr` is valid for 4096 bytes for the life of this block.
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, 4096) };
        f.read_exact_at(buf, off).expect("O_DIRECT pread");
        buf.to_vec()
    };
    // SAFETY: allocated above with the same layout, not used afterwards.
    unsafe { std::alloc::dealloc(ptr, layout) };
    out
}

/// One traced op's `(mono_ns, stage_name)` chain from a `.trace` dump.
fn chains(trace: &serde_json::Value) -> std::collections::BTreeMap<u64, Vec<(u64, String)>> {
    let names: std::collections::HashMap<u64, String> = trace["stages"]
        .as_object()
        .expect("stages table")
        .iter()
        .map(|(id, name)| {
            (
                id.parse::<u64>().unwrap(),
                name.as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut out: std::collections::BTreeMap<u64, Vec<(u64, String)>> = Default::default();
    for s in trace["samples"].as_array().expect("samples") {
        let row = s.as_array().unwrap();
        let (op, stage, ns) = (
            row[0].as_u64().unwrap(),
            row[1].as_u64().unwrap(),
            row[2].as_u64().unwrap(),
        );
        out.entry(op).or_default().push((ns, names[&stage].clone()));
    }
    for c in out.values_mut() {
        c.sort();
    }
    out
}

fn stage_ns(chain: &[(u64, String)], name: &str) -> Option<u64> {
    chain.iter().find(|(_, n)| n == name).map(|(ns, _)| *ns)
}

#[test]
fn cold_zc_reads_decompose_into_four_exact_hops_with_a_per_op_chain() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("hops");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    const READS: u64 = 256;
    const FILE_LEN: usize = 4096 * READS as usize * 2;

    // Mount 1: publish the file (the RAM tiers of THIS daemon hold it;
    // the mount that reads must be a fresh one so every read is cold).
    {
        let log0 = base.join("mount-publish.log");
        let mount = spawn_zc_mount(&meta, &mnt, &log0, &[]);
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
        publish_file(&mnt, "cold.bin", FILE_LEN);
    }

    // Mount 2: cold, zc-armed, op-trace armed.
    let log = base.join("mount-read.log");
    let mount = spawn_zc_mount(&meta, &mnt, &log, &[("SQUEEZEFS_OP_TRACE", "1")]);
    assert!(zc_armed(&log), "the second mount arms zc like the first");
    let path = mnt.join("cold.bin");
    let pre = stats(&mnt);
    assert_eq!(
        pre["op_trace_armed"].as_bool(),
        Some(true),
        "SQUEEZEFS_OP_TRACE=1 arms the ring"
    );

    // Distinct 4 KiB windows, each block's first touch: cold by
    // construction, every read a zc fetch.
    for i in 0..READS {
        let off = i * 2 * 4096;
        let got = odirect_pread_4k(&path, off);
        let want = ((off / 4096) as u8) ^ 0x5A;
        assert!(
            got.iter().all(|b| *b == want),
            "read {i} at {off}: device bytes are the file's bytes"
        );
    }
    let post = stats(&mnt);

    // 1. Engagement + the count law. `read_zc_serve_bytes` is the exact
    // direct-leg instrument; `fuse3_zc_replies` also counts the bounce
    // bridge every paged reply on a zc session rides (the `.stats` read
    // whose commit lands after `pre` was built), so it bounds from below.
    assert_eq!(
        word(&post, "read_zc_serve_bytes") - word(&pre, "read_zc_serve_bytes"),
        READS * 4096,
        "every cold aligned read rode the zc leg"
    );
    assert!(word(&post, "fuse3_zc_replies") - word(&pre, "fuse3_zc_replies") >= READS);
    let (tot_n0, tot_s0) = phase(&pre, "zc_bridge_phase_ns", "total");
    let (tot_n1, tot_s1) = phase(&post, "zc_bridge_phase_ns", "total");
    assert_eq!(tot_n1 - tot_n0, READS, "one `total` sample per zc fetch");
    let mut hop_sum = 0u64;
    for p in BRIDGE_PHASES {
        let (n0, s0) = phase(&pre, "zc_bridge_phase_ns", p);
        let (n1, s1) = phase(&post, "zc_bridge_phase_ns", p);
        assert_eq!(n1 - n0, READS, "one `{p}` sample per zc fetch");
        hop_sum += s1 - s0;
    }
    // 2. Exact containment: the hops partition the total to the ns.
    assert_eq!(
        hop_sum,
        tot_s1 - tot_s0,
        "Σ(msg_hop, sq_wait, device_cq, wake_hop) ≡ total — one clock read per boundary"
    );
    // The handler's `block_fetch` contains the bridge total (its t0 is
    // the pre-send probe instant, a few hundred ns earlier).
    let (bf_n0, bf_s0) = phase(&pre, "read_serve_phase_ns", "block_fetch");
    let (bf_n1, bf_s1) = phase(&post, "read_serve_phase_ns", "block_fetch");
    assert_eq!(bf_n1 - bf_n0, READS);
    assert!(
        bf_s1 - bf_s0 >= tot_s1 - tot_s0,
        "block_fetch ⊇ bridge total (handler-side)"
    );

    // 3. The per-op chain (the ring samples 1 in `divisor`; every
    // sampled read must carry the whole ordered bridge chain).
    let raw = std::fs::read_to_string(mnt.join(".trace")).expect("read .trace");
    let trace: serde_json::Value = serde_json::from_str(&raw).expect(".trace JSON");
    let divisor = trace["divisor"].as_u64().expect("divisor");
    let all = chains(&trace);
    let bridged: Vec<&Vec<(u64, String)>> = all
        .values()
        .filter(|c| stage_ns(c, "bridge_sent").is_some())
        .collect();
    assert!(
        !bridged.is_empty(),
        "{READS} reads at divisor {divisor} must sample at least one bridge chain \
         ({} ops traced)",
        all.len()
    );
    const ORDER: [&str; 7] = [
        "keys_resolved",
        "bridge_sent",
        "bridge_taken",
        "dev_submit",
        "dev_complete",
        "block_fetched",
        "read_validated",
    ];
    let mut span_sums = [0u64; 4];
    for c in &bridged {
        let ns: Vec<u64> = ORDER
            .iter()
            .map(|s| stage_ns(c, s).unwrap_or_else(|| panic!("chain lacks {s}: {c:?}")))
            .collect();
        assert!(
            ns.windows(2).all(|w| w[0] <= w[1]),
            "bridge chain is ordered: {c:?}"
        );
        for (i, acc) in span_sums.iter_mut().enumerate() {
            // bridge_sent→bridge_taken→dev_submit→dev_complete→block_fetched
            *acc += ns[i + 2] - ns[i + 1];
        }
    }
    // The chain's spans vs the histograms' means (the stitch tool's
    // containment law): exact for the first three (shared clock reads);
    // `wake_hop`'s chain end is `block_fetched`, read a few hundred ns
    // after the handler's own `wake_hop` end, so it may only be LONGER.
    let n = bridged.len() as u64;
    for (i, p) in BRIDGE_PHASES.iter().enumerate() {
        let (n0, s0) = phase(&pre, "zc_bridge_phase_ns", p);
        let (n1, s1) = phase(&post, "zc_bridge_phase_ns", p);
        let hist_mean = (s1 - s0) as f64 / (n1 - n0) as f64;
        let trace_mean = span_sums[i] as f64 / n as f64;
        let ratio = trace_mean / hist_mean.max(1.0);
        if *p == "wake_hop" {
            assert!(ratio >= 0.9, "{p}: trace {trace_mean} vs hist {hist_mean}");
        } else if n >= 8 {
            // A 1-in-N sample of a tight distribution: loose bound, the
            // exact law is the sum check above.
            assert!(
                (0.25..=4.0).contains(&ratio),
                "{p}: trace mean {trace_mean} vs hist mean {hist_mean} (n={n})"
            );
        }
    }
    eprintln!(
        "zc bridge: {READS} reads, {} traced chains (divisor {divisor}); means µs: {}",
        n,
        BRIDGE_PHASES
            .iter()
            .chain(std::iter::once(&"total"))
            .map(|p| {
                let (n0, s0) = phase(&pre, "zc_bridge_phase_ns", p);
                let (n1, s1) = phase(&post, "zc_bridge_phase_ns", p);
                format!("{p}={:.1}", (s1 - s0) as f64 / (n1 - n0) as f64 / 1000.0)
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}

/// The composed READ dispatch law's PARTITION on a zc-armed session
/// (R-2 ⊕ R-3): every READ delivered on the ring takes exactly one arm —
/// served inline (warm), fused onto the queue worker's lane (cold, zc,
/// under the ceiling), or handed to a handler lane — so
/// `serves + zc_read_fusions + demotes ≡ READs`, and the two handler
/// venues together are exactly the in-place replies
/// (`zc_read_fusions + demotes ≡ fuse3_read_inplace_replies`). With the
/// fusion lever off (the shipped default) the same READs all take the
/// lane arm (the R-2 shape); with it on they fuse, and only the fused arm
/// resolves bridge CQEs in the worker's mid-pass reap.
#[test]
fn composed_dispatch_law_partitions_every_read_on_a_zc_session() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("partition");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    const READS: u64 = 128;
    const FILE_LEN: usize = 4096 * READS as usize * 2;
    {
        let log0 = base.join("mount-publish.log");
        let mount = spawn_zc_mount(&meta, &mnt, &log0, &[]);
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
        publish_file(&mnt, "cold.bin", FILE_LEN);
    }
    let words = |m: &serde_json::Value| -> (u64, u64, u64, u64) {
        (
            word(m, "transport_fast_dispatch_serves"),
            word(m, "fuse3_zc_read_fusions"),
            word(m, "transport_fast_dispatch_demotes"),
            word(m, "fuse3_read_inplace_replies"),
        )
    };
    let mut msg_hop_means = Vec::new();
    // The lever's default is OFF (arm (c) ships — the composition
    // measured it); both arms are named explicitly so the contract does
    // not depend on the default.
    for (arm, envs) in [
        ("fused", vec![("SQUEEZEFS_FUSE_ZC_READ_FUSION", "1")]),
        ("lane", vec![("SQUEEZEFS_FUSE_ZC_READ_FUSION", "0")]),
    ] {
        let log = base.join(format!("mount-{arm}.log"));
        let mount = spawn_zc_mount(&meta, &mnt, &log, &envs);
        assert!(zc_armed(&log), "{arm}: the mount arms zc");
        let path = mnt.join("cold.bin");
        let pre = stats(&mnt);
        for i in 0..READS {
            let off = i * 2 * 4096;
            let got = odirect_pread_4k(&path, off);
            assert!(got.iter().all(|b| *b == ((off / 4096) as u8) ^ 0x5A));
        }
        let post = stats(&mnt);
        let (s0, f0, d0, i0) = words(&pre);
        let (s1, f1, d1, i1) = words(&post);
        let (ds, df, dd, di) = (s1 - s0, f1 - f0, d1 - d0, i1 - i0);
        // The population: READS cold data reads + the `.stats` read whose
        // commit lands after `pre` was built (a virtual ino ⇒ demote to a
        // lane; its size is above the ceiling anyway).
        assert_eq!(ds, 0, "{arm}: a cold zc read is never served inline");
        assert_eq!(
            df + dd,
            di,
            "{arm}: the two handler venues are exactly the in-place replies"
        );
        assert!(
            di >= READS,
            "{arm}: every data read replied in place ({di} for {READS})"
        );
        match arm {
            "fused" => {
                // ≥: the `.stats` read the bracket issues is chunked by
                // the kernel, and its under-ceiling tail chunk fuses too.
                assert!(
                    df >= READS,
                    "fused: every cold 4 KiB READ fused onto the worker ({df} for {READS})"
                );
                assert_eq!(
                    word(&post, "fuse3_zc_read_fusion_demotions")
                        - word(&pre, "fuse3_zc_read_fusion_demotions"),
                    0,
                    "fused: the lane never refused a fresh delivery"
                );
            }
            _ => assert_eq!(df, 0, "lane: the lever off fuses nothing"),
        }
        let (n0, s0) = phase(&pre, "zc_bridge_phase_ns", "msg_hop");
        let (n1, s1) = phase(&post, "zc_bridge_phase_ns", "msg_hop");
        assert_eq!(n1 - n0, READS, "{arm}: one bridge per data read");
        msg_hop_means.push((s1 - s0) as f64 / READS as f64);
        // The venue law: a fused READ's fetch CQE resolves in the worker's
        // MID-PASS reap (the interleave runs only while fused tasks are
        // resident); on the lane arm no fused task exists, so every bridge
        // CQE resolves at a pass bottom.
        let midpass =
            word(&post, "fuse3_fused_midpass_reaps") - word(&pre, "fuse3_fused_midpass_reaps");
        match arm {
            "fused" => assert!(midpass > 0, "fused: bridge CQEs resolve mid-pass"),
            _ => assert_eq!(midpass, 0, "lane: no fused task, no mid-pass reap"),
        }
        drop(mount);
    }
    // Observation, not a contract (the hop's cost is the scheduler's at
    // load, a few µs on an idle box): fused msg_hop is a same-thread
    // channel op, the lane's is an eventfd wake + a worker pass.
    eprintln!(
        "msg_hop mean: fused {:.0} ns, lane {:.0} ns",
        msg_hop_means[0], msg_hop_means[1]
    );
    let _ = std::fs::remove_dir_all(&base);
}
