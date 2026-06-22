use fuse3::raw::{prelude::*, Request};
use sha2::{Digest, Sha256};
use rand::SeedableRng;
use rand::RngCore;
use rand_chacha::ChaCha8Rng;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::PathBuf;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn clean_db() -> Option<()> {
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());
    Some(())
}

fn generate_seeded_data(seed: u64, size: usize) -> Vec<u8> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill_bytes(&mut data);
    data
}

fn calculate_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

#[tokio::test]
async fn test_data_integrity_various_sizes() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "integrity_test_vol";

    // Format the volume
    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024, // 4MB block size
        100 * 1024 * 1024 * 1024, // 100GB capacity
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_integrity")]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Datasets: 10KB (micro/inline), 500KB (small/staged), 10MB (large/striped)
    let sizes = vec![
        ("micro.bin", 10 * 1024, 42),
        ("small.bin", 500 * 1024, 43),
        ("large.bin", 10 * 1024 * 1024, 44),
    ];

    let mut file_infos = Vec::new();

    for (name, size, seed) in sizes {
        let data = generate_seeded_data(seed, size);
        let checksum = calculate_sha256(&data);

        // Create file
        let reply_create = fs.create(req, 1, OsStr::new(name), 0o644, 0).await.unwrap();
        let ino = reply_create.attr.ino;

        // Write file
        fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();

        file_infos.push((ino, size, checksum));
    }

    // Force flush if necessary, and then clear memory caches
    // Recreating them forces reload of block maps from Garnet and read from disk cache or S3!
    drop(fs);

    // Recreate filesystem structure with the SAME Garnet and S3 mock backend
    let cache_recreate = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router_recreate = DataRouter::new(dlm.clone(), multi_backend.clone(), cache_recreate);
    let fs_recreate = SqueezefsFilesystem::new(router_recreate, dlm.clone(), 1000, 1000);
    fs_recreate.init(req).await.unwrap();

    for (ino, size, expected_checksum) in file_infos {
        let reply_read = fs_recreate.read(req, ino, 0, size as u64, 0).await.unwrap();
        assert_eq!(reply_read.data.len(), size);
        let checksum = calculate_sha256(&reply_read.data);
        assert_eq!(checksum, expected_checksum, "Checksum mismatch for inode {}", ino);
    }
}
