//! PR M4 — FUSE handler teardown contracts (design-metadata-throughput
//! §5.1 D1.b + D1.c), tests-first.
//!
//! Encodes the behaviors the M4 implementation must deliver:
//!
//! 1. **The per-op `timeout()` future-drop vector is RETIRED** (D1.b, the
//!    M4 → M7 load-bearing edge): a mutation whose `commit_tx` stalls past
//!    `SQUEEZEFS_TIMEOUT` **completes** — no `ETIMEDOUT` synthesis, no
//!    future drop mid-commit (a drop between ring admission and
//!    reservation leaks admitted budget forever — `journal_core.rs`
//!    `#[must_use]` Admission; an uncompleted registered reservation
//!    wedges `completed_upto`). The op is instead **visible to the
//!    watchdog scan** while in flight.
//! 2. **Watchdog registry mechanics**: overdue ops are reported with op
//!    detail (kind, ino, age) + counted in
//!    `METRICS.fuse_op_watchdog_overdue`; op exit clears the slot;
//!    fresh ops under threshold are not reported.
//! 3. **Ring-admission-park escalation** (the audit table's
//!    `disabled_volumes` rung): a committer parked ≥ `SQUEEZEFS_TIMEOUT`
//!    on a wedged-not-failed ring trips `note_journal_failure` until the
//!    volume fail-stops — the op errs (bounded), `is_failed()` latches,
//!    and the routed layer mirrors the volume into `disabled_volumes`.
//! 4. **Bounded FLUSH/FSYNC barrier waits** (audit row 1): the
//!    `SyncCoalescer` bound lives INSIDE the coalescer — a hung leader
//!    `sync_fn` produces a synthesized error within the bound for the
//!    whole batch (leader + followers), and the coalescer is NOT wedged
//!    afterwards (`flushing` resets; the next barrier runs).
//! 5. **Parent generation counters** (D1.c): every entry-set mutation
//!    bumps the affected directory generations (create/mkdir/unlink/
//!    rename both parents/rmdir parent + victim), and readdir snapshot
//!    coherence holds — including concurrent readdir-during-create.
//!
//! Runs against local file-backed sandboxes (no root, no mount). This
//! binary deliberately arms `SQUEEZEFS_TIMEOUT=1` (memoized process-wide
//! at first use) so old-world timeout behavior is observable at test
//! speed; tests pass explicit thresholds to the watchdog primitives.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use futures::StreamExt;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{op_watchdog_tick, FuseOpKind, OpProf, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, TEST_COMMIT_ADMITTED_STALL_MS};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
use squeezefs::meta_backend::sync_coalescer::SyncCoalescer;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Arm the short process-wide timeout knob BEFORE anything memoizes it.
/// Every test calls this first; under `--test-threads=1` the first caller
/// wins and the whole binary runs with a 1 s `SQUEEZEFS_TIMEOUT`.
fn arm_short_timeout() {
    std::env::set_var("SQUEEZEFS_TIMEOUT", "1");
}

async fn open_v3_meta(path: &std::path::Path, len: u64) -> Arc<KvMetaBackend> {
    open_v3_meta_with_ring(path, len, None).await
}

async fn open_v3_meta_with_ring(
    path: &std::path::Path,
    len: u64,
    journal_len_override: Option<u64>,
) -> Arc<KvMetaBackend> {
    format_v3(
        path,
        len,
        &FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

/// The `meta_lv_fuse_tests` harness shape: a full `SqueezefsFilesystem`
/// against file-backed sandboxes, handlers driven directly (no mount).
struct FsSandbox {
    fs: SqueezefsFilesystem,
    _meta: NamedTempFile,
    _backing: NamedTempFile,
    _staging: TempDir,
}

async fn fs_sandbox() -> FsSandbox {
    let dlm = DlmClient::new().unwrap();

    let backing = NamedTempFile::new().unwrap();
    backing.as_file().set_len(64 * 1024 * 1024).unwrap();
    let nvme_dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));

    let block_alloc = Arc::new(
        BlockAllocator::new("m4_watchdog_tests")
            .await
            .expect("BlockAllocator init"),
    );

    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(meta.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    FsSandbox {
        fs,
        _meta: meta,
        _backing: backing,
        _staging: staging,
    }
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 4242,
    }
}

/// Collect one full listing of `dir` through the readdir handler
/// (paging by resume cookie until a short page), names only.
async fn readdir_names(fs: &SqueezefsFilesystem, dir: u64) -> Vec<String> {
    let mut names = Vec::new();
    let mut offset: i64 = 0;
    loop {
        let reply = fs
            .readdir(req(), dir, 0, offset)
            .await
            .expect("readdir handler");
        let entries: Vec<_> = reply.entries.collect().await;
        if entries.is_empty() {
            break;
        }
        let mut last_offset = offset;
        for e in &entries {
            let e = e.as_ref().expect("dir entry");
            let name = e.name.to_string_lossy().to_string();
            last_offset = e.offset;
            if name != "." && name != ".." && name != ".config" && name != ".stats" {
                names.push(name);
            }
        }
        if last_offset == offset {
            break; // no forward progress ⇒ end of stream
        }
        offset = last_offset;
    }
    names
}

// ===========================================================================
// (2) Watchdog registry mechanics — the scan primitive.
// ===========================================================================

#[test]
fn watchdog_tick_reports_overdue_with_op_detail_and_clears_on_exit() {
    arm_short_timeout();
    let overdue_before = METRICS.fuse_op_watchdog_overdue.load(Ordering::Relaxed);

    let op = OpProf::begin_forced(FuseOpKind::Create, 424_242);
    // Threshold zero: every in-flight op is overdue by definition.
    let report = op_watchdog_tick(Duration::ZERO);
    assert!(
        report.iter().any(|o| o.op == "create" && o.ino == 424_242),
        "an in-flight create past the threshold must be reported with op detail; got {report:?}"
    );
    assert!(
        METRICS.fuse_op_watchdog_overdue.load(Ordering::Relaxed) > overdue_before,
        "each overdue observation must count fuse_op_watchdog_overdue"
    );

    // A generous threshold reports nothing for a fresh op.
    let report = op_watchdog_tick(Duration::from_secs(3600));
    assert!(
        !report.iter().any(|o| o.ino == 424_242),
        "a fresh op must not be reported under a 1 h threshold; got {report:?}"
    );

    // Op exit clears the slot: no ghost reports.
    drop(op);
    let report = op_watchdog_tick(Duration::ZERO);
    assert!(
        !report.iter().any(|o| o.ino == 424_242),
        "a completed op must leave the registry; got {report:?}"
    );
}

// ===========================================================================
// (1) The commit_tx no-drop pin — the M4 → M7 load-bearing edge.
// ===========================================================================

/// A create whose commit stalls past `SQUEEZEFS_TIMEOUT` (1 s in this
/// binary) inside the admission hazard window must COMPLETE (no
/// ETIMEDOUT, no future drop) and be visible to the watchdog while in
/// flight. Pre-M4 this fails twice: the handler synthesizes ETIMEDOUT at
/// 1 s (dropping the commit future mid-stall — the budget-leak vector),
/// and the op never appears in the registry because registration was
/// profile-gated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_commit_completes_and_watchdog_logs_instead_of_timeout_drop() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = sandbox.fs.clone();

    TEST_COMMIT_ADMITTED_STALL_MS.store(2500, Ordering::SeqCst);
    let create_task = {
        let fs = fs.clone();
        tokio::spawn(async move {
            fs.create(req(), 1, OsStr::new("slow_commit.txt"), 0o644, 0)
                .await
        })
    };

    // While the create is parked in its stalled commit, the watchdog scan
    // must see it (bounded poll loop on an observable, not sleep-sync).
    let mut watchdog_saw_create = false;
    let poll_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !create_task.is_finished() && std::time::Instant::now() < poll_deadline {
        if op_watchdog_tick(Duration::from_millis(100))
            .iter()
            .any(|o| o.op == "create" && o.ino == 1)
        {
            watchdog_saw_create = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let res = tokio::time::timeout(Duration::from_secs(30), create_task)
        .await
        .expect("create must finish (stall is 2.5 s)")
        .expect("create task must not panic");
    TEST_COMMIT_ADMITTED_STALL_MS.store(0, Ordering::SeqCst);

    assert!(
        watchdog_saw_create,
        "an over-threshold in-flight create must be visible to the watchdog scan \
         (always-on op registration, D1.b)"
    );
    assert!(
        res.is_ok(),
        "a slow commit must COMPLETE after M4 — per-op timeout() synthesis of \
         ETIMEDOUT (which drops the commit future mid-flight and leaks admitted \
         journal budget) is retired; got {res:?}"
    );

    // The commit really landed (nothing was dropped mid-pipeline).
    let looked_up = fs.lookup(req(), 1, OsStr::new("slow_commit.txt")).await;
    assert!(
        looked_up.is_ok(),
        "the stalled-but-completed create must be durable/visible: {looked_up:?}"
    );
}

// ===========================================================================
// (3) Ring-admission-park escalation → disabled_volumes (audit row 2).
// ===========================================================================

/// Wedge a volume's ring (hold every admittable byte, so the drain can
/// never free budget) and prove a parked committer escalates: errs out
/// bounded, latches `failed`, and the routed layer mirrors the volume
/// into `disabled_volumes`. Pre-M4 the committer parks forever (the
/// wedged-not-failed class the audit names) and this test's outer bound
/// trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_park_past_threshold_escalates_to_disabled_volume() {
    arm_short_timeout(); // read at open: the escalation threshold (1 s)
    let meta = NamedTempFile::new().unwrap();
    // Tiny ring (the R10 liveness-storm recipe): 512 KiB, floor is 384 KiB.
    let kv = open_v3_meta_with_ring(meta.path(), 64 * 1024 * 1024, Some(512 * 1024)).await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));

    // Saturate the user admission budget and HOLD it: reusable_upto
    // advances cannot reclaim admitted-but-unreserved budget, so the ring
    // is wedged-not-failed for every later committer.
    let mut held = Vec::new();
    for chunk in [4096u64, 64] {
        while let Some(adm) = kv
            .journal_ring()
            .core()
            .try_admit(chunk, AdmissionClass::User)
        {
            held.push(adm);
            assert!(held.len() < 1_000_000, "admission accounting runaway");
        }
    }
    assert!(!held.is_empty(), "the tiny ring must saturate");

    let create_res = tokio::time::timeout(
        Duration::from_secs(20),
        routed.create(ROOT_INO, "parked.txt", libc::S_IFREG | 0o644, 0, 0),
    )
    .await;

    assert!(
        create_res.is_ok(),
        "a committer parked on a wedged ring must ESCALATE within the \
         SQUEEZEFS_TIMEOUT lattice (~3 crossings at 1 s here), never park forever \
         (pre-M4 behavior)"
    );
    let create_res = create_res.unwrap();
    assert!(
        create_res.is_err(),
        "the escalated committer must fail loud, not silently succeed"
    );
    assert!(
        kv.is_failed(),
        "parked-past-threshold must trip note_journal_failure until the volume \
         fail-stops (design §5.1 D1.b table row 2)"
    );
    assert!(
        routed.check_volume_enabled(0).is_err(),
        "the routed layer must mirror the failed volume into disabled_volumes"
    );
    assert!(
        kv.journal_full_stalls() > 0,
        "the park itself must be counted (meta_kv_journal_full_stalls)"
    );

    // Hygiene: give the budget back before teardown (the volume is failed
    // either way; this keeps the accounting audit clean).
    for adm in held {
        kv.journal_ring().core().release(adm);
    }
}

// ===========================================================================
// (4) Bounded FLUSH/FSYNC barrier waits (audit row 1) — inside the
//     coalescer, leader and follower, with no post-timeout wedge.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_barrier_errs_on_hung_leader_and_coalescer_recovers() {
    arm_short_timeout();
    let c = Arc::new(SyncCoalescer::new());

    // Leader whose device barrier never completes.
    let hung = {
        let c = c.clone();
        tokio::spawn(async move {
            c.barrier_bounded(Duration::from_secs(1), || async {
                std::future::pending::<()>().await;
                Ok(())
            })
            .await
        })
    };
    let res = tokio::time::timeout(Duration::from_secs(15), hung).await;
    assert!(
        res.is_ok(),
        "a hung sync_fn must produce a synthesized error within the bound \
         (leader-side bounded wait, audit row 1) — the barrier never returned"
    );
    let res = res.unwrap().expect("barrier task must not panic");
    assert!(
        res.is_err(),
        "the bounded-out barrier must surface an error to its caller"
    );

    // The coalescer must NOT be wedged (flushing latched true) after a
    // bounded-out leader: a fresh fast barrier succeeds.
    let ok = tokio::time::timeout(
        Duration::from_secs(15),
        c.barrier_bounded(Duration::from_secs(1), || async { Ok(()) }),
    )
    .await;
    assert!(
        ok.is_ok(),
        "a barrier after a bounded-out leader must run (no flushing=true wedge)"
    );
    assert!(ok.unwrap().is_ok(), "the fast barrier must succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_barrier_bounds_followers_behind_a_slow_leader() {
    arm_short_timeout();
    let c = Arc::new(SyncCoalescer::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let started = Arc::new(tokio::sync::Notify::new());

    // Leader with a long-but-not-hung barrier (released by the test).
    let leader = {
        let c = c.clone();
        let gate = gate.clone();
        let started = started.clone();
        tokio::spawn(async move {
            c.barrier_bounded(Duration::from_secs(60), || {
                let gate = gate.clone();
                let started = started.clone();
                async move {
                    started.notify_one();
                    let permit = gate.acquire().await.unwrap();
                    permit.forget();
                    Ok(())
                }
            })
            .await
        })
    };
    started.notified().await; // leader is inside sync_fn

    // Follower with a short bound: must err out within ITS bound even
    // though the leader's barrier is still in flight.
    let follower = {
        let c = c.clone();
        tokio::spawn(async move {
            c.barrier_bounded(Duration::from_millis(200), || async { Ok(()) })
                .await
        })
    };
    let follower_res = tokio::time::timeout(Duration::from_secs(15), follower).await;
    assert!(
        follower_res.is_ok(),
        "a follower's bounded wait must fire within its own bound while the \
         leader's barrier is still in flight"
    );
    assert!(
        follower_res.unwrap().expect("no panic").is_err(),
        "the bounded-out follower must surface a synthesized error"
    );

    // Release the leader: its own barrier completes fine. TWO permits —
    // the bounded-out follower's oneshot sender is still queued (only the
    // receiver died), so the leader correctly serves it a second batch
    // whose fan-out lands in the dead oneshot (harmless by design).
    gate.add_permits(2);
    let leader_res = tokio::time::timeout(Duration::from_secs(15), leader)
        .await
        .expect("leader must finish once released")
        .expect("no panic");
    assert!(leader_res.is_ok(), "the released leader's barrier succeeds");
}

// ===========================================================================
// (5) Parent generation counters (D1.c) + readdir snapshot coherence.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entry_set_mutations_bump_parent_generations() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = &sandbox.fs;

    // mkdir bumps the parent (root).
    let g_root_0 = fs.dir_generation(1);
    let dir = fs
        .mkdir(req(), 1, OsStr::new("gen_dir"), 0o755, 0)
        .await
        .expect("mkdir")
        .attr
        .ino;
    let g_root_1 = fs.dir_generation(1);
    assert!(
        g_root_1 > g_root_0,
        "mkdir must bump the parent's readdir generation ({g_root_0} → {g_root_1})"
    );

    // create bumps its parent.
    let g_dir_0 = fs.dir_generation(dir);
    fs.create(req(), dir, OsStr::new("a.txt"), 0o644, 0)
        .await
        .expect("create");
    let g_dir_1 = fs.dir_generation(dir);
    assert!(
        g_dir_1 > g_dir_0,
        "create must bump the parent's readdir generation ({g_dir_0} → {g_dir_1})"
    );

    // rename bumps BOTH parents.
    let dir2 = fs
        .mkdir(req(), 1, OsStr::new("gen_dir2"), 0o755, 0)
        .await
        .expect("mkdir2")
        .attr
        .ino;
    let (g_src_0, g_dst_0) = (fs.dir_generation(dir), fs.dir_generation(dir2));
    fs.rename(req(), dir, OsStr::new("a.txt"), dir2, OsStr::new("b.txt"))
        .await
        .expect("rename");
    assert!(
        fs.dir_generation(dir) > g_src_0,
        "rename must bump the source parent's generation"
    );
    assert!(
        fs.dir_generation(dir2) > g_dst_0,
        "rename must bump the destination parent's generation"
    );

    // unlink bumps its parent.
    let g_dst_1 = fs.dir_generation(dir2);
    fs.unlink(req(), dir2, OsStr::new("b.txt"))
        .await
        .expect("unlink");
    assert!(
        fs.dir_generation(dir2) > g_dst_1,
        "unlink must bump the parent's generation"
    );

    // rmdir bumps the parent AND the removed directory itself (its own
    // snapshot must die with it — the old code invalidated both).
    let g_root_2 = fs.dir_generation(1);
    let g_victim_0 = fs.dir_generation(dir);
    fs.rmdir(req(), 1, OsStr::new("gen_dir"))
        .await
        .expect("rmdir");
    assert!(
        fs.dir_generation(1) > g_root_2,
        "rmdir must bump the parent's generation"
    );
    assert!(
        fs.dir_generation(dir) > g_victim_0,
        "rmdir must bump the victim directory's own generation"
    );
}

/// Readdir must never serve a pre-mutation snapshot after the mutation's
/// handler returned: the create-then-list sequence (the moka-invalidate
/// behavior this PR replaces with generation keys) stays exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readdir_snapshot_never_survives_a_mutation() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = &sandbox.fs;

    let dir = fs
        .mkdir(req(), 1, OsStr::new("coherent"), 0o755, 0)
        .await
        .expect("mkdir")
        .attr
        .ino;

    fs.create(req(), dir, OsStr::new("one"), 0o644, 0)
        .await
        .expect("create one");
    let listing = readdir_names(fs, dir).await;
    assert_eq!(listing, vec!["one".to_string()], "first snapshot exact");

    // The listing above snapshotted the directory; this create must be
    // visible to the very next readdir (stale snapshot dies by key).
    fs.create(req(), dir, OsStr::new("two"), 0o644, 0)
        .await
        .expect("create two");
    let mut listing = readdir_names(fs, dir).await;
    listing.sort();
    assert_eq!(
        listing,
        vec!["one".to_string(), "two".to_string()],
        "readdir after create must include the new entry"
    );

    fs.unlink(req(), dir, OsStr::new("one"))
        .await
        .expect("unlink one");
    let listing = readdir_names(fs, dir).await;
    assert_eq!(
        listing,
        vec!["two".to_string()],
        "readdir after unlink must drop the dead entry"
    );
}

/// Concurrent readdir-during-create coherence (the mission's named case):
/// 4 writers storm one directory while 2 readers list it continuously;
/// every mid-flight listing must be internally consistent (no duplicate
/// names — a snapshot is a point-in-time set) and the final listing is
/// exactly the created set.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_readdir_during_create_is_coherent() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = sandbox.fs.clone();

    let dir = fs
        .mkdir(req(), 1, OsStr::new("storm"), 0o755, 0)
        .await
        .expect("mkdir")
        .attr
        .ino;

    const WRITERS: usize = 4;
    const PER_WRITER: usize = 50;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..2 {
        let fs = fs.clone();
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let mut listings = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let names = readdir_names(&fs, dir).await;
                let mut dedup = names.clone();
                dedup.sort();
                dedup.dedup();
                assert_eq!(
                    dedup.len(),
                    names.len(),
                    "a mid-storm readdir listing must never contain duplicates"
                );
                listings += 1;
            }
            listings
        }));
    }

    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let fs = fs.clone();
        writers.push(tokio::spawn(async move {
            for i in 0..PER_WRITER {
                fs.create(req(), dir, OsStr::new(&format!("f{w}_{i}")), 0o644, 0)
                    .await
                    .expect("storm create");
            }
        }));
    }
    for w in writers {
        w.await.expect("writer");
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let listings = r.await.expect("reader");
        assert!(listings > 0, "readers must have listed at least once");
    }

    let mut final_listing = readdir_names(&fs, dir).await;
    final_listing.sort();
    let mut expected: Vec<String> = (0..WRITERS)
        .flat_map(|w| (0..PER_WRITER).map(move |i| format!("f{w}_{i}")))
        .collect();
    expected.sort();
    assert_eq!(
        final_listing, expected,
        "the final readdir must list exactly the created set"
    );
}

// ===========================================================================
// (6) VL8 item 2 — the 013/464 async-write wedge's two structural defects
//     (live capture 2026-07-21: /tmp/vl8_fstests/wedge_* evidence pack).
// ===========================================================================

/// `copy_file_range` orders its two inode guards by RAW INO, but the
/// guards live in a hash-STRIPED table: raw-ino order is NOT a total
/// order on the lock instances, so two concurrent cfr ops can acquire
/// the same two stripes in opposite sequence — ABBA, and every op whose
/// ino hashes onto the held/waited stripes wedges behind it forever.
/// The acquisition sequence must be monotone in SHARD INDEX.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cfr_pair_guard_order_is_shard_monotone() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = &sandbox.fs;

    // Search a small ino range for a pair whose raw-ino order INVERTS the
    // shard order (guaranteed to exist: the shard map is a hash).
    let mut checked = 0u32;
    for a in 2u64..2000 {
        for b in (a + 1)..(a + 64).min(2000) {
            let (first, second) = fs.inode_pair_lock_order(a, b);
            let s_first = fs.active_inode_locks.shard_index(first);
            let s_second = fs.active_inode_locks.shard_index(second);
            if s_first != s_second {
                checked += 1;
                assert!(
                    s_first < s_second,
                    "guard sequence for inos ({a},{b}) = ({first},{second}) acquires \
                     shard {s_first} before shard {s_second} — raw-ino order over a \
                     hash-striped lock table is ABBA-capable (VL8 item 2)"
                );
            }
        }
    }
    assert!(checked > 1000, "the search must actually exercise pairs");
}

/// A `copy_file_range` blocked on its inode guards must be VISIBLE to the
/// op watchdog (kind + ino). In the live wedge the two deadlocked cfr
/// handlers were invisible — every diagnosis started from the victims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cfr_registers_with_watchdog_while_guard_blocked() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = sandbox.fs.clone();

    let src = fs
        .create(req(), 1, OsStr::new("cfr_wd_src.txt"), 0o644, 0)
        .await
        .expect("create src")
        .attr
        .ino;
    let dst = fs
        .create(req(), 1, OsStr::new("cfr_wd_dst.txt"), 0o644, 0)
        .await
        .expect("create dst")
        .attr
        .ino;

    // Hold the SOURCE's inode guard: the cfr blocks at guard acquisition.
    let src_guard = fs.get_inode_lock(src).write().await;

    let cfr_task = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.copy_file_range(req(), src, 0, 0, dst, 0, 0, 16, 0).await })
    };

    let mut seen = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !cfr_task.is_finished() {
        if op_watchdog_tick(Duration::ZERO)
            .iter()
            .any(|o| o.op == "copy_file_range" && o.ino == src)
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        seen,
        "a guard-blocked copy_file_range must appear in the watchdog registry \
         (kind copy_file_range, ino {src}) — the live wedge's holders were invisible"
    );

    drop(src_guard);
    let _ = tokio::time::timeout(Duration::from_secs(10), cfr_task)
        .await
        .expect("cfr must complete once the guard is released")
        .expect("cfr task join");
}

/// Same contract for `fallocate` (the other watchdog-blind op class the
/// live wedge capture caught permanently in flight).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fallocate_registers_with_watchdog_while_guard_blocked() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = sandbox.fs.clone();

    let ino = fs
        .create(req(), 1, OsStr::new("falloc_wd.txt"), 0o644, 0)
        .await
        .expect("create")
        .attr
        .ino;

    let guard = fs.get_inode_lock(ino).write().await;
    let task = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.fallocate(req(), ino, 0, 0, 4096, 0).await })
    };

    let mut seen = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !task.is_finished() {
        if op_watchdog_tick(Duration::ZERO)
            .iter()
            .any(|o| o.op == "fallocate" && o.ino == ino)
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        seen,
        "a guard-blocked fallocate must appear in the watchdog registry (ino {ino})"
    );

    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("fallocate must complete once the guard is released")
        .expect("fallocate task join");
}

// ===========================================================================
// (7) VL8 item 9 — the syncfs transient-ENOENT vector: a kernel-writeback
//     WRITE against a destroyed (unlinked + reclaimed) ino.
// ===========================================================================

/// With the writeback cache + clean-handle FLUSH elision, the kernel can
/// flush dirty pages AFTER close+unlink destroyed the ino. That WRITE
/// used to fail ENOENT — which lands in the superblock errseq and makes
/// the NEXT `syncfs` report ENOENT once on a perfectly healthy mount
/// (the VL7-rig leg-12 transient; REPRODUCED live on this rig 2026-07-21).
/// Unlink IS the discard authority for that data: a WRITE whose ino is
/// verified-NotFound must be answered as a counted discard
/// (`writeback_orphan_discards` — the FIND-M11-A remount-law analogue),
/// never an errno that poisons the sb errseq.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_to_reclaimed_ino_is_a_counted_discard_not_enoent() {
    arm_short_timeout();
    let sandbox = fs_sandbox().await;
    let fs = sandbox.fs.clone();
    // The reclaim worker pool arms in init() (mount does this for real).
    fs.init(req()).await.expect("init");

    let ino = fs
        .create(req(), 1, OsStr::new("wb_orphan.txt"), 0o644, 0)
        .await
        .expect("create")
        .attr
        .ino;
    // Some real dirty-page-era content while the file lives.
    fs.write(
        req(),
        ino,
        0,
        0,
        bytes::Bytes::from(vec![0xAAu8; 4096]),
        0,
        0,
    )
    .await
    .expect("write to a live file");

    // Close the create-handle (create counted an open; an open ino is
    // never reclaimed), then unlink + FORGET like the kernel would.
    fs.release(req(), ino, ino, 0, 0, false)
        .await
        .expect("release");
    fs.unlink(req(), 1, OsStr::new("wb_orphan.txt"))
        .await
        .expect("unlink");
    // Destroy is deferred until the kernel's FORGET drops the nlookup —
    // deliver it (the reclaim worker then destroys the inode).
    fs.forget(req(), ino, u64::MAX).await;

    // Wait until the reclaim actually destroyed the ino (verified
    // NotFound — the discard license).
    let backend = fs.meta_backend.clone().expect("backend");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match backend.getattr(ino).await {
            Err(_) => break,
            Ok(_) if std::time::Instant::now() > deadline => {
                panic!("unlinked ino {ino} was never reclaimed — fixture assumption broke")
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

    // The kernel's deferred writeback WRITE arrives after destruction.
    let before = METRICS.writeback_orphan_discards.load(Ordering::Relaxed);
    let reply = fs
        .write(
            req(),
            ino,
            0,
            0,
            bytes::Bytes::from(vec![0xBBu8; 4096]),
            0,
            0,
        )
        .await
        .expect(
            "a writeback WRITE against a verified-reclaimed ino must be a \
             counted DISCARD (unlink is the discard authority), not an errno \
             that poisons the superblock errseq into a spurious syncfs failure",
        );
    assert_eq!(reply.written, 4096, "the discard acks the full payload");
    assert!(
        METRICS.writeback_orphan_discards.load(Ordering::Relaxed) > before,
        "the discard must be COUNTED (writeback_orphan_discards) — silent \
         success would hide real custody loss"
    );
}
