//! D-3 (e2e perf audit DLM board #4) — the in-process METADATA STORM that
//! reads the stripe-collision census: which striped table collides under
//! the mdstorm shape, at what rate per op, and what the 4a wait/hold
//! histograms say while it does. The field row (`tests/run_mdstorm.sh`,
//! root + a mount) is the parent campaign's; this is the same shape over
//! the routed backend directly — T concurrent committers, the canonical
//! create → rename → unlink sequence, both the ONE-DIR interleave (the
//! parent-lock crucible) and the MANY-DIRS mfcreate shape.
//!
//! Contracts pinned (the numbers are printed as rows; `--nocapture`):
//!
//! - **closure**: `lock_phase_ns.dlm_guard_wait.count` ≡ Σ the two 4a
//!   census classes over the storm — one wait sample per contended 4a
//!   acquire, classified exactly once;
//! - **the census names the table**: on the MANY-DIRS shape (distinct
//!   parents, distinct names) every 4a wait is a stripe COLLISION (no two
//!   committers ever ask for the same key — modulo the two-phase unlink's
//!   re-acquire artifact, bounded by the collision count), and on the
//!   ONE-DIR rename phase the 4a I-class records KEY waits (every rename
//!   takes the one parent EXCLUSIVE — the workload's own serialization,
//!   not the table's);
//! - **the 4a scoping**: a solo sequential committer never waits on its
//!   own previous tx (the guards drop before the ack);
//! - **the un-held tables stay quiet**: the storm commits no data, so
//!   `block_flush` and `serve_ino` record nothing and `inode_meta` only
//!   what the create path's metadata-cache refill touches.
//!
//! Width lever: `SQUEEZEFS_DLM_STRIPES` is read once per process, so a
//! bracket runs this binary once per width:
//! `SQUEEZEFS_DLM_STRIPES=4096 cargo test --release --test
//! dlm_stripe_storm_tests -- --nocapture --test-threads=1` vs unset
//! (derived). Suite runs `--test-threads=1` (process-global counters).

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::stripe_locks::{
    BLOCK_FLUSH_CENSUS, DLM_DENTRY_CENSUS, DLM_INODE_CENSUS, INODE_META_CENSUS,
    LEASE_WAITER_CENSUS, SERVE_INO_CENSUS,
};
use std::sync::Arc;
use std::time::Instant;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 512 * 1024 * 1024;
const NODE_SIZE: usize = 256 * 1024;
const RING_LEN: u64 = 32 * 1024 * 1024;

/// Writers (the mdstorm rig's `SQZ_MDSTORM_THREADS`, here the in-flight
/// committer population) and names per writer per phase.
fn storm_shape() -> (usize, usize) {
    let threads = std::env::var("SQZ_STORM_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let per = std::env::var("SQZ_STORM_PER_THREAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) { 64 } else { 512 });
    (threads, per)
}

async fn sandbox() -> (Arc<RoutedMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    (Arc::new(RoutedMetaBackend::new(vec![kv])), file)
}

#[derive(Clone, Copy, Default, Debug)]
struct Census {
    dlm_inode: (u64, u64),
    dlm_dentry: (u64, u64),
    serve_ino: (u64, u64),
    inode_meta: (u64, u64),
    block_flush: (u64, u64),
    lease_waiter: (u64, u64),
}

fn census() -> Census {
    Census {
        dlm_inode: DLM_INODE_CENSUS.snapshot(),
        dlm_dentry: DLM_DENTRY_CENSUS.snapshot(),
        serve_ino: SERVE_INO_CENSUS.snapshot(),
        inode_meta: INODE_META_CENSUS.snapshot(),
        block_flush: BLOCK_FLUSH_CENSUS.snapshot(),
        lease_waiter: LEASE_WAITER_CENSUS.snapshot(),
    }
}

fn sub(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (a.0 - b.0, a.1 - b.1)
}

fn census_delta(before: Census, after: Census) -> Census {
    Census {
        dlm_inode: sub(after.dlm_inode, before.dlm_inode),
        dlm_dentry: sub(after.dlm_dentry, before.dlm_dentry),
        serve_ino: sub(after.serve_ino, before.serve_ino),
        inode_meta: sub(after.inode_meta, before.inode_meta),
        block_flush: sub(after.block_flush, before.block_flush),
        lease_waiter: sub(after.lease_waiter, before.lease_waiter),
    }
}

/// `(count, sum_ns, buckets)` of one `lock_phase_ns` phase.
fn phase(fam: &serde_json::Value, name: &str) -> (u64, u64, Vec<u64>) {
    let h = &fam[name];
    let count = h["count"].as_u64().unwrap();
    let sum = h["sum_ns"].as_u64().unwrap();
    let buckets = squeezefs::latency_core::LATENCY_BUCKET_LABELS
        .iter()
        .map(|l| h["buckets"][*l].as_u64().unwrap_or(0))
        .collect();
    (count, sum, buckets)
}

/// Bucket-resolved percentile (upper bound label of the bucket the
/// percentile falls in) over a bucket DELTA.
fn pct(buckets: &[u64], p: f64) -> &'static str {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return "-";
    }
    let target = ((total as f64) * p).ceil() as u64;
    let mut acc = 0;
    for (i, b) in buckets.iter().enumerate() {
        acc += b;
        if acc >= target {
            return squeezefs::latency_core::LATENCY_BUCKET_LABELS[i];
        }
    }
    squeezefs::latency_core::LATENCY_BUCKET_LABELS[buckets.len() - 1]
}

fn bucket_delta(a: &[u64], b: &[u64]) -> Vec<u64> {
    a.iter().zip(b).map(|(x, y)| x - y).collect()
}

struct PhaseRow {
    name: &'static str,
    ops: usize,
    wall: std::time::Duration,
    census: Census,
    wait_count: u64,
    wait_sum_ns: u64,
    wait_p50: &'static str,
    wait_p99: &'static str,
    hold_count: u64,
    hold_p50: &'static str,
    hold_p99: &'static str,
}

impl PhaseRow {
    fn print(&self, width: usize) {
        let ops_s = self.ops as f64 / self.wall.as_secs_f64();
        let per = |c: (u64, u64)| {
            format!(
                "{:.4}/{:.4}",
                c.0 as f64 / self.ops as f64,
                c.1 as f64 / self.ops as f64
            )
        };
        println!(
            "[storm w={width}] {:<18} ops={:<6} wall={:>7.3}s ops/s={:>8.0} | 4a I coll/key per op {} | 4a D {} | inode_meta {} | block {} | serve {} | lease {} | dlm_guard_wait n={} Σ={:.1}ms p50={} p99={} | dlm_guard_hold n={} p50={} p99={}",
            self.name,
            self.ops,
            self.wall.as_secs_f64(),
            ops_s,
            per(self.census.dlm_inode),
            per(self.census.dlm_dentry),
            per(self.census.inode_meta),
            per(self.census.block_flush),
            per(self.census.serve_ino),
            per(self.census.lease_waiter),
            self.wait_count,
            self.wait_sum_ns as f64 / 1e6,
            self.wait_p50,
            self.wait_p99,
            self.hold_count,
            self.hold_p50,
            self.hold_p99,
        );
    }
}

/// Run one phase: `threads` tasks, each running `per` ops of `op(w, i)`
/// (writer index, name index) concurrently; measure the census and the
/// two 4a histograms around it.
async fn run_phase<F, Fut>(name: &'static str, threads: usize, per: usize, op: F) -> PhaseRow
where
    F: Fn(usize, usize) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    use squeezefs::fuse_client::lock_phase_json;
    let c0 = census();
    let f0 = lock_phase_json();
    let (w0, ws0, wb0) = phase(&f0, "dlm_guard_wait");
    let (h0, _, hb0) = phase(&f0, "dlm_guard_hold");
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(threads);
    for w in 0..threads {
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..per {
                op(w, i).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let wall = t0.elapsed();
    // The last commits' guards release on the conveyor's fan-out, which
    // completes before `commit_tx` returns to the committer — so every
    // op's hold sample has landed by now.
    let f1 = lock_phase_json();
    let (w1, ws1, wb1) = phase(&f1, "dlm_guard_wait");
    let (h1, _, hb1) = phase(&f1, "dlm_guard_hold");
    let wb = bucket_delta(&wb1, &wb0);
    let hb = bucket_delta(&hb1, &hb0);
    PhaseRow {
        name,
        ops: threads * per,
        wall,
        census: census_delta(c0, census()),
        wait_count: w1 - w0,
        wait_sum_ns: ws1 - ws0,
        wait_p50: pct(&wb, 0.50),
        wait_p99: pct(&wb, 0.99),
        hold_count: h1 - h0,
        hold_p50: pct(&hb, 0.50),
        hold_p99: pct(&hb, 0.99),
    }
}

/// The closure law over one row: every contended 4a acquire was
/// classified exactly once.
fn assert_closure(row: &PhaseRow) {
    let census_total = row.census.dlm_inode.0
        + row.census.dlm_inode.1
        + row.census.dlm_dentry.0
        + row.census.dlm_dentry.1;
    assert_eq!(
        row.wait_count, census_total,
        "[{}] dlm_guard_wait.count ({}) ≡ Σ 4a census ({}) — one wait sample per contended 4a acquire",
        row.name, row.wait_count, census_total
    );
}

const MODE: u32 = libc::S_IFREG | 0o644;

/// The 4a scoping D-3 landed (`fan_out` drops the guards BEFORE the ack):
/// a SOLO sequential committer can never contend with anyone, so its
/// exclusive re-acquisition of the same key (mkdir after mkdir under one
/// parent takes `I{1}` exclusive every time) must record ZERO 4a waits.
/// Before the scoping the ack raced the lane's guard drop and the
/// committer's next op parked on its own previous tx (`dlm_inode_key_
/// waits`, p50 ≤ 16 µs) on a timing-dependent fraction of ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_sequential_committer_never_waits_on_its_own_previous_tx() {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    let (routed, _vol) = sandbox().await;
    let c0 = census();
    let f0 = squeezefs::fuse_client::lock_phase_json();
    let n = if cfg!(debug_assertions) { 256 } else { 2048 };
    for i in 0..n {
        routed
            .create(1, &format!("d{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir");
    }
    let f1 = squeezefs::fuse_client::lock_phase_json();
    let d = census_delta(c0, census());
    assert_eq!(
        (d.dlm_inode, d.dlm_dentry),
        ((0, 0), (0, 0)),
        "a solo sequential committer never records a 4a wait — the guards drop before the ack"
    );
    let (w0, _, _) = phase(&f0, "dlm_guard_wait");
    let (w1, _, _) = phase(&f1, "dlm_guard_wait");
    assert_eq!(w1 - w0, 0, "no dlm_guard_wait sample either");
    let (h0, _, _) = phase(&f0, "dlm_guard_hold");
    let (h1, _, _) = phase(&f1, "dlm_guard_hold");
    assert_eq!(h1 - h0, n as u64, "one exclusive I-guard hold per mkdir");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn storm_census_convicts_the_colliding_table() {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    let (threads, per) = storm_shape();
    let width = squeezefs::stripe_locks::dlm_stripe_width();
    let (routed, _vol) = sandbox().await;
    println!(
        "[storm w={width}] threads={threads} per_thread={per} possible_cpus={} q_depth_desired={} (SQUEEZEFS_DLM_STRIPES={:?})",
        squeezefs::cpu::possible_cpus(),
        fuse3::raw::Q_DEPTH_DESIRED,
        std::env::var("SQUEEZEFS_DLM_STRIPES").ok()
    );

    // ---- MANY-DIRS (the mfcreate shape): per-writer parents, minted
    // outside the timed window.
    let mut dirs = Vec::with_capacity(threads);
    for w in 0..threads {
        let d = routed
            .create(1, &format!("t{w}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir");
        dirs.push(d.ino);
    }
    let dirs = Arc::new(dirs);

    let r = routed.clone();
    let d = dirs.clone();
    let md_create = run_phase("manydirs/create", threads, per, move |w, i| {
        let r = r.clone();
        let d = d.clone();
        async move {
            r.create(d[w], &format!("f{i}"), MODE, 0, 0)
                .await
                .expect("create");
        }
    })
    .await;
    let r = routed.clone();
    let d = dirs.clone();
    let md_rename = run_phase("manydirs/rename", threads, per, move |w, i| {
        let r = r.clone();
        let d = d.clone();
        async move {
            r.rename(d[w], &format!("f{i}"), d[w], &format!("r{i}"), 0)
                .await
                .expect("rename");
        }
    })
    .await;
    let r = routed.clone();
    let d = dirs.clone();
    let md_unlink = run_phase("manydirs/unlink", threads, per, move |w, i| {
        let r = r.clone();
        let d = d.clone();
        async move {
            r.unlink(d[w], &format!("r{i}")).await.expect("unlink");
        }
    })
    .await;

    // ---- ONE-DIR (the shared-dir interleave): writer w takes names
    // i = w, w+T, … in dir 1.
    let r = routed.clone();
    let od_create = run_phase("onedir/create", threads, per, move |w, i| {
        let r = r.clone();
        async move {
            r.create(1, &format!("s{}", i * threads + w), MODE, 0, 0)
                .await
                .expect("create");
        }
    })
    .await;
    let r = routed.clone();
    let od_rename = run_phase("onedir/rename", threads, per, move |w, i| {
        let r = r.clone();
        async move {
            let n = i * threads + w;
            r.rename(1, &format!("s{n}"), 1, &format!("q{n}"), 0)
                .await
                .expect("rename");
        }
    })
    .await;
    let r = routed.clone();
    let od_unlink = run_phase("onedir/unlink", threads, per, move |w, i| {
        let r = r.clone();
        async move {
            r.unlink(1, &format!("q{}", i * threads + w))
                .await
                .expect("unlink");
        }
    })
    .await;

    let rows = [
        &md_create, &md_rename, &md_unlink, &od_create, &od_rename, &od_unlink,
    ];
    for row in rows {
        row.print(width);
        assert_closure(row);
        assert_eq!(
            row.census.block_flush,
            (0, 0),
            "[{}] a metadata storm touches no block stripe",
            row.name
        );
        assert_eq!(
            row.census.serve_ino,
            (0, 0),
            "[{}] no publish plane is armed",
            row.name
        );
        assert_eq!(
            row.census.lease_waiter,
            (0, 0),
            "[{}] no lease is taken",
            row.name
        );
    }
    // MANY-DIRS: distinct parents, distinct names — no two committers
    // ever ask for one key, so every 4a wait is the table's (a
    // collision), never the workload's. The census under-reports a
    // collision as a key wait only when a FIRST-TOUCH holder is preempted
    // between its grant and its stamp for longer than the bounded spin —
    // a shared box can do that to a few, never to the population.
    for row in [&md_create, &md_rename] {
        let coll = row.census.dlm_inode.0 + row.census.dlm_dentry.0;
        let keyw = row.census.dlm_inode.1 + row.census.dlm_dentry.1;
        assert!(
            keyw <= 1 + (coll + keyw) / 8,
            "[{}] many-dirs shape has no same-key 4a wait: {keyw} key waits vs {coll} collisions (I {:?} D {:?})",
            row.name,
            row.census.dlm_inode,
            row.census.dlm_dentry
        );
    }
    // Unlink is TWO-PHASE (discover under `I{parent}` shared + D, drop,
    // re-lock the full set): a stripe-mate that queued behind phase 1 makes
    // the phase-2 re-acquire a FIFO-courtesy refusal whose last-acquirer
    // word is the asker's OWN key — a collision-caused wait the census
    // files as a key wait. Each such wait needs a queued stripe-mate, i.e.
    // one collision event, so it is bounded by the collision count.
    {
        let coll = md_unlink.census.dlm_inode.0 + md_unlink.census.dlm_dentry.0;
        let keyw = md_unlink.census.dlm_inode.1 + md_unlink.census.dlm_dentry.1;
        assert!(
            keyw <= 1 + coll,
            "[manydirs/unlink] key waits ({keyw}) are the two-phase re-acquire artifact, bounded by collisions ({coll})"
        );
    }
    // ONE-DIR rename: every rename takes `I{1}` EXCLUSIVE — the parent
    // lock IS the workload's serialization, so the I-class must record
    // KEY waits (the crucible the census must never misfile as width).
    assert!(
        od_rename.census.dlm_inode.1 > 0,
        "one-dir rename storm serializes on I{{1}}: key waits expected, saw {:?}",
        od_rename.census.dlm_inode
    );
}
