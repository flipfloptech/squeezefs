use squeezefs::dlm::MetaClient;
use squeezefs::error::Result;

#[test]
fn test_sentinel_url_parsing() {
    let sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/mymaster";
    let meta_client = MetaClient::new(sentinel_url).unwrap();
    assert!(matches!(meta_client, MetaClient::Sentinel(_)));
}

#[tokio::test]
async fn test_connection_resilience() -> Result<()> {
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = MetaClient::new(&redis_url)?;

    // Acquire connection
    let _conn = client.get_connection().await?;

    // Re-getting connection should succeed
    let _conn2 = client.get_connection().await?;

    Ok(())
}
