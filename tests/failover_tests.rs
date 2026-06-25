use squeezefs::dlm::MetaClient;
use squeezefs::error::Result;

#[test]
fn test_sentinel_url_parsing() {
    let sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/mymaster";
    let meta_client = MetaClient::new(sentinel_url).unwrap();
    assert!(matches!(meta_client, MetaClient::Sentinel(_)));
}

async fn is_db_available() -> bool {
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

#[tokio::test]
async fn test_connection_resilience() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = MetaClient::new(&redis_url)?;

    // Acquire connection
    let _conn = client.get_connection().await?;

    // Re-getting connection should succeed
    let _conn2 = client.get_connection().await?;

    Ok(())
}

#[tokio::test]
async fn test_single_bound_reconnection() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let local_ip = "127.0.0.1".parse::<std::net::IpAddr>().unwrap();

    let client = MetaClient::new_with_local_ips(&redis_url, vec![local_ip]).await?;

    let mut conn = client.get_connection().await?;
    let res: String = redis::cmd("PING").query_async(&mut conn).await?;
    assert_eq!(res, "PONG");

    Ok(())
}
