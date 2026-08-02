//! Resource-release pin (2026-07-27 write-pipeline campaign conviction,
//! pre-existing on dev): dropping a `DataRouter` graph must release every
//! staging/read-cache segment file descriptor.
//!
//! The convicted cycle: `BackendRouter.read_tier_purge` (an `Arc<dyn Fn>`
//! closure owning the `TieredCache`) → `TieredCache.nvme.backend_router`
//! (a STRONG `Arc<BackendRouter>` cell) → `BackendRouter`. Every dropped
//! mount/fixture graph stayed immortal — measured ~65 leaked segment-file
//! fds per test harness, EMFILE by the end of any suite with ~27 fixtures
//! at the stock 2048 soft limit. The fix makes the staging host hold the
//! router WEAK (the `set_data_router` discipline: the router owns the
//! cache, never the reverse).
//!
//! RED against the strong-cell tree: `leaked == 0` fails with ~65.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

fn count_tmp_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter(|e| {
            std::fs::read_link(e.as_ref().unwrap().path())
                .map(|l| l.to_string_lossy().starts_with("/tmp"))
                .unwrap_or(false)
        })
        .count()
}

/// Non-async on purpose: the graph is built and dropped inside a scoped
/// runtime so the count runs AFTER runtime teardown — live background
/// workers (merge worker, health probe) cannot alias as leaks.
#[test]
fn dropped_router_graph_releases_all_segment_fds() {
    let base = count_tmp_fds();
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dlm = DlmClient::new().unwrap();
            let b = NamedTempFile::new().unwrap();
            std::fs::File::create(b.path())
                .unwrap()
                .set_len(64 * 1024 * 1024)
                .unwrap();
            let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
            let ba = Arc::new(BlockAllocator::new("fd_release_ns").await.unwrap());
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
            let _ = router;
        });
        drop(rt);
    }
    let leaked = count_tmp_fds().saturating_sub(base);
    assert_eq!(
        leaked, 0,
        "a dropped DataRouter graph must release every segment fd — {leaked} \
         still open (the read_tier_purge ↔ backend_router Arc cycle is back?)"
    );
}
