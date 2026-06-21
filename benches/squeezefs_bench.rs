use criterion::{criterion_group, criterion_main, Criterion};
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use tempfile::tempdir;
use tokio::runtime::Runtime;

fn bench_squeezefs_routing(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

    // Check if redis/garnet is running, if not skip benchmarking
    let connection_ok = rt.block_on(async {
        if let Ok(client) = redis::Client::open(redis_url.clone()) {
            client.get_multiplexed_tokio_connection().await.is_ok()
        } else {
            false
        }
    });

    if !connection_ok {
        println!("Skipping benchmarks: Redis/Garnet server not available.");
        return;
    }

    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = rt.block_on(RustFsClient::new());
    let temp_dir = tempdir().unwrap();
    let cache = TieredCache::new(
        temp_dir.path().to_path_buf(),
        backend.clone(),
        dlm.redis_client().clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm, backend, cache);

    let mut group = c.benchmark_group("squeezefs_routing_writes");

    // Bench Micro-file routing path (< 64KB)
    let micro_data = vec![8u8; 1024]; // 1KB
    group.bench_function("write_micro_file_1kb", |b| {
        b.to_async(&rt).iter(|| async {
            router
                .write_file("bench_micro.bin", &micro_data, 1)
                .await
                .unwrap();
        });
    });

    // Bench Small-file routing path (64KB - 4MB)
    let small_data = vec![8u8; 128 * 1024]; // 128KB
    group.bench_function("write_small_file_128kb", |b| {
        b.to_async(&rt).iter(|| async {
            router
                .write_file("bench_small.bin", &small_data, 2)
                .await
                .unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_squeezefs_routing);
criterion_main!(benches);
