use fuse3::raw::{prelude::*, Request};
use rand::RngCore;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use sha2::{Digest, Sha256};
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::PathBuf;
use tempfile::tempdir;
use redis::AsyncCommands;

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
        4 * 1024 * 1024,
        // 4MB block size
        100 * 1024 * 1024 * 1024,
        // 100GB capacity
        0,
        // inodes limit
        "none",
        // compression
        "none",
        // encrypt_algo
        None,
        // encrypt_key
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_integrity")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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
        fs.flush(req, ino, 0, 0).await.unwrap();
        fs.release(req, ino, 0, 0, 0, false).await.unwrap();

        file_infos.push((ino, size, checksum));
    }

    // Force flush if necessary, and then clear memory caches
    // Recreating them forces reload of block maps from Garnet and read from disk cache or S3!
    drop(fs);

    // Recreate filesystem structure with the SAME Garnet and S3 mock backend
    let cache_recreate = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router_recreate = DataRouter::new(dlm.clone(), multi_backend.clone(), cache_recreate);
    let fs_recreate = SqueezefsFilesystem::new(router_recreate, dlm.clone(), 1000, 1000);
    fs_recreate.init(req).await.unwrap();

    for (ino, size, expected_checksum) in file_infos {
        let reply_read = fs_recreate.read(req, ino, 0, 0, size as u32).await.unwrap();
        assert_eq!(reply_read.data.len(), size);
        let checksum = calculate_sha256(&reply_read.data);
        assert_eq!(
            checksum, expected_checksum,
            "Checksum mismatch for inode {}",
            ino
        );
    }
}

fn calculate_sha512(data: &[u8]) -> String {
    use sha2::Sha512;
    let mut hasher = Sha512::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

#[tokio::test]
async fn test_data_integrity_chunked_writes_sha512() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "integrity_test_vol_chunked";

    // Format the volume
    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024, // 4MB block size
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_integrity_chunked")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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

    // Generate 6MB of seeded data (crosses the 4MB block boundary)
    let size = 6 * 1024 * 1024;
    let data = generate_seeded_data(12345, size);
    let expected_checksum = calculate_sha512(&data);

    // Create file
    let reply_create = fs.create(req, 1, OsStr::new("chunked_test.bin"), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Write file in 128KB chunks
    let chunk_size = 128 * 1024;
    let mut offset = 0;
    while offset < size {
        let end = std::cmp::min(offset + chunk_size, size);
        let chunk = &data[offset..end];
        fs.write(req, ino, 0, offset as u64, chunk, 0, 0).await.unwrap();
        offset = end;
    }

    fs.flush(req, ino, 0, 0).await.unwrap();
    
    // Test immediate read from the active filesystem instance (cached data check)
    let reply_read_immediate = fs.read(req, ino, 0, 0, size as u32).await.unwrap();
    assert_eq!(reply_read_immediate.data.len(), size);
    let checksum_immediate = calculate_sha512(&reply_read_immediate.data);
    assert_eq!(checksum_immediate, expected_checksum, "Immediate read checksum mismatch");

    fs.release(req, ino, 0, 0, 0, false).await.unwrap();

    // Recreate filesystem to bypass RAM caches and force read from S3/Staging
    drop(fs);

    let cache_recreate = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router_recreate = DataRouter::new(dlm.clone(), multi_backend.clone(), cache_recreate);
    let fs_recreate = SqueezefsFilesystem::new(router_recreate, dlm.clone(), 1000, 1000);
    fs_recreate.init(req).await.unwrap();

    let reply_read = fs_recreate.read(req, ino, 0, 0, size as u32).await.unwrap();
    assert_eq!(reply_read.data.len(), size);
    let checksum = calculate_sha512(&reply_read.data);
    assert_eq!(checksum, expected_checksum);
}

#[tokio::test]
async fn test_striped_rmw_corruption_with_compression() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "integrity_test_vol_lz4";

    // Format the volume with lz4 compression!
    format_volume(
        &redis_url,
        fs_name,
        1 * 1024 * 1024, // 1MB block size to easily cross boundaries
        100 * 1024 * 1024 * 1024,
        0,
        "lz4", // lz4 compression enabled!
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_integrity_lz4")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    
    // Set format parameters on the router
    let crypto = squeezefs::crypto_compress::CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    router.set_crypto(crypto);

    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Create a 6MB file (will be striped because block size is 1MB and size > 4MB progressive layout threshold)
    let size = 6 * 1024 * 1024;
    let mut data = generate_seeded_data(54321, size);
    
    let reply_create = fs.create(req, 1, OsStr::new("striped_lz4.bin"), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Write file sequentially to make it striped
    fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();
    fs.release(req, ino, 0, 0, 0, false).await.unwrap();

    // 2. Perform a partial write/seek (RMW) on the striped file
    // Write 100 bytes at offset 1.5MB (block 1, which is offset 1MB to 2MB)
    let patch_offset = 1500 * 1024;
    let patch_data = vec![7u8; 100];
    
    // Update our reference data
    data[patch_offset..patch_offset + 100].copy_from_slice(&patch_data);
    let expected_checksum = calculate_sha512(&data);

    let _reply_open = fs.open(req, ino, 0).await.unwrap();
    fs.write(req, ino, 0, patch_offset as u64, &patch_data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();
    fs.release(req, ino, 0, 0, 0, false).await.unwrap();

    // Recreate filesystem to clear memory caches and read back from storage
    drop(fs);

    let cache_recreate = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router_recreate = DataRouter::new(dlm.clone(), multi_backend.clone(), cache_recreate);
    let crypto_recreate = squeezefs::crypto_compress::CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    router_recreate.set_crypto(crypto_recreate);
    
    let fs_recreate = SqueezefsFilesystem::new(router_recreate, dlm.clone(), 1000, 1000);
    fs_recreate.init(req).await.unwrap();

    let reply_read = fs_recreate.read(req, ino, 0, 0, size as u32).await.unwrap();
    assert_eq!(reply_read.data.len(), size);
    let checksum = calculate_sha512(&reply_read.data);
    assert_eq!(checksum, expected_checksum);
}

#[tokio::test]
async fn test_posix_locks_released_on_close() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "posix_lock_release_test";

    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_posix_lock")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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

    // Create file
    let reply_create = fs.create(req, 1, OsStr::new("lock_file.bin"), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Acquire POSIX range lock (owner = 5678)
    fs.setlk(req, ino, 0, 5678, 0, 100, libc::F_WRLCK as u32, 1234, false).await.unwrap();

    // Verify lock is acquired by checking stats json
    let reply_read = fs.read(req, 0xffff_ffff_ffff_fffd, 0, 0, 100000).await.unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&reply_read.data).unwrap();
    let count = stats["active_posix_locks_count"].as_u64().unwrap();
    assert_eq!(count, 1);

    // Call release/close with lock owner = 5678
    fs.release(req, ino, 0, 0, 5678, false).await.unwrap();

    // Verify lock is released
    let reply_read2 = fs.read(req, 0xffff_ffff_ffff_fffd, 0, 0, 100000).await.unwrap();
    let stats2: serde_json::Value = serde_json::from_slice(&reply_read2.data).unwrap();
    let count2 = stats2["active_posix_locks_count"].as_u64().unwrap();
    assert_eq!(count2, 0);
}

#[tokio::test]
async fn test_copy_file_range_same_file_safety() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "copy_self_test";

    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_copy_self")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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

    // Create file
    let reply_create = fs.create(req, 1, OsStr::new("copy_self.bin"), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Write some data (striped, 6MB)
    let size = 6 * 1024 * 1024;
    let data = generate_seeded_data(999, size);
    fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();

    // Copy to self: copy first 1MB of file to offset 2MB of same file
    // This used to panic
    let res = fs.copy_file_range(req, ino, 0, 0, ino, 0, 2 * 1024 * 1024, 1024 * 1024, 0).await;
    assert!(res.is_ok(), "copy_file_range inside same file failed: {:?}", res.err());
}

#[tokio::test]
async fn test_rename_overwrite_resource_cleanup() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "rename_cleanup_test";

    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_rename_cleanup")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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

    // 1. Create source file (striped, 5MB)
    let reply_src = fs.create(req, 1, OsStr::new("src.bin"), 0o644, 0).await.unwrap();
    let src_ino = reply_src.attr.ino;
    let src_data = generate_seeded_data(101, 5 * 1024 * 1024);
    fs.write(req, src_ino, 0, 0, &src_data, 0, 0).await.unwrap();
    fs.flush(req, src_ino, 0, 0).await.unwrap();
    fs.release(req, src_ino, 0, 0, 0, false).await.unwrap();

    // 2. Create dest file (striped, 5MB)
    let reply_dest = fs.create(req, 1, OsStr::new("dest.bin"), 0o644, 0).await.unwrap();
    let dest_ino = reply_dest.attr.ino;
    let dest_data = generate_seeded_data(202, 5 * 1024 * 1024);
    fs.write(req, dest_ino, 0, 0, &dest_data, 0, 0).await.unwrap();
    fs.flush(req, dest_ino, 0, 0).await.unwrap();
    fs.release(req, dest_ino, 0, 0, 0, false).await.unwrap();

    // Get block map id of dest file before overwrite to verify blocks are deleted
    let mut con = dlm.get_connection().await.unwrap();
    let dest_meta_key = format!("metadata:inode_{}", dest_ino);
    let dest_block_map_id: String = con.hget(&dest_meta_key, "block_map_id").await.unwrap();
    let dest_block_map_key = format!("block_map:{}", dest_block_map_id);

    // Verify block map key exists before rename
    let exists_before: bool = con.exists(&dest_block_map_key).await.unwrap();
    assert!(exists_before);

    // Rename src to dest (which overwrites dest)
    fs.rename(req, 1, OsStr::new("src.bin"), 1, OsStr::new("dest.bin")).await.unwrap();

    // Verify that the dest block map key has been deleted completely
    let exists_after: bool = con.exists(&dest_block_map_key).await.unwrap();
    assert!(!exists_after, "Overwritten file block map key was not deleted!");
}

#[tokio::test]
async fn test_data_integrity_known_sha512_hash() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "known_sha512_integrity";

    format_volume(
        &redis_url,
        fs_name,
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_known_sha")]),
        None,
        None,
        None,
        None,
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
        Some("128MB"),
        Some("500MB"),
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

    // Create striped file (4.2MB) with known data
    let reply_create = fs.create(req, 1, OsStr::new("known_sha512.bin"), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    let base_pattern = b"SQUEEZEFS_SHA512_CORRUPTION_TEST_SEQUENCE_";
    let mut data = Vec::with_capacity(base_pattern.len() * 100000);
    for _ in 0..100000 {
        data.extend_from_slice(base_pattern);
    }
    let expected_hash = "c70f30db7b4f0305f60d3f5b126cc3c2bc473f6c3b3398eb7564a40eecf06ef96456f1a75d031105cc601d416338f7fc53261b7abba2253d27c08760521f6534";

    // Write file
    fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();
    fs.release(req, ino, 0, 0, 0, false).await.unwrap();

    // Recreate FS to bypass memory cache
    drop(fs);

    let cache_recreate = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router_recreate = DataRouter::new(dlm.clone(), multi_backend.clone(), cache_recreate);
    let fs_recreate = SqueezefsFilesystem::new(router_recreate, dlm.clone(), 1000, 1000);
    fs_recreate.init(req).await.unwrap();

    let reply_read = fs_recreate.read(req, ino, 0, 0, data.len() as u32).await.unwrap();
    assert_eq!(reply_read.data.len(), data.len());
    let checksum = calculate_sha512(&reply_read.data);
    assert_eq!(checksum, expected_hash);
}

