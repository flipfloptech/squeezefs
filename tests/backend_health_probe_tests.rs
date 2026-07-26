//! L3 transport-economy — the statx residual (attributed): every ranged
//! read runs `BackendRouter::is_backend_healthy`, whose `Path::exists()`
//! probe statx'es the DATA DEVICE NODE per call — 0.76 statx/op measured
//! on the charter workload (`.benchmarks/2026-07-18-l3-transport-economy.md`).
//! The node probe is a liveness *hint* (the I/O path itself fails loud on
//! a vanished device), so it is TTL-cached on the device object
//! ([`squeezefs::nvme_dev::NODE_PROBE_TTL_MS`]); the explicit
//! `unhealthy_backends` mark stays authoritative and instant.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::{NvmeBlockDev, NODE_PROBE_TTL_MS};
use squeezefs::routing::BackendRouter;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_node_probe_is_ttl_cached_not_per_call() {
    let dlm = DlmClient::new("local").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dev_path = dir.path().join("dev.img");
    std::fs::File::create(&dev_path)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(dev_path.to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "health_probe_test")
            .await
            .unwrap(),
    );
    let router = BackendRouter::new(
        ba,
        nvme,
        Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024)),
    );

    assert!(
        router.is_backend_healthy("backend_0"),
        "an existing device node reads healthy"
    );

    // The explicit unhealthy mark is authoritative and INSTANT — it is the
    // real health state machine; the node probe is only a liveness hint.
    router
        .unhealthy_backends
        .insert("backend_0".to_string(), true);
    assert!(
        !router.is_backend_healthy("backend_0"),
        "the explicit unhealthy mark must bypass any probe cache instantly"
    );
    router.unhealthy_backends.remove("backend_0");
    assert!(router.is_backend_healthy("backend_0"));

    // THE lever: within the TTL the cached probe serves — no per-call
    // statx. (Pre-fix this flips false immediately: the probe ran
    // Path::exists() on every call.)
    std::fs::remove_file(&dev_path).unwrap();
    assert!(
        router.is_backend_healthy("backend_0"),
        "within NODE_PROBE_TTL_MS the cached probe result serves — a \
         per-call device-node statx is the 0.76/op L3 regression"
    );

    // An expired probe re-checks the node: the vanished device is
    // detected within one TTL.
    std::thread::sleep(std::time::Duration::from_millis(NODE_PROBE_TTL_MS + 100));
    assert!(
        !router.is_backend_healthy("backend_0"),
        "an expired probe must re-check the node and see it gone"
    );
}

/// The 2026-07-26 one-extra-device-fetch flake class, attributed: the
/// health worker's liveness probe (`read_block(0, 4096)`) counted in
/// `get_obj` — the process-global CHURN DETECTOR every counter-window
/// suite and per-PR gate keys on (`get_obj/unique ≈ 1.0`, AGENTS.md).
/// The probe's completion is queued behind real fixture I/O on the uring
/// worker, so it lands at a nondeterministic point and randomly inflates
/// a counted window by exactly +1 — reproduced as
/// `hybrid_io_tests::escape_direct_device_true_is_device_true` (dev
/// ~5/10) and `read_tier_refetch_churn_tests::
/// fetched_blocks_are_tier_visible_and_never_refetched` (dev ~3/10),
/// both backtraced to `perform_device_health_check`.
///
/// The contract: health-probe device reads are CONTROL-PLANE diagnostics
/// — they must never count in `get_obj` (which stays the raw *data*
/// device-read-op counter) — while real data reads keep counting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_probe_device_reads_never_count_in_get_obj() {
    use squeezefs::fuse_client::METRICS;
    use squeezefs::health::Probe;
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let dev_path = dir.path().join("probe-dev.img");
    std::fs::File::create(&dev_path)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(dev_path.to_str().unwrap()));

    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let probe = squeezefs::routing::perform_device_health_check(&nvme).await;
    assert_eq!(probe, Probe::Ok, "healthy device probes Ok");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0,
        "a health-probe read must NOT count in get_obj — the probe lands \
         at a nondeterministic point and poisons every counter-window \
         assertion and per-PR churn gate keyed on it"
    );

    // Semantics guard: real data reads still count (get_obj remains the
    // raw device-read-op counter for data traffic — do not over-remove).
    nvme.read_block(0, 4096).await.expect("data read");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0 + 1,
        "a real data read still counts in get_obj"
    );
}
