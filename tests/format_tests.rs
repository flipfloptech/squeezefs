use squeezefs::fuse_client::format_volume_ext;
use std::fs::File;
use tempfile::tempdir;

#[test]
fn test_format_size_human() {
    let format_logic = |bytes: u64| {
        let kib = bytes as f64 / 1024.0;
        let mib = kib / 1024.0;
        let gib = mib / 1024.0;
        let tib = gib / 1024.0;
        let pib = tib / 1024.0;

        if pib >= 1.0 {
            format!("{:.2} PiB", pib)
        } else if tib >= 1.0 {
            format!("{:.2} TiB", tib)
        } else if gib >= 1.0 {
            format!("{:.2} GiB", gib)
        } else if mib >= 1.0 {
            format!("{:.2} MiB", mib)
        } else if kib >= 1.0 {
            format!("{:.2} KiB", kib)
        } else {
            format!("{} B", bytes)
        }
    };

    assert_eq!(format_logic(500), "500 B");
    assert_eq!(format_logic(1500), "1.46 KiB");
    assert_eq!(format_logic(10 * 1024 * 1024), "10.00 MiB");
    assert_eq!(format_logic(5 * 1024 * 1024 * 1024), "5.00 GiB");
    assert_eq!(format_logic(2 * 1024 * 1024 * 1024 * 1024), "2.00 TiB");
}

#[tokio::test]
async fn test_format_volume_quick_and_full() {
    let temp_dir = tempdir().unwrap();
    let backing_file = temp_dir.path().join("test_backing.img");

    // Create an 8MB file for testing
    let file = File::create(&backing_file).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let redis_url = "redis://127.0.0.1:6379/9"; // Use db 9 for test isolation

    // Clear test db first
    let client = redis::Client::open(redis_url).unwrap();
    if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
        let _: () = redis::cmd("FLUSHDB")
            .query_async(&mut con)
            .await
            .unwrap_or(());
    }

    // 1. Test Quick Format
    let res = format_volume_ext(
        redis_url,
        "format_test_vol",
        4 * 1024 * 1024,
        8 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        None,
        None,
        None,
        backing_file.to_str(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        true, // quick: true
    )
    .await;

    assert!(res.is_ok(), "Quick format should succeed: {:?}", res.err());

    // 2. Test Full Format (runs the multi-threaded progress-bar wiping loop)
    let res_full = format_volume_ext(
        redis_url,
        "format_test_vol_full",
        4 * 1024 * 1024,
        8 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        None,
        None,
        None,
        backing_file.to_str(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false, // quick: false (runs full wipe of 8MB)
    )
    .await;

    assert!(
        res_full.is_ok(),
        "Full format of 8MB should succeed: {:?}",
        res_full.err()
    );
}
