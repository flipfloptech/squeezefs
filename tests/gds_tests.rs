use squeezefs::cache::gds::GdsCache;
use squeezefs::error::SqueezefsError;

#[tokio::test]
async fn test_gds_unavailability_flow() {
    let gds = GdsCache::new();
    
    // In a test environment without a real discrete GPU and libcufile.so, GDS must report as unavailable.
    assert!(!gds.is_available());
    
    // Calling read_direct should fail with SqueezefsError::GdsError
    let result = gds.read_direct("test_block_key", 0x1000, 0, 1024).await;
    
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, SqueezefsError::GdsError(_)),
        "Expected GdsError, got {:?}", err
    );
}

#[tokio::test]
async fn test_gds_cufile_load_failure_handling() {
    // This test verifies that even if compiled with the 'gds' feature, 
    // GdsCache handles a missing libcufile.so gracefully rather than crashing.
    let gds = GdsCache::new();
    
    // GDS should not crash on initialization and should fallback to unavailable.
    if !gds.is_available() {
        let result = gds.read_direct("dummy_key", 0x2000, 0, 512).await;
        assert!(matches!(result.unwrap_err(), SqueezefsError::GdsError(_)));
    }
}
