//! RES-8 (pre-RC engineering spec §7): a detached data-path task that
//! panics must be CONTAINED, COUNTED and LOUD — never silently lost.
//!
//! The named anchor is the write ACK path: the completing WRITE replies
//! with the block's custody parked and the durable upload rides a
//! detached `tpc_spawn`. A panic in that task loses the block's
//! write-back with **no counter at all** — and because the phase
//! histogram is recorded at the END of the task body, the failure shows
//! up as an *under-report* (fewer `Total` samples) rather than as a
//! failure. The same shape repeats across ~15 data-path sites: the IPC
//! ring handoffs, the bg-admit prefetch fills, the writeback retry
//! worker, the R5 parked-drain worker, the extent-fold worker, the
//! dehydration workers, the epoch sweeper. Several of those are
//! LOOPS — one panic ends that machinery for the life of the mount,
//! invisibly.
//!
//! Contracts pinned here:
//!
//! 1. **Contained + counted**: a panicking detached body bumps
//!    `detached_task_panics` and does not propagate.
//! 2. **The lane survives**: a panic on a fuse3 handler lane does not
//!    stop that lane serving subsequent work (the write ACK path's
//!    venue is the same lane every kernel handler runs on).
//! 3. **Zero cost on the happy path**: a body that returns normally
//!    moves no counter.
//! 4. **The class is closed mechanically**: no `src/` site calls
//!    `fuse3::raw::tpc_spawn` directly any more — the guarded wrapper is
//!    the only door (the R-6 unified-purge grep-guard precedent).
//!
//! RED against dev 7d1ec2e1: `crate::detached` and
//! `detached_task_panics` do not exist, and 5 `src/` sites call
//! `fuse3::raw::tpc_spawn` bare.

use squeezefs::detached;
use squeezefs::fuse_client::METRICS;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn panics() -> u64 {
    METRICS.detached_task_panics.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Contracts 1 + 3 — contained, counted, free when nothing panics.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contained_body_counts_the_panic_and_does_not_propagate() {
    let before = panics();
    detached::contain("res8_unit", async {
        panic!("RES-8 unit: injected detached-task panic");
    })
    .await;
    assert_eq!(
        panics() - before,
        1,
        "RES-8: a detached panic must be counted (detached_task_panics)"
    );

    let before = panics();
    let ran = Arc::new(AtomicU64::new(0));
    let r = ran.clone();
    detached::contain("res8_unit_ok", async move {
        r.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the body still runs");
    assert_eq!(panics(), before, "a clean body moves no counter");
}

// ---------------------------------------------------------------------------
// Contract 2 — the handler lane survives a panicking detached task.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_lane_survives_a_panicking_detached_task() {
    let before = panics();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    detached::tpc_spawn_guarded("res8_lane_panic", async {
        panic!("RES-8 lane: injected detached-task panic");
    });
    detached::tpc_spawn_guarded("res8_lane_after", async move {
        let _ = tx.send(());
    });

    tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .expect("RES-8: the handler lane must keep serving after a panic")
        .expect("the follow-up body must run");

    // The panic is accounted even though nothing joined the task.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while panics() == before {
        assert!(
            std::time::Instant::now() < deadline,
            "RES-8: the lane panic was never counted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Contract 4 — the class is closed mechanically (grep guard).
// ---------------------------------------------------------------------------

#[test]
fn no_src_site_calls_tpc_spawn_bare() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // The wrapper itself is the ONE sanctioned caller.
            if path.file_name().and_then(|f| f.to_str()) == Some("detached.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read source");
            for (i, line) in src.lines().enumerate() {
                let l = line.trim_start();
                if l.starts_with("//") || l.starts_with("///") {
                    continue;
                }
                if l.contains("fuse3::raw::tpc_spawn(")
                    || l.contains("fuse3::raw::tpc_spawn_on_node(")
                {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "RES-8: detached handler-lane spawns must go through \
         crate::detached (unwind-catching + counted) — a bare tpc_spawn \
         loses its work silently:\n{}",
        offenders.join("\n")
    );
}
