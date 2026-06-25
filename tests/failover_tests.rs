use squeezefs::dlm::MetaClient;
use squeezefs::error::Result;

#[test]
fn test_sentinel_url_parsing() {
    let sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/mymaster";
    let meta_client = MetaClient::new(sentinel_url).unwrap();
    assert!(matches!(meta_client, MetaClient::Sentinel { .. }));
}

#[tokio::test]
async fn test_sentinel_connection_caching() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = MetaClient::new(&redis_url)?;

    // 1. Get a valid multiplexed connection to Garnet/Redis
    let mut real_conn = client.get_connection().await?;
    match redis::cmd("PING")
        .query_async::<_, String>(&mut real_conn)
        .await
    {
        Ok(pong) => println!("PING real_conn succeeded: {}", pong),
        Err(e) => println!("PING real_conn failed: {:?}", e),
    }

    let conn_val = match &real_conn {
        squeezefs::dlm::MetaConnection::Single { conn, .. } => conn.clone(),
        _ => panic!("Expected MetaConnection::Single"),
    };

    // 2. Pre-populate SENTINEL_CONN_POOL with this valid connection under service name "fake_master"
    squeezefs::dlm::SENTINEL_CONN_POOL.insert("fake_master".to_string(), conn_val);

    // 3. Create a Sentinel MetaClient pointing to dummy sentinel servers but service name "fake_master"
    let dummy_sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/fake_master";
    let sentinel_client = MetaClient::new(dummy_sentinel_url)?;

    // 4. Call get_connection on the sentinel client. It should HIT the cache and succeed!
    let mut sentinel_conn = sentinel_client.get_connection().await?;

    // 5. Verify we can run operations
    match redis::cmd("PING")
        .query_async::<_, String>(&mut sentinel_conn)
        .await
    {
        Ok(res) => {
            println!("PING sentinel_conn succeeded: {}", res);
            assert_eq!(res, "PONG");
        }
        Err(e) => {
            println!("PING sentinel_conn failed: {:?}", e);
            squeezefs::dlm::SENTINEL_CONN_POOL.remove("fake_master");
            return Err(e.into());
        }
    }

    // Clean up
    squeezefs::dlm::SENTINEL_CONN_POOL.remove("fake_master");

    Ok(())
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
