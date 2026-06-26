use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::gds::GdsCache;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::routing::DataRouter;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_router() -> DataRouter {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());
    let cache = TieredCache::new(
        vec![std::env::temp_dir()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();
    DataRouter::new(dlm, multi_backend, cache)
}

#[tokio::test]
async fn test_gds_unavailability_flow() {
    let gds = GdsCache::new(vec![]);
    let router = setup_router().await;

    // In a test environment without a real discrete GPU and libcufile.so, GDS must report as unavailable.
    assert!(!gds.is_available());

    // Calling read_direct should fail with SqueezefsError::GdsError
    let result = gds
        .read_direct("test_block_key", 0x1000, 0, 1024, &router)
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, SqueezefsError::GdsError(_)),
        "Expected GdsError, got {:?}",
        err
    );
}

#[tokio::test]
async fn test_gds_cufile_load_failure_handling() {
    // This test verifies that even if compiled with the 'gds' feature,
    // GdsCache handles a missing libcufile.so gracefully rather than crashing.
    let gds = GdsCache::new(vec![]);
    let router = setup_router().await;

    // GDS should not crash on initialization and should fallback to unavailable.
    if !gds.is_available() {
        let result = gds.read_direct("dummy_key", 0x2000, 0, 512, &router).await;
        assert!(matches!(result.unwrap_err(), SqueezefsError::GdsError(_)));
    }
}
