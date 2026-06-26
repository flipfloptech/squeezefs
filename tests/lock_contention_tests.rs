use squeezefs::cache::lru::LruCache;
use squeezefs::dlm::BoundConnection;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[test]
fn test_lru_cache_sharding_count() {
    // 1. Small capacity should use a small number of shards (1 or 16)
    let small_cache = LruCache::with_capacity(5 * 1024 * 1024);
    assert!(
        small_cache.num_shards() == 1 || small_cache.num_shards() == 16,
        "Small cache should have 1 or 16 shards, got {}",
        small_cache.num_shards()
    );

    // 2. Large capacity cache should programmatically scale to match CPU core count
    let expected_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(16);
    let expected_shards = expected_cores.next_power_of_two();

    // Provide a capacity that is guaranteed to be large enough for the full shard count
    let capacity = (expected_shards * 4 * 1024 * 1024) as u64;
    let large_cache = LruCache::with_capacity(capacity);

    assert_eq!(
        large_cache.num_shards(),
        expected_shards,
        "Large cache shard count should match expected core count (cores={}, power_of_two={})",
        expected_cores,
        expected_shards
    );
}

#[tokio::test]
async fn test_bound_connection_rwlock_concurrency() {
    // Construct a dummy BoundConnection with a dummy Redis multiplexed connection
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let conn = match client.get_multiplexed_tokio_connection().await {
        Ok(c) => c,
        Err(_) => {
            println!("Skipping Redis-dependent connection test: local Redis/Garnet not available");
            return;
        }
    };

    let bound = BoundConnection {
        conn: Arc::new(tokio::sync::RwLock::new(conn)),
        local_ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6379),
        conn_info: client.get_connection_info().clone(),
    };

    // Spawn multiple tasks that acquire read locks concurrently to clone the connection
    let mut handles = vec![];
    for _ in 0..10 {
        let bound_clone = bound.clone();
        let handle = tokio::spawn(async move {
            // Read-locking bound.conn should be completely concurrent (no blocking)
            let _conn_clone = bound_clone.conn.read().await.clone();
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap();
    }
}
