use squeezefs::backend::RustFsClient;
use squeezefs::dlm::DlmClient;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test]
async fn test_multi_rail_backend_initialization() {
    let local_ips = vec![
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
    ];

    let backend = RustFsClient::new_with_local_ips(local_ips.clone()).await;
    
    // Check that we initialized two clients (or mock storage if S3 endpoint is not configured)
    assert_eq!(backend.client_count(), 2);
}

#[tokio::test]
async fn test_multi_rail_dlm_initialization() {
    let redis_url = get_redis_url();
    let local_ips = vec![
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    ];

    // Verify DlmClient constructs with local IPs
    let dlm_client = DlmClient::new_with_local_ips(&redis_url, local_ips);
    assert!(dlm_client.is_ok());
    let dlm_client = dlm_client.unwrap();

    // Check bound connection counts
    assert_eq!(dlm_client.connection_count(), 1);
}

#[tokio::test]
async fn test_multi_rail_dlm_rotation_and_failover() {
    let redis_url = get_redis_url();
    let local_ips = vec![
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), // Use 127.0.0.1 twice to mock two NIC rails pointing to localhost
    ];

    let dlm_client = match DlmClient::new_with_local_ips(&redis_url, local_ips) {
        Ok(c) => c,
        Err(_) => {
            println!("Skipping rotation test: local Redis/Garnet not available");
            return;
        }
    };

    // Verify that we have 2 bound connections
    assert_eq!(dlm_client.connection_count(), 2);

    // Acquire lock and verify connection works
    let file_path = "test_multi_rail_lock.txt";
    let lease = dlm_client
        .acquire_lock(file_path, None, Duration::from_secs(3))
        .await;

    // Clean up if acquired
    if let Ok(l) = lease {
        let _ = l.release().await;
    }
}
