use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::keyfile::{derive_volume_key, KeyMaterial, VolumeKey};
use tokio::runtime::Runtime;

/// The mount-resolved volume key the encrypted benches run under (KW-1:
/// HKDF-SHA-256 over the operator's key file material + the volume's KDF
/// salt — `docs/design-key-handling.md` §2). FIELD shape: 44 B of base64
/// key material, the documented `head -c 32 /dev/urandom | base64` form.
fn bench_volume_key() -> VolumeKey {
    let material =
        KeyMaterial::from_bytes(b"NQmQmLQ0y4nqRz9m2rQK4Zt8m0Yy5tJp1c7Hh4Vd0Yg=".to_vec())
            .expect("bench key material");
    derive_volume_key(&material, &[0x5Au8; 32])
}

fn bench_crypto_compress(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let key = bench_volume_key();

    let mut group = c.benchmark_group("crypto_compress_throughput");

    let sizes = [4 * 1024, 4 * 1024 * 1024]; // 4KB and 4MB

    // 1. None / Passthrough
    let state_none = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
    // 2. LZ4 compression
    let state_lz4 = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    // 3. Zstd compression
    let state_zstd = CryptoCompressState::new("zstd".to_string(), "none".to_string(), None);
    // 4. AES-256-GCM encryption
    let state_aes =
        CryptoCompressState::new("none".to_string(), "aes256gcm".to_string(), Some(&key));
    // 5. ChaCha20-Poly1305 encryption
    let state_chacha =
        CryptoCompressState::new("none".to_string(), "chacha20".to_string(), Some(&key));
    // 6. LZ4 + AES-256-GCM combined
    let state_combined =
        CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&key));

    // §5.7 CRYPTO_SCRATCH_POOL: production mounts initialize the scratch
    // pool from the configured block size (`DataRouter::set_crypto`);
    // mirror that so the named benches measure the shipping pooled path.
    let block_size = 4 * 1024 * 1024;
    for state in [
        &state_lz4,
        &state_zstd,
        &state_aes,
        &state_chacha,
        &state_combined,
    ] {
        state.init_scratch_pool(block_size);
    }

    // Heap-path comparison pair (§5.7): identical config, pool never
    // initialized — quantifies pooled scratch vs per-transform heap `Vec`s
    // on the same workload.
    let state_combined_unpooled =
        CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&key));

    for &size in &sizes {
        let input_data = bytes::Bytes::from(vec![0xAAu8; size]);

        // None/Passthrough
        group.bench_with_input(
            BenchmarkId::new("passthrough_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_none;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // LZ4
        group.bench_with_input(
            BenchmarkId::new("lz4_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_lz4;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // Zstd
        group.bench_with_input(
            BenchmarkId::new("zstd_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_zstd;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // AES-256-GCM
        group.bench_with_input(
            BenchmarkId::new("aes_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_aes;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // ChaCha20-Poly1305
        group.bench_with_input(
            BenchmarkId::new("chacha_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_chacha;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // LZ4 + AES combined
        group.bench_with_input(
            BenchmarkId::new("lz4_aes_combined_write", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_combined;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );

        // LZ4 + AES combined, heap path (§5.7 pooled-vs-heap comparison)
        group.bench_with_input(
            BenchmarkId::new("lz4_aes_combined_write_unpooled", size),
            &input_data,
            |b, data| {
                b.to_async(&rt).iter(|| {
                    let state = &state_combined_unpooled;
                    let data = data.clone();
                    async move { state.process_write_async(data).await.unwrap() }
                });
            },
        );
    }

    group.finish();
}

/// Micro-benches for the `squeezefs bench` engine helpers that sit on the
/// load generator's per-op path: the deterministic pattern fill (runs once
/// per written block — must comfortably outpace the mount's write
/// throughput) and the size parser / block-order shuffle (startup cost).
fn bench_bench_engine_helpers(c: &mut Criterion) {
    use squeezefs::bench::{block_order, fill_block, parse_size};

    let mut group = c.benchmark_group("bench_engine_helpers");

    for (label, len) in [("4k", 4usize * 1024), ("1m", 1024 * 1024)] {
        let mut buf = vec![0u8; len];
        group.throughput(criterion::Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("fill_block", label), &len, |b, _| {
            let mut block = 0u64;
            b.iter(|| {
                block = block.wrapping_add(1);
                fill_block(&mut buf, 3, 7, block);
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("bench_engine_setup");
    group.bench_function("parse_size", |b| {
        b.iter(|| {
            parse_size(std::hint::black_box("128k")).unwrap()
                + parse_size(std::hint::black_box("10g")).unwrap()
                + parse_size(std::hint::black_box("1048576")).unwrap()
        })
    });
    // 1 GiB @ 1 MiB blocks = 1024 entries: the default shape's shuffle.
    group.bench_function("block_order_rand_1024", |b| {
        b.iter(|| block_order(std::hint::black_box(true), 1024))
    });
    group.finish();
}

/// KW-1 key-wrap micro-benches (`docs/design-key-handling.md`): the mount
/// path pays `derive_volume_key` once and a cold `unwrap_session_key` per
/// distinct wrap blob; the transformed-volume I/O path pays the CACHED
/// resolve on every single block. FIELD shape: the 44 B base64 key
/// material the docs tell operators to generate, the 32 B AEAD data key
/// the write path mints per session, and the 63 B wrap blob every block
/// header carries.
fn bench_key_wrap(c: &mut Criterion) {
    let key = bench_volume_key();
    let material =
        KeyMaterial::from_bytes(b"NQmQmLQ0y4nqRz9m2rQK4Zt8m0Yy5tJp1c7Hh4Vd0Yg=".to_vec()).unwrap();
    let salt = [0x5Au8; 32];
    let mut group = c.benchmark_group("crypto_key_wrap");

    // Mount path: one HKDF extract + two expands per mount.
    group.bench_function("derive_volume_key", |b| {
        b.iter(|| derive_volume_key(std::hint::black_box(&material), std::hint::black_box(&salt)))
    });

    for algo in ["aes256gcm", "chacha20"] {
        let state = CryptoCompressState::new("none".to_string(), algo.to_string(), Some(&key));
        let data_key = [0x42u8; 32];
        let blob = state.wrap_session_key(&data_key).unwrap();

        group.bench_function(BenchmarkId::new("wrap_session_key", algo), |b| {
            b.iter(|| {
                state
                    .wrap_session_key(std::hint::black_box(&data_key))
                    .unwrap()
            })
        });
        // Cold unwrap: what a first-touch block header costs before the
        // moka memo covers it.
        group.bench_function(BenchmarkId::new("unwrap_session_key_cold", algo), |b| {
            b.iter(|| {
                state
                    .unwrap_session_key(std::hint::black_box(&blob))
                    .unwrap()
            })
        });
        // The transformed-volume I/O path: every block resolves the
        // session key through this (P2-3 precomputed fast path).
        group.bench_function(BenchmarkId::new("resolve_encrypt_key_cached", algo), |b| {
            b.iter(|| state.resolve_encrypt_key().unwrap())
        });
        // The read side's memoized unwrap (moka hit) as the block header
        // decoder sees it: one 4 KiB encrypted block, warm cache.
        let payload = vec![0xA5u8; 4096];
        let encrypted = state.encrypt(&payload).unwrap();
        let _ = state.decrypt(&encrypted).unwrap(); // prime the memo
        group.bench_function(BenchmarkId::new("decrypt_4k_cached_unwrap", algo), |b| {
            b.iter(|| state.decrypt(std::hint::black_box(&encrypted)).unwrap())
        });
    }
    group.finish();
}

/// **PERF-10 · the transformed stored image's DMA grain.**
///
/// A compressed/encrypted image's length is a transform artifact, so it was
/// essentially never a 4 KiB multiple and `NvmeBlockDev::write_block` took
/// its bounce branch for EVERY transformed block: a third copy (pooled
/// scratch -> aligned bounce buffer, plus the tail memset) on top of the
/// merge copy and the DMA. The stored image is now zero-padded to the grain
/// inside the pooled scratch, so the aligned (zero-copy) branch applies.
///
/// The two arms price the trade directly:
/// * `pad_fill_write_4m` - the shipped cost: `process_write` including the
///   <= 4 KiB pad memset (the existing `crypto_compress_throughput` group
///   covers the transform itself; this isolates the 4 MiB field shape).
/// * `bounce_copy_4m` - the removed cost: exactly what the unaligned branch
///   did (pooled-aligned alloc + memcpy of the stored image + tail memset).
fn bench_stored_image_grain(c: &mut Criterion) {
    use squeezefs::cache::pool::POOLED_BUF_ALIGN;
    use std::hint::black_box;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let key = bench_volume_key();
    let block_size = 4 * 1024 * 1024;
    let state = CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&key));
    state.init_scratch_pool(block_size);

    // Incompressible payload: the store-raw escape's long image, i.e. the
    // worst (and most common on real data) stored length.
    let mut seed: u64 = 0x5EED_F00D;
    let payload: Vec<u8> = (0..block_size)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        })
        .collect();
    let payload = bytes::Bytes::from(payload);
    let stored_len = rt
        .block_on(state.process_write_async(payload.clone()))
        .expect("transform")
        .len();

    let mut group = c.benchmark_group("crypto_stored_image_grain");
    group.throughput(criterion::Throughput::Bytes(stored_len as u64));
    group.bench_function("pad_fill_write_4m", |b| {
        b.to_async(&rt).iter(|| {
            let state = &state;
            let data = payload.clone();
            async move { state.process_write_async(data).await.unwrap() }
        });
    });
    group.bench_function("bounce_copy_4m", |b| {
        let src = vec![0xA5u8; stored_len];
        let aligned = stored_len.next_multiple_of(POOLED_BUF_ALIGN);
        b.iter(|| {
            let mut dst: Vec<u8> = Vec::with_capacity(aligned);
            // SAFETY: capacity reserved above; every byte written below.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    black_box(src.as_ptr()),
                    dst.as_mut_ptr(),
                    stored_len,
                );
                std::ptr::write_bytes(dst.as_mut_ptr().add(stored_len), 0, aligned - stored_len);
                dst.set_len(aligned);
            }
            black_box(dst)
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_stored_image_grain,
    bench_crypto_compress,
    bench_key_wrap,
    bench_bench_engine_helpers
);
criterion_main!(benches);
