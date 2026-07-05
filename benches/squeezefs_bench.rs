use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use squeezefs::crypto_compress::CryptoCompressState;
use tokio::runtime::Runtime;
use rsa::RsaPrivateKey;
use rsa::pkcs1::EncodeRsaPrivateKey;

fn bench_crypto_compress(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Generate RSA key for benchmark encryption modes
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pem = priv_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();

    let mut group = c.benchmark_group("crypto_compress_throughput");

    let sizes = [4 * 1024, 4 * 1024 * 1024]; // 4KB and 4MB

    // 1. None / Passthrough
    let state_none = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
    // 2. LZ4 compression
    let state_lz4 = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    // 3. Zstd compression
    let state_zstd = CryptoCompressState::new("zstd".to_string(), "none".to_string(), None);
    // 4. AES-256-GCM encryption
    let state_aes = CryptoCompressState::new("none".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));
    // 5. ChaCha20-Poly1305 encryption
    let state_chacha = CryptoCompressState::new("none".to_string(), "chacha20-rsa".to_string(), Some(&pem));
    // 6. LZ4 + AES-256-GCM combined
    let state_combined = CryptoCompressState::new("lz4".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));

    for &size in &sizes {
        let input_data = bytes::Bytes::from(vec![0xAAu8; size]);

        // None/Passthrough
        group.bench_with_input(BenchmarkId::new("passthrough_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_none;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });

        // LZ4
        group.bench_with_input(BenchmarkId::new("lz4_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_lz4;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });

        // Zstd
        group.bench_with_input(BenchmarkId::new("zstd_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_zstd;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });

        // AES-256-GCM
        group.bench_with_input(BenchmarkId::new("aes_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_aes;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });

        // ChaCha20-Poly1305
        group.bench_with_input(BenchmarkId::new("chacha_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_chacha;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });

        // LZ4 + AES combined
        group.bench_with_input(BenchmarkId::new("lz4_aes_combined_write", size), &input_data, |b, data| {
            b.to_async(&rt).iter(|| {
                let state = &state_combined;
                let data = data.clone();
                async move {
                    state.process_write_async(data).await.unwrap()
                }
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_crypto_compress);
criterion_main!(benches);
