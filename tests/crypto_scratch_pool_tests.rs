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

static TEST_PEM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn test_pem() -> &'static str {
    TEST_PEM.get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let mut rng = rand::thread_rng();
        rsa::RsaPrivateKey::new(&mut rng, 2048)
            .unwrap()
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .unwrap()
            .to_string()
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

    let state = CryptoCompressState::new(
        "lz4".to_string(),
        "aes256gcm-rsa".to_string(),
        Some(test_pem()),
    );
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
    let reference = CryptoCompressState::new(
        "lz4".to_string(),
        "aes256gcm-rsa".to_string(),
        Some(test_pem()),
    );
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
        "aes256gcm-rsa".to_string(),
        Some(test_pem()),
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
