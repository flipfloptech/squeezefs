//! §5.7 CRYPTO_SCRATCH_POOL wiring (zero-copy write-path design, severable
//! sub-commit).
//!
//! Contract under test:
//!
//! - **Mount wiring**: `DataRouter::set_crypto` initializes the state's
//!   scratch pool from the router's *configured* block size (FUSE `init`
//!   calls `set_block_size(config.block_size)` immediately before
//!   `set_crypto`), and the `cache.nvme` crypto clone shares that same
//!   pool — non-passthrough mounts transform into pooled scratch, never
//!   per-block heap `Vec`s.
//! - **Pooled transform → DMA read-back parity**: a block transformed
//!   through the pooled scratch survives the real io_uring
//!   `NvmeBlockDev::write_block` / `read_block` cycle byte-for-byte and
//!   decodes through the untouched read path (the pooled backing's
//!   keep-alive owner must hold across uring completion).
//!
//! RED against current code: `init_scratch_pool` / `scratch_pool_buf_len`
//! do not exist yet (compile failure); once the API lands, the wiring test
//! fails until `set_crypto` actually initializes the pool.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

static TEST_KEY: std::sync::OnceLock<squeezefs::keyfile::VolumeKey> = std::sync::OnceLock::new();

/// The mount-resolved volume key these tests encrypt under (KW-1 — the
/// operator's key file material + the volume's KDF salt, never a PEM).
fn test_key() -> &'static squeezefs::keyfile::VolumeKey {
    TEST_KEY.get_or_init(|| {
        let material = squeezefs::keyfile::KeyMaterial::from_bytes(
            b"squeezefs-test-key-material-0123456789".to_vec(),
        )
        .expect("test key material");
        squeezefs::keyfile::derive_volume_key(&material, &[0x33u8; 32])
    })
}

struct H {
    router: DataRouter,
    dev: Arc<NvmeBlockDev>,
    _b: NamedTempFile,
    _s: TempDir,
}

async fn make(test_id: &str) -> H {
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba, nvme.clone());
    H {
        router,
        dev: nvme,
        _b: b,
        _s: s,
    }
}

fn mixed_payload(len: usize) -> Vec<u8> {
    let mut v = vec![0x5Au8; len];
    let tail = len / 4;
    let mut seed: u64 = 0x5EED;
    for byte in &mut v[len - tail..] {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *byte = (seed >> 33) as u8;
    }
    v
}

/// `set_crypto` must size the CRYPTO_SCRATCH_POOL off the router's
/// configured block size — not a compiled-in default — and hand the
/// `cache.nvme` clone the same shared pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_set_crypto_inits_scratch_pool_from_block_size() {
    let h = make("scratch_wiring").await;
    let bs: usize = 1024 * 1024; // deliberately non-default (default is 4 MiB)
    h.router.set_block_size(bs as u64);

    let state =
        CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(test_key()));
    h.router.set_crypto(state);

    let buf_len = h
        .router
        .get_crypto()
        .scratch_pool_buf_len()
        .expect("set_crypto must initialize the scratch pool for non-passthrough mounts");

    // Sized from the CONFIGURED block size: worst case is a shade over the
    // block (≤ +128 KiB per §5.7), nowhere near the 4 MiB default's shape.
    assert!(buf_len > bs, "worst case must exceed the block size");
    assert!(
        buf_len <= bs + 128 * 1024,
        "pool sized for the configured 1 MiB block, got {buf_len}"
    );

    // A reference state initialized directly for the same block size and
    // key agrees exactly (same §5.7 formula, same actual wrapped-key blob).
    let reference =
        CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(test_key()));
    reference.init_scratch_pool(bs);
    assert_eq!(Some(buf_len), reference.scratch_pool_buf_len());

    // The staging-flush clone (write_block_from_staging's transform leg)
    // must see the same initialized pool.
    assert_eq!(
        h.router
            .cache
            .nvme
            .crypto
            .get()
            .expect("set_crypto must populate the staging crypto clone")
            .scratch_pool_buf_len(),
        Some(buf_len)
    );
}

/// Pooled transform output survives the real uring DMA + read-back and
/// decodes through the untouched read path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pooled_transform_dma_read_back_parity() {
    let h = make("scratch_dma_parity").await;
    let bs: usize = 256 * 1024;
    h.router.set_block_size(bs as u64);
    h.router.set_crypto(CryptoCompressState::new(
        "lz4".to_string(),
        "aes256gcm".to_string(),
        Some(test_key()),
    ));
    let crypto = h.router.get_crypto();
    assert!(crypto.scratch_pool_buf_len().is_some());

    let payload = bytes::Bytes::from(mixed_payload(bs));
    let processed = crypto.process_write_async(payload.clone()).await.unwrap();

    h.dev.write_block(0, processed.clone()).await.unwrap();
    let raw = h.dev.read_block(0, processed.len()).await.unwrap();
    assert_eq!(
        raw.as_ref(),
        processed.as_ref(),
        "DMA image must match the pooled transform output byte-for-byte"
    );
    let plain = crypto.process_read_async(raw).await.unwrap();
    assert_eq!(
        plain.as_ref(),
        payload.as_ref(),
        "read path must recover the plaintext from the pooled image"
    );
}

/// **PERF-10 — a pooled transformed image is 4 KiB-grained, so the DMA
/// takes its ALIGNED (zero-copy) branch.**
///
/// A compressed/encrypted image's length is a transform artifact and is
/// essentially never a 4 KiB multiple, so `NvmeBlockDev::write_block` used
/// to take its bounce branch for EVERY transformed block — a third copy
/// (pooled scratch → aligned bounce buffer) on top of the merge copy and
/// the DMA. The stored image is now zero-padded to the 4 KiB grain (the
/// frame is self-delimiting, so readers ignore the pad) whenever the
/// padded length fits both the scratch backing and the allocator chunk.
///
/// Instrument: `nvme_unaligned_write_fallbacks` must not move across a
/// transformed write cycle. RED pre-fix: the counter moves by 1 per block
/// and the image length is not a 4 KiB multiple.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pooled_transform_is_dma_aligned_no_bounce() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;

    let h = make("scratch_dma_aligned").await;
    let bs: usize = 256 * 1024;
    h.router.set_block_size(bs as u64);
    h.router.set_crypto(CryptoCompressState::new(
        "lz4".to_string(),
        "aes256gcm".to_string(),
        Some(test_key()),
    ));
    let crypto = h.router.get_crypto();
    assert!(crypto.scratch_pool_buf_len().is_some(), "pooled path armed");

    // Two shapes: compressible (short image) and incompressible (the
    // store-raw escape's long image). Both must land on the grain.
    for (tag, payload) in [
        ("compressible", vec![0xA7u8; bs]),
        ("incompressible", mixed_payload(bs)),
    ] {
        let payload = bytes::Bytes::from(payload);
        let stored = crypto.process_write_async(payload.clone()).await.unwrap();
        assert_eq!(
            stored.len() % 4096,
            0,
            "{tag}: stored image length {} is not a 4 KiB multiple — the DMA \
             will bounce (PERF-10)",
            stored.len()
        );
        assert_eq!(
            stored.as_ptr() as usize % 4096,
            0,
            "{tag}: pooled image must stay 4 KiB-aligned"
        );

        // The instrument: a real DMA of this image must not bounce.
        let before = METRICS
            .nvme_unaligned_write_fallbacks
            .load(Ordering::Relaxed);
        h.dev.write_block(0, stored.clone()).await.unwrap();
        assert_eq!(
            METRICS
                .nvme_unaligned_write_fallbacks
                .load(Ordering::Relaxed),
            before,
            "{tag}: transformed write took the unaligned bounce path (PERF-10)"
        );

        // Round-trip through the untouched read path: the pad is invisible.
        let raw = h.dev.read_block(0, stored.len()).await.unwrap();
        let plain = crypto.process_read_async(raw).await.unwrap();
        assert_eq!(
            plain.as_ref(),
            payload.as_ref(),
            "{tag}: padding must not disturb the plaintext round-trip"
        );
    }
}
