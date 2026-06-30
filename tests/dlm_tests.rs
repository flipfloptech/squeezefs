use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use std::time::Duration;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn get_client() -> Option<DlmClient> {
    let url = get_redis_url();
    match DlmClient::new(&url) {
        Ok(client) => {
            // Test connection
            let client_url = url.clone();
            let c = redis::Client::open(client_url).ok()?;
            if c.get_multiplexed_tokio_connection().await.is_err() {
                return None;
            }
            Some(client)
        }
        Err(_) => None,
    }
}

#[tokio::test]
async fn test_lock_acquisition_and_release() {
    let client = match get_client().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let file_path = "test_file_1.txt";

    // 1. Acquire lock
    let lease = client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
        .expect("Should acquire lock successfully");

    assert_eq!(lease.file_path(), file_path);
    assert!(lease.fencing_token() > 0);

    let err = match client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
    {
        Ok(_) => panic!("Expected lock acquisition to fail"),
        Err(e) => e,
    };

    assert!(matches!(err, SqueezefsError::LockFailed { .. }));

    // 3. Release lock
    lease
        .release()
        .await
        .expect("Should release lock successfully");

    // 4. Re-acquire lock (should succeed now)
    let _lease2 = client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
        .expect("Should acquire lock again after release");
}

#[tokio::test]
async fn test_lock_drop_auto_release() {
    let client = match get_client().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let file_path = "test_file_2.txt";

    {
        // Acquire lock and immediately let it go out of scope (drop)
        let _lease = client
            .acquire_lock(file_path, None, Duration::from_secs(3))
            .await
            .expect("Should acquire lock");
    }

    // Wait a brief moment for drop background spawn to execute
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Try to acquire the lock again (should succeed since drop releases the lock)
    let _lease2 = client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
        .expect("Should acquire lock successfully after drop");
}

#[tokio::test]
async fn test_fencing_token_monotony() {
    let client = match get_client().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let file_path = "test_file_3.txt";

    // 1. First lease
    let lease1 = client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
        .expect("Should acquire lock");
    let token1 = lease1.fencing_token();

    lease1.release().await.expect("Should release");

    // 2. Second lease
    let lease2 = client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await
        .expect("Should acquire lock");
    let token2 = lease2.fencing_token();

    assert!(token2 > token1, "Fencing tokens must be strictly monotonic");
    lease2.release().await.expect("Should release");
}

#[tokio::test]
async fn test_byte_range_locks() {
    let client = match get_client().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let file_path = "test_file_4.txt";

    // Acquire lock on range [0, 100]
    let lease1 = client
        .acquire_lock(file_path, Some((0, 100)), Duration::from_secs(3))
        .await
        .expect("Should acquire lock on range 0-100");

    // Acquire lock on range [101, 200] (different range on same file should succeed)
    let lease2 = client
        .acquire_lock(file_path, Some((101, 200)), Duration::from_secs(3))
        .await
        .expect("Should acquire lock on range 101-200");

    let err = match client
        .acquire_lock(file_path, Some((0, 100)), Duration::from_secs(3))
        .await
    {
        Ok(_) => panic!("Expected lock acquisition to fail"),
        Err(e) => e,
    };

    assert!(matches!(err, SqueezefsError::LockFailed { .. }));

    lease1.release().await.expect("Should release");
    lease2.release().await.expect("Should release");
}

#[tokio::test]
async fn test_cluster_client_initialization() {
    use squeezefs::dlm::MetaClient;

    // Single-node mode
    let single_client = MetaClient::new("redis://127.0.0.1:6379").unwrap();
    assert!(matches!(single_client, MetaClient::Single { .. }));

    // Cluster mode via comma-separated list
    let cluster_client_comma =
        MetaClient::new("redis://127.0.0.1:6379,redis://127.0.0.1:6380").unwrap();
    assert!(matches!(cluster_client_comma, MetaClient::Cluster(_)));

    // Cluster mode via protocol prefix
    let cluster_client_proto = MetaClient::new("redis+cluster://127.0.0.1:6379").unwrap();
    assert!(matches!(cluster_client_proto, MetaClient::Cluster(_)));
}
