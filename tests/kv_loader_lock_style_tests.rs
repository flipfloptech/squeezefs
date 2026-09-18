//! **One acquisition style per `scc` table in the KV node cache** (PR 12b
//! review round 1, Issue 13 — a SHIPPED-BUG fix on every layout).
//!
//! `NodeCache::load_for_slot` took the single-flight table's bucket ASYNC
//! (`inflight.entry_async(addr).await`) while `InflightLoadGuard::drop`
//! and every other user of the table took it SYNC (`remove_if_sync`). On
//! the two-lane `sqz-meta` pool that pair is a self-deadlock: `saa` hands
//! a released bucket to the next QUEUED waiter, an async waiter GRANTED
//! the bucket resumes only when its task is polled, and both lanes that
//! could poll it sat in a sync wait on that very bucket — the fleet's
//! `-o ro` reader wedged both meta lanes under fsck C1's fan-out (hundreds
//! of slot-tree walks colliding on uncached nodes), `squeezefs fsck` hung
//! 31 minutes and the reader's `umount` hung with it. The loader map is
//! every layout's (a flat volume's C1 walk runs the same loader over its
//! three trees), so the fix is called out as a shipped-bug fix and pinned
//! flat here.
//!
//! The interleaving itself lives inside `saa`'s hand-off (a bucket granted
//! to a parked async waiter) — no seam of ours forces it without a hook
//! inside the lock, so the pin is two-fold: a STATIC rail (the two tables
//! carry no `*_async` acquisition — the law, greppable, red on
//! reintroduction) and a bounded FLAT stress (hundreds of colliding loads
//! through the meta lanes complete under a timeout — a wedge is a red
//! test, never a hang). The stamped twin is `sym_n_daemon_tests::
//! hundreds_of_colliding_node_loads_on_the_meta_lanes_never_wedge`.

mod common;

use common::sym::*;
use std::path::Path;
use std::sync::Arc;

/// The tables whose every acquisition is SYNC, and the async forms that
/// must never appear on them.
const TABLES: &[&str] = &["inflight", "map", "retired"];
const ASYNC_FORMS: &[&str] = &[
    "entry_async(",
    "read_async(",
    "remove_async(",
    "remove_if_async(",
    "update_async(",
    "insert_async(",
    "upsert_async(",
    "contains_async(",
    "get_async(",
];

#[test]
fn the_node_caches_scc_tables_are_taken_in_one_acquisition_style() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(repo.join("src/meta_backend/kv/node_cache.rs"))
        .expect("node_cache.rs readable");
    let mut violations = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        for table in TABLES {
            let needle = format!(".{table}.");
            if !line.contains(&needle) && !line.contains(&format!("self.{table}")) {
                continue;
            }
            for form in ASYNC_FORMS {
                if line.contains(form) {
                    violations.push(format!(
                        "src/meta_backend/kv/node_cache.rs:{}: {}",
                        idx + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "an ASYNC acquisition on a node-cache scc table whose other users take it SYNC — the \
         mixed pair is a self-deadlock on the meta lanes (PR 12b review round 1, Issue 13):\n{}",
        violations.join("\n")
    );
}

/// The FLAT stress: a flat volume, its three trees' roots, a FRESH open
/// (an empty cache — every load MISSES) and hundreds of concurrent loads
/// of the same addresses through the `sqz-meta` pool, some dropping the
/// image again behind the others. Bounded: a wedge is the timeout's
/// red, never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundreds_of_colliding_flat_node_loads_on_the_meta_lanes_never_wedge() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_flat_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // A population wide enough for every tree to have leaves.
    let root = squeezefs::meta_backend::kv::builder::ROOT_INO;
    for i in 0..600u32 {
        let name = format!("f{i}");
        squeezefs::meta_backend::Metadata::create(
            &*routed,
            root,
            &name,
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .expect("create");
    }
    vol.checkpoint_now().await.unwrap();
    let roots: Vec<u64> = vol.flat_trees().iter().map(|t| t.root().addr).collect();
    assert_eq!(roots.len(), 3, "a flat volume's three trees");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);

    // The fresh open: an empty node cache — the loads below all MISS.
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let cache = Arc::clone(vol.node_cache());
    let mut joins = Vec::new();
    for round in 0..240u32 {
        for addr in &roots {
            let cache = Arc::clone(&cache);
            let addr = *addr;
            joins.push(squeezefs::meta_exec::spawn_meta_join(
                "colliding_flat_load",
                async move {
                    let loaded = cache.load(addr).await;
                    if round % 8 == 7 {
                        if let Some(node) = loaded.as_ref().ok().and_then(|n| n.clone()) {
                            // Some loaders drop the image again behind the
                            // others — the miss/collide cycle repeats.
                            let _ = cache.test_drop_clean_node(&node);
                        }
                    }
                    loaded.map(|n| n.is_some()).unwrap_or(false)
                },
            ));
        }
    }
    let all = async {
        let mut ok = 0usize;
        for j in joins {
            if j.await.unwrap_or(false) {
                ok += 1;
            }
        }
        ok
    };
    let ok = tokio::time::timeout(std::time::Duration::from_secs(120), all)
        .await
        .expect("720 colliding flat loads on the meta lanes must complete — a wedge here is the mixed-style single-flight lock");
    assert_eq!(ok, 720, "every load answered its node");
    shutdown(&routed).await;
}
