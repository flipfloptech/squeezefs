use redis::AsyncCommands;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume_ext, get_volume_status};

fn get_sharded_redis_url() -> String {
    // We target three separate database indexes (1, 2, 3) on the local Redis/Garnet instance to act as separate shards.
    "redis+sharded://127.0.0.1:6379/1,127.0.0.1:6379/2,127.0.0.1:6379/3".to_string()
}

#[tokio::test]
async fn test_sharded_url_parsing_and_routing() {
    let sharded_url = get_sharded_redis_url();
    let dlm = DlmClient::new(&sharded_url).expect("Failed to create DlmClient");

    assert_eq!(dlm.shard_count(), 3);

    // Verify connection routing for specific inodes
    // ino % 3
    let _conn_0 = dlm
        .get_connection_for_inode(3)
        .await
        .expect("Failed to get connection");
    let _conn_1 = dlm
        .get_connection_for_inode(4)
        .await
        .expect("Failed to get connection");
    let _conn_2 = dlm
        .get_connection_for_inode(5)
        .await
        .expect("Failed to get connection");

    // Under the hood, these should point to databases 1, 2, and 3
    // We can verify this by checking the connection URL/info address of the underlying single client if accessible.
}

#[tokio::test]
async fn test_sharded_volume_formatting_and_initialization() {
    let sharded_url = get_sharded_redis_url();

    // Clear databases 1, 2, and 3 first
    for db in 1..=3 {
        let client =
            redis::Client::open(format!("redis://127.0.0.1:6379/{}", db)).expect("Client open");
        if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
            let _: () = redis::cmd("FLUSHDB")
                .query_async(&mut con)
                .await
                .unwrap_or(());
        }
    }

    format_volume_ext(
        &sharded_url,
        "sharded_test_vol",
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        None,
        None,
        None,
        None, // nvme_target_path
        None, // ip
        None, // port
        None, // subnqn
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        true, // quick
    )
    .await
    .expect("Failed to format sharded volume");

    // Get volume status should query shard 0 (db 1) and return correct settings
    let status = get_volume_status(&sharded_url)
        .await
        .expect("Failed to get status");
    assert_eq!(status["Setting"]["Name"], "sharded_test_vol");

    // Root inode 1 is 1 % 3 == 1, so squeezefs:attr:1 must exist on db 2 (Shard 1)
    let client_1 = redis::Client::open("redis://127.0.0.1:6379/2").expect("db 2 client");
    let mut con_1 = client_1
        .get_multiplexed_tokio_connection()
        .await
        .expect("db 2 connection");
    let root_exists: bool = con_1.exists("squeezefs:attr:1").await.unwrap_or(false);
    assert!(
        root_exists,
        "Root inode 1 attributes not formatted on Shard 1 (db 2)"
    );

    // Inode counter on Shard 1 should be set to 4 (1 + N)
    let counter_1: u64 = con_1.get("squeezefs:inode_counter").await.unwrap_or(0);
    assert_eq!(counter_1, 4);

    // Inode counter on Shard 2 (db 3) should be set to 2 (1 + i)
    let client_2 = redis::Client::open("redis://127.0.0.1:6379/3").expect("db 3 client");
    let mut con_2 = client_2
        .get_multiplexed_tokio_connection()
        .await
        .expect("db 3 connection");
    let counter_2: u64 = con_2.get("squeezefs:inode_counter").await.unwrap_or(0);
    assert_eq!(counter_2, 2);
}
