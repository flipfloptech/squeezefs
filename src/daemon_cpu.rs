//! Daemon CPU attribution — e2e audit E (2026-09-02,
//! `.benchmarks/2026-09-02-e2e-audit`). No rig or row emitted cpu-ns per
//! user byte or per op; this is the process-side term: `daemon_cpu_ns`
//! (RUSAGE_SELF utime+stime) and `daemon_cpu_ns_by_class` (per-thread
//! `/proc/self/task/*/schedstat` folded by comm prefix).
//!
//! Sampled at stats-read time ONLY — the stats read is the whole cost
//! (one `getrusage` + two small procfs reads per live thread). Retired
//! threads keep their last sampled ns in a per-tid ledger, so a class
//! never falls when its threads exit (a monotone gauge; the CPU a thread
//! burns between its last sample and its exit is the one term this loses).
//!
//! Class table: the daemon's named thread populations. There is no tokio
//! worker class — rip-tokio-total left none; the checkpoint/SMO task, the
//! reclaim worker and the publish conveyor all ride the `sqz-meta` lanes,
//! and device reclaim commands ride the `sqz-blk` blocking pool, so those
//! are attributed by LANE, not by task.

use std::collections::HashMap;
use std::sync::Mutex;

/// One comm-prefix → class row.
pub struct ThreadClass {
    pub prefix: &'static str,
    pub class: &'static str,
}

/// Comm prefix table, first match wins (order matters: `sqz-ipc-svc`
/// before any shorter `sqz-ipc` prefix could be added). The mount-slot
/// comm suffix (`m{:x}`) sits after the index, so prefix matching
/// tolerates it.
pub const CLASSES: [ThreadClass; 10] = [
    ThreadClass {
        prefix: "fuse3-tpc",
        class: "fuse3-tpc",
    },
    // The FUSE-over-io_uring queue workers (`f3-ur{qid}[-{qid}]`, plus the
    // `/dev/fuse` watcher): the transport's submit/reap side and, on a zc
    // kernel, the device DMA issuer of the READ direct leg. R-1 (2026-09-02)
    // found them folded into `other` at 51 % of a kern rand-4k row's CPU.
    ThreadClass {
        prefix: "f3-ur",
        class: "fuse3-ur",
    },
    ThreadClass {
        prefix: "sqz-ipc-svc",
        class: "sqz-ipc-svc",
    },
    ThreadClass {
        prefix: "sqz-ipc-dd",
        class: "sqz-ipc-dd",
    },
    ThreadClass {
        prefix: "sqz-meta",
        class: "sqz-meta",
    },
    // The per-volume journal lanes (C-2): both commit-conveyor stages and
    // the volume's journal ring, one thread per writable volume.
    ThreadClass {
        prefix: "sqz-jrnl",
        class: "sqz-jrnl",
    },
    ThreadClass {
        prefix: "sqz-blk",
        class: "sqz-blk",
    },
    ThreadClass {
        prefix: "sqz-nvme",
        class: "sqz-nvme",
    },
    ThreadClass {
        prefix: "sqz-zcrx",
        class: "sqz-zcrx",
    },
    // Both `sqz_time` service threads (the squeezefs-ipc registry and the
    // fuse3 fork's `#[path]`-shared copy): the timer class's own CPU face
    // — tombstone pops under each registry lock (R-1, candidate finding 48).
    ThreadClass {
        prefix: "sqz-timer",
        class: "sqz-timer",
    },
];

/// The residual class (main thread, timer, signal, accept, reap, …).
pub const OTHER: &str = "other";

/// Class of a thread comm.
pub fn classify(comm: &str) -> &'static str {
    CLASSES
        .iter()
        .find(|c| comm.starts_with(c.prefix))
        .map(|c| c.class)
        .unwrap_or(OTHER)
}

#[inline]
fn class_index(class: &str) -> usize {
    CLASSES
        .iter()
        .position(|c| c.class == class)
        .unwrap_or(CLASSES.len())
}

/// One sample: process total + per-class ns (every class present, `other`
/// last). `by_class` is read BEFORE `total_ns`, so `Σ by_class ≤ total_ns`
/// holds by construction.
pub struct CpuSample {
    pub total_ns: u64,
    pub by_class: Vec<(&'static str, u64)>,
}

/// The sampling ledger: per live tid its last `(class index, ns)`, plus
/// per class the ns of threads that retired (vanished, or a tid reused —
/// `sum_exec_runtime` is monotone per thread, so a DROP proves reuse).
/// A stats read is a FUSE op, so a plain mutex is in-policy here.
struct Ledger {
    live: HashMap<u32, (usize, u64)>,
    retired: Vec<u64>,
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

/// `RUSAGE_SELF` utime + stime, ns.
pub fn process_cpu_ns() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable rusage; RUSAGE_SELF needs no
    // other input.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let tv = |t: libc::timeval| (t.tv_sec as u64) * 1_000_000_000 + (t.tv_usec as u64) * 1_000;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// One thread's on-CPU ns: `schedstat` field 1 (`sum_exec_runtime`);
/// without `CONFIG_SCHED_INFO`, `stat` utime+stime ticks scaled to ns.
fn thread_cpu_ns(dir: &std::path::Path) -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string(dir.join("schedstat")) {
        if let Some(ns) = s.split_whitespace().next().and_then(|v| v.parse().ok()) {
            return Some(ns);
        }
    }
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // Fields after the parenthesised comm: state is field 3, utime/stime
    // are fields 14/15 (1-based) — i.e. indices 11 and 12 past ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    let mut it = rest.split_whitespace();
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    // SAFETY: sysconf is a pure query.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    Some((utime + stime) * 1_000_000_000 / hz)
}

/// Sample the process: scan live threads (folding into the ledger), then
/// read the process total.
pub fn sample() -> CpuSample {
    let mut guard = LEDGER.lock().unwrap_or_else(|e| e.into_inner());
    let ledger = guard.get_or_insert_with(|| Ledger {
        live: HashMap::new(),
        retired: vec![0; CLASSES.len() + 1],
    });
    let mut seen: Vec<u32> = Vec::with_capacity(ledger.live.len());
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for entry in tasks.flatten() {
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let dir = entry.path();
            let Some(ns) = thread_cpu_ns(&dir) else {
                continue;
            };
            let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
            let ci = class_index(classify(comm.trim_end()));
            seen.push(tid);
            match ledger.live.get_mut(&tid) {
                Some(e) if e.0 == ci && ns >= e.1 => e.1 = ns,
                Some(e) => {
                    // Reused tid (class changed or the monotone runtime
                    // dropped): the predecessor retires at its last sample.
                    ledger.retired[e.0] += e.1;
                    *e = (ci, ns);
                }
                None => {
                    ledger.live.insert(tid, (ci, ns));
                }
            }
        }
    }
    // Vanished threads retire at their last sample (a class never falls).
    let gone: Vec<u32> = ledger
        .live
        .keys()
        .filter(|t| !seen.contains(t))
        .copied()
        .collect();
    for tid in gone {
        if let Some((ci, ns)) = ledger.live.remove(&tid) {
            ledger.retired[ci] += ns;
        }
    }
    let mut sums = ledger.retired.clone();
    for (ci, ns) in ledger.live.values() {
        sums[*ci] += ns;
    }
    drop(guard);
    let total_ns = process_cpu_ns();
    let by_class = CLASSES
        .iter()
        .map(|c| c.class)
        .chain(std::iter::once(OTHER))
        .zip(sums)
        .collect();
    CpuSample { total_ns, by_class }
}

impl CpuSample {
    /// `daemon_cpu_ns_by_class` stats payload: `{class: ns}`.
    pub fn by_class_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (k, v) in &self.by_class {
            m.insert(k.to_string(), serde_json::Value::from(*v));
        }
        serde_json::Value::Object(m)
    }
}
