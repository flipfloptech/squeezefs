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

/// Non-async on purpose; the count CONVERGES rather than reads
/// instantly (rip-tokio-total port): background workers (merge worker,
/// health probe, dehydrate workers, reclaim worker) now live on the
/// process-lifetime sqz pools with BOUNDED exit edges — weak-upgrade
/// failure, last-clone-drop stop guards, channel closure — each firing
/// within one 2 s tick of the graph drop. The contract this pins is
/// unchanged and stronger than the old scoped-runtime form: no
/// IMMORTAL fd hold (the convicted Arc cycle held forever), AND every
/// worker exit edge actually fires (the immortal-fs law's fd face —
/// the old form never verified the workers released anything, it
/// killed them with the runtime).
#[test]
fn dropped_router_graph_releases_all_segment_fds() {
    let base = count_tmp_fds();
    {
        {
            squeezefs_ipc::sqz_blocking::block_on(async {
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
        }
    }
    // Converge-within-bound: the workers' exit edges are tick-cadenced
    // (2 s); 10 s = five ticks of margin. A count that never converges
    // is an IMMORTAL hold — the convicted cycle class.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let leaked = count_tmp_fds().saturating_sub(base);
        if leaked == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped DataRouter graph must release every segment fd — {leaked} \
             still open after 10 s (an IMMORTAL hold: the read_tier_purge ↔ \
             backend_router Arc cycle, or a worker exit edge that never fires)"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
