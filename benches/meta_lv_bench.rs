use criterion::{criterion_group, criterion_main, Criterion};
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata};
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;

fn bench_metalv_metadata(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 64 * 1024 * 1024).unwrap();
    rt.block_on(async { MetaLvBackend::format(&meta_storage).await })
        .unwrap();
    let backend = MetaLvBackend::new(meta_storage);

    let mut group = c.benchmark_group("meta_lv_metadata");

    group.bench_function("create_unlink_file", |b| {
        b.to_async(&rt).iter(|| {
            let name = format!("file_{}", rand::random::<u64>());
            let backend_ref = &backend;
            async move {
                backend_ref.create(1, &name, 0o644, 0, 0).await.unwrap();
                backend_ref.unlink(1, &name).await.unwrap();
            }
        });
    });

    group.bench_function("lookup_file", |b| {
        let _ = rt.block_on(async { backend.create(1, "lookup_target", 0o644, 0, 0).await });
        b.to_async(&rt).iter(|| {
            let backend_ref = &backend;
            async move {
                backend_ref.lookup(1, "lookup_target").await.unwrap();
            }
        });
    });

    group.bench_function("set_get_xattr", |b| {
        let _ = rt.block_on(async { backend.create(1, "xattr_target", 0o644, 0, 0).await });
        let ino = rt.block_on(async { backend.lookup(1, "xattr_target").await.unwrap().ino });
        let val = b"benchmark_value";
        b.to_async(&rt).iter(|| {
            let backend_ref = &backend;
            async move {
                backend_ref.setxattr(ino, "user.bench", val).await.unwrap();
                let res = backend_ref
                    .getxattr(ino, "user.bench")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(res.len(), val.len());
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_metalv_metadata);
criterion_main!(benches);
