//! Transport ingress economy campaign — Phase 1: the WRITE transport
//! residence family (`write_transport_phase_ns`), the write twin of
//! `read_transport_phase_ns` (`.benchmarks/2026-08-01-serve-decomposition.md`
//! §6 item 3: "extend the transport family to WRITE — the stamps exist,
//! the family is READ-gated by one opcode check").
//!
//! Why this exists: the write wall's ~5.3 ms pre-handler leg is a STATED
//! INFERENCE in the decomposition note (§4.1 — "the same dispatch machinery
//! reads measured at 3.25 ms daemon-side", inferred from shared machinery,
//! never measured). This family converts that inference into measurement
//! and is the campaign's write-side acceptance instrument.
//!
//! Phases (identical semantics to the READ family — one shared
//! `TransportPhase` enum, two op-class tables):
//!
//! - `queue_wait` — ring CQE reaped (inbound push) → session dispatch
//!   pop (the WRITE stamps ride the same dispatch loop).
//! - `dispatch_lag` — dispatch → the write-handler future's first poll.
//! - `reply_commit` — `fs.write` returned → the reply handed to the
//!   transport.
//! - `transport_total` — inbound push → reply committed; `fio clat −
//!   transport_total` = the kernel-side residue for WRITE, statable by
//!   subtraction (the §4.1 split).
//!
//! Always-on (the `write_pipeline_phase_ns` cost contract: ≤ 4 `Instant`
//! reads per WRITE actually delivered over the armed uring transport);
//! buckets through the SHARED `latency_core`, so the write table composes
//! bucket-for-bucket with `fuse_op_phase_ns[write]` and
//! `write_pipeline_phase_ns`. Error replies deliberately record nothing.
//! Engagement (every armed-session WRITE records) is field-verified — a
//! pure unit test cannot exercise an armed session (the read family's
//! standing repro-port exception).
//!
//! RED against dev 3cd528b: `fuse3::TransportPhase`,
//! `fuse3::write_transport_phase_{record,snapshot}` and the root
//! `write_transport_phase_json` / stats-inode key do not exist.
//!
//! Phase 3 (the build — contracts 4/5/6 below): the mechanism hunt named
//! the term **pinned-thread runqueue hostage** — every fuse3-tpc lane (and
//! every fuse-over-uring queue worker) is hard-pinned to ONE core, so
//! under load a cross-thread wake waits ms-class for that specific core's
//! runqueue (local discriminators: lanes at 2.3–13.7 s runqueue wait vs
//! ~2.4 s CPU per 35 s window; SCHED_FIFO and affinity-widening each
//! collapse queue_wait+dispatch_lag 3.1+2.9 → 0.04+0.04 ms, −98.6 %).
//! The fix: **node-scoped affinity** — a lane/queue-worker thread stays
//! on its home NUMA node (locality preserved: `node_lanes` grouping,
//! arena binding, `tpc_spawn_on_node` unchanged) but may run on ANY
//! process-mask CPU of that node. `SQUEEZEFS_FUSE_PIN_SCOPE=core` is the
//! A0 measurement lever (the exact pre-campaign 1-CPU pin posture) and
//! the operational escape. Plus the in-place WRITE reply arm (the READ
//! P2 twin): an armed session's WRITE reply is a synchronous COMMIT
//! enqueue from the handler task — no reply-channel + reply-task hop
//! (whose wake pays the same hostage class on the pinned main runtime) —
//! with `fuse3_write_inplace_replies` as the engagement gauge that keeps
//! it wired (the READ arm's silent-disengagement lesson).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{write_transport_phase_json, SqueezefsFilesystem, STATS_INODE};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs_testkit::{mount_supported, site, skip};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tempfile::{tempdir, NamedTempFile, TempDir};

const TRANSPORT_PHASES: [&str; 5] = [
    "queue_wait",
    "dispatch_lag",
    "reply_commit",
    "transport_total",
    // 2026-08-04 kmbuf campaign: COMMIT-carrying ring-flush syscall
    // duration — per-FLUSH sampled (saturated/wait-free flushes only),
    // NOT per-op; the venue of the kernel's commit-side copy machinery,
    // so the killed FR_LOCKED/GUP term shows as this phase's
    // before/after delta on kmbuf A/Bs.
    "commit_flush",
];

/// The histogram families are process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Sum of one phase histogram's buckets (= spans recorded).
fn phase_count(family: &serde_json::Value, phase: &str) -> u64 {
    family
        .get(phase)
        .unwrap_or_else(|| panic!("phase key {phase} missing from family: {family}"))
        .as_object()
        .expect("phase histogram must be a bucket object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

fn read_family_snapshot_sums() -> Vec<(String, u64)> {
    fuse3::read_transport_phase_snapshot()
        .iter()
        .map(|(name, buckets)| ((*name).to_string(), buckets.iter().sum::<u64>()))
        .collect()
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The read_serve_phase suite's harness shape: real v3 meta backend, real
/// router stack — enough to read the stats inode.
async fn make(test_id: &str, uuid: [u8; 16]) -> H {
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — the WRITE transport family exists with exactly the READ
// family's phase set (names AND order: the two tables must compose in one
// analyzer), recording is phase-exact, and the write family is INDEPENDENT
// of the read family (a WRITE span must never pollute the read table — the
// read acceptance numbers ride on it).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_family_shape_phase_exact_and_independent() {
    let _g = serial().await;

    let read_before = read_family_snapshot_sums();
    let snap0 = fuse3::write_transport_phase_snapshot();
    fuse3::write_transport_phase_record(
        fuse3::TransportPhase::QueueWait,
        std::time::Duration::from_micros(100),
    );
    let snap1 = fuse3::write_transport_phase_snapshot();

    assert_eq!(snap1.len(), TRANSPORT_PHASES.len());
    for (i, name) in TRANSPORT_PHASES.iter().enumerate() {
        assert_eq!(
            snap1[i].0, *name,
            "write transport phase order/name must mirror the read family"
        );
        let d: u64 = snap1[i].1.iter().sum::<u64>() - snap0[i].1.iter().sum::<u64>();
        let want = u64::from(*name == "queue_wait");
        assert_eq!(
            d, want,
            "one write queue_wait span recorded ⇒ exactly the write \
             queue_wait phase moves (phase {name} moved by {d})"
        );
    }

    // Independence: the read table did not move.
    assert_eq!(
        read_family_snapshot_sums(),
        read_before,
        "recording a WRITE span must never move the READ transport family"
    );

    // And the reverse: a read span leaves the write table flat.
    let wsnap0 = fuse3::write_transport_phase_snapshot();
    fuse3::read_transport_phase_record(
        fuse3::TransportPhase::DispatchLag,
        std::time::Duration::from_micros(100),
    );
    let wsnap1 = fuse3::write_transport_phase_snapshot();
    for i in 0..TRANSPORT_PHASES.len() {
        assert_eq!(
            wsnap1[i].1.iter().sum::<u64>(),
            wsnap0[i].1.iter().sum::<u64>(),
            "recording a READ span must never move the WRITE transport family"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 2 — the root JSON payload carries the full shared-core bucket
// set (the fuse3 rig buckets through the same latency_core, label-for-
// bucket exact), and a recorded span lands in the shared-core bucket.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_family_json_buckets_through_the_shared_core() {
    let _g = serial().await;

    fuse3::write_transport_phase_record(
        fuse3::TransportPhase::TransportTotal,
        std::time::Duration::from_micros(100),
    );

    let fam = write_transport_phase_json();
    for name in TRANSPORT_PHASES {
        let hist = fam.get(name).expect("phase present in JSON");
        let obj = hist.as_object().expect("bucketed object");
        assert_eq!(
            obj.len(),
            squeezefs::latency_core::LATENCY_BUCKET_LABELS.len(),
            "write transport histograms carry the full standard bucket set"
        );
        for label in squeezefs::latency_core::LATENCY_BUCKET_LABELS {
            assert!(
                obj.contains_key(label),
                "standard bucket label {label} present"
            );
        }
    }

    let idx = squeezefs::latency_core::latency_bucket_index(100);
    let snap = fuse3::write_transport_phase_snapshot();
    let total_row = snap
        .iter()
        .find(|(n, _)| *n == "transport_total")
        .expect("transport_total phase");
    assert!(
        total_row.1[idx] >= 1,
        "the recorded 100 µs span must land in shared-core bucket {idx}"
    );
}

// ---------------------------------------------------------------------------
// Contract 3 — the stats inode carries `write_transport_phase_ns` UNGATED
// (always-on: the field reads the write decomposition off production
// mounts without a remount-to-arm round trip), alongside the read family.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_write_transport_family_ungated() {
    let _g = serial().await;
    assert!(
        std::env::var("SQUEEZEFS_OP_PROFILE").is_err(),
        "fixture premise: the profile rig must be OFF — the family is \
         deliberately always-on"
    );

    let h = make("wtp_stats_surface", *b"wtp-stats-vol-v3").await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let metrics = stats.get("metrics").expect("metrics object");
    for key in ["read_transport_phase_ns", "write_transport_phase_ns"] {
        let fam = metrics
            .get(key)
            .unwrap_or_else(|| panic!("stats inode metrics must carry {key} UNGATED"));
        for p in TRANSPORT_PHASES {
            let _ = phase_count(fam, p);
        }
    }
    // The in-place WRITE reply engagement gauge rides along (the READ
    // arm's silent-disengagement lesson: a gauge is what keeps it wired).
    let v = metrics
        .get("fuse3_write_inplace_replies")
        .expect("stats inode metrics must carry fuse3_write_inplace_replies");
    assert!(v.is_u64(), "fuse3_write_inplace_replies is a counter");
}

// ---------------------------------------------------------------------------
// Contract 4 — pin-scope derivation is pure and exact: `core` = the
// pre-campaign 1-CPU pin; `node` = every AVAILABLE cpu of the home cpu's
// node (locality preserved, hostage deleted); an unknown node degrades to
// the whole available set (freedom is the safe direction — a 1-CPU pin is
// the measured failure). Env: default node; `core` honored; junk = node.
// ---------------------------------------------------------------------------

#[test]
fn pin_scope_parse_and_affinity_derivation() {
    use fuse3::PinScope;

    assert_eq!(fuse3::pin_scope_from_env(None), PinScope::Node);
    assert_eq!(fuse3::pin_scope_from_env(Some("node")), PinScope::Node);
    assert_eq!(fuse3::pin_scope_from_env(Some("core")), PinScope::Core);
    assert_eq!(
        fuse3::pin_scope_from_env(Some("bogus")),
        PinScope::Node,
        "unknown value must degrade to the default posture (loudly), never crash"
    );

    // avail = process-mask cores after the lane pool's core-0 reserve;
    // node_of: cpus 0..4 on node 0, 4..8 on node 1, cpu 9 unknown.
    let avail = vec![1usize, 2, 3, 4, 5, 6, 7, 9];
    let node_of = |cpu: usize| -> Option<usize> {
        match cpu {
            0..=3 => Some(0),
            4..=7 => Some(1),
            _ => None,
        }
    };
    let node_cpus = |node: usize| -> Vec<usize> {
        match node {
            0 => vec![0, 1, 2, 3],
            1 => vec![4, 5, 6, 7],
            _ => vec![],
        }
    };

    // core scope: exactly the home cpu.
    assert_eq!(
        fuse3::scoped_affinity_cpus(PinScope::Core, 2, &avail, node_of, node_cpus),
        vec![2]
    );
    // node scope: home 2 (node 0) ⇒ node-0 cpus ∩ avail (core 0 stays
    // reserved because it is not in avail).
    assert_eq!(
        fuse3::scoped_affinity_cpus(PinScope::Node, 2, &avail, node_of, node_cpus),
        vec![1, 2, 3]
    );
    // node scope on the other node keeps locality.
    assert_eq!(
        fuse3::scoped_affinity_cpus(PinScope::Node, 5, &avail, node_of, node_cpus),
        vec![4, 5, 6, 7]
    );
    // unknown node ⇒ the whole available set (never a 1-CPU hostage).
    assert_eq!(
        fuse3::scoped_affinity_cpus(PinScope::Node, 9, &avail, node_of, node_cpus),
        avail
    );
}

// ---------------------------------------------------------------------------
// Mount harness (the transport_concurrency_tests shape: real unprivileged
// mount, kernel FUSE-over-io_uring; skips honestly where unsupported).
// ---------------------------------------------------------------------------

struct Mount {
    child: Child,
    mnt: PathBuf,
    base: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn stats(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.mnt.join(".stats")).expect("read .stats");
        serde_json::from_str(&raw).expect("stats JSON")
    }

    fn metric_u64(&self, key: &str) -> u64 {
        self.stats()["metrics"][key]
            .as_u64()
            .unwrap_or_else(|| panic!("metric {key} missing/not-u64"))
    }

    /// `(comm, Cpus_allowed_count)` for every daemon thread.
    fn thread_affinities(&self) -> Vec<(String, u32)> {
        let pid = self.child.id();
        let mut out = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return out;
        };
        for t in tasks.flatten() {
            let dir = t.path();
            let Ok(comm) = std::fs::read_to_string(dir.join("comm")) else {
                continue;
            };
            let Ok(status) = std::fs::read_to_string(dir.join("status")) else {
                continue;
            };
            let count = status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                .map(|l| {
                    l.trim()
                        .split(',')
                        .map(|range| match range.split_once('-') {
                            Some((a, b)) => {
                                b.trim().parse::<u32>().unwrap_or(0)
                                    - a.trim().parse::<u32>().unwrap_or(0)
                                    + 1
                            }
                            None => 1,
                        })
                        .sum::<u32>()
                })
                .unwrap_or(0);
            out.push((comm.trim().to_string(), count));
        }
        out
    }

    fn unmount(mut self) {
        let _ = Command::new("fusermount3")
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

fn mount_fs(tag: &str, envs: &[(&str, &str)]) -> Mount {
    let bin = env!("CARGO_BIN_EXE_squeezefs");
    let base = std::env::temp_dir().join(format!("sqfs_tingress_{tag}_{}", std::process::id()));
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
    cmd.env_remove("SQUEEZEFS_FUSE_PIN_SCOPE");
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

// ---------------------------------------------------------------------------
// Contract 5 — the live affinity posture: by DEFAULT every fuse3-tpc lane
// and every fuse-over-uring thread is schedulable on more than one CPU
// (the pinned-runqueue hostage is gone); under the A0 lever
// (`SQUEEZEFS_FUSE_PIN_SCOPE=core`) the lanes are 1-CPU pinned exactly as
// before the campaign (the measurement control is the prior posture, live).
// ---------------------------------------------------------------------------

#[test]
fn default_posture_is_node_scoped_lever_restores_core_pins() {
    if !mount_supported(site!()) {
        return;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cpus <= 2 {
        skip!(Capability, "needs > 2 CPUs to discriminate postures");
    }

    // Default: node scope.
    let m = mount_fs("pin_default", &[]);
    let affs = m.thread_affinities();
    let lanes: Vec<_> = affs
        .iter()
        .filter(|(c, _)| c.starts_with("fuse3-tpc"))
        .collect();
    let workers: Vec<_> = affs
        .iter()
        .filter(|(c, _)| c.starts_with("f3-ur"))
        .collect();
    assert!(!lanes.is_empty(), "no fuse3-tpc lanes visible: {affs:?}");
    assert!(!workers.is_empty(), "no fuse-over-uring threads visible");
    for (comm, n) in &lanes {
        assert!(
            *n > 1,
            "default posture: lane {comm} is 1-CPU pinned (the hostage posture); \
             affinities: {affs:?}"
        );
    }
    for (comm, n) in &workers {
        assert!(
            *n > 1,
            "default posture: queue thread {comm} is 1-CPU pinned; affinities: {affs:?}"
        );
    }
    m.unmount();

    // A0 lever: the pre-campaign core pins, byte-for-byte posture.
    let m = mount_fs("pin_core", &[("SQUEEZEFS_FUSE_PIN_SCOPE", "core")]);
    let affs = m.thread_affinities();
    let lanes: Vec<_> = affs
        .iter()
        .filter(|(c, _)| c.starts_with("fuse3-tpc"))
        .collect();
    assert!(!lanes.is_empty(), "no fuse3-tpc lanes visible: {affs:?}");
    for (comm, n) in &lanes {
        assert_eq!(
            *n, 1,
            "core lever: lane {comm} must be exactly 1-CPU pinned; affinities: {affs:?}"
        );
    }
    assert!(
        affs.iter().any(|(c, n)| c.starts_with("f3-ur") && *n == 1),
        "core lever: at least the online-qid queue workers must be 1-CPU pinned"
    );
    m.unmount();
}

// ---------------------------------------------------------------------------
// Contract 6 — the in-place WRITE reply arm engages on an armed session
// (fuse3_write_inplace_replies accounts the row's WRITEs — the gauge that
// keeps the arm wired), the write transport family records live spans,
// and data round-trips byte-exact through the in-place arm.
// ---------------------------------------------------------------------------

#[test]
fn write_inplace_replies_engage_and_round_trip() {
    if !mount_supported(site!()) {
        return;
    }
    let m = mount_fs("wr_inplace", &[]);

    let g0 = m.metric_u64("fuse3_write_inplace_replies");
    let fam0 = m.stats()["metrics"]["write_transport_phase_ns"].clone();

    // 8 MiB buffered write + fsync: the kernel issues >= 8 FUSE_WRITEs
    // (max_write = 1 MiB) over the armed transport.
    let path = m.mnt.join("inplace_probe.bin");
    let payload: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    {
        let mut f = std::fs::File::create(&path).expect("create probe file");
        f.write_all(&payload).expect("write probe payload");
        f.sync_all().expect("fsync probe payload");
    }

    let g1 = m.metric_u64("fuse3_write_inplace_replies");
    assert!(
        g1 - g0 >= 8,
        "armed-session WRITE replies must ride the in-place arm \
         (fuse3_write_inplace_replies moved {} for an 8 MiB write)",
        g1 - g0
    );

    let fam1 = m.stats()["metrics"]["write_transport_phase_ns"].clone();
    for p in TRANSPORT_PHASES {
        if p == "commit_flush" {
            // Per-FLUSH sampled (saturated flushes only) — presence is
            // pinned by the shape contracts; growth is load-dependent.
            continue;
        }
        assert!(
            phase_count(&fam1, p) > phase_count(&fam0, p),
            "live armed-session WRITEs must record write transport phase {p}"
        );
    }

    // Round-trip through a fresh open (page cache still proves the reply
    // path acked the right bytes; the fsync above forced real WRITEs).
    let got = std::fs::read(&path).expect("read probe back");
    assert_eq!(got.len(), payload.len(), "probe length");
    assert_eq!(got, payload, "probe content must round-trip byte-exact");

    m.unmount();
}
