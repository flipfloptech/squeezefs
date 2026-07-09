use criterion::{criterion_group, criterion_main, Criterion};
use squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator;
use squeezefs::meta_backend::kv::bset::{build_bset, lookup, merge, BsetView};
use squeezefs::meta_backend::kv::node::{NodeLayout, DEFAULT_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    NodeCache, NodeCacheConfig, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::record::{
    dentry_key, dentry_name_hash54, inode_key, DentryValue, InodeDelta, InodeValue, Record,
    TREE_INODES,
};
use squeezefs::meta_backend::kv::tree::{KvTree, SmoContext};
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata};
use std::hint::black_box;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;

fn bench_metalv_metadata(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 256 * 1024 * 1024).unwrap();
    rt.block_on(async {
        MetaLvBackend::format_v2_for_tests(&meta_storage, true, true, None).await
    })
    .unwrap();
    let backend = MetaLvBackend::new(meta_storage);

    let mut group = c.benchmark_group("meta_lv_metadata");

    group.bench_function("create_unlink_file", |b| {
        b.to_async(&rt).iter(|| {
            let name = format!("file_{}", rand::random::<u64>());
            let backend_ref = &backend;
            async move {
                let ino = backend_ref.create(1, &name, 0o644, 0, 0).await.unwrap().ino;
                backend_ref.unlink(1, &name).await.unwrap();
                // Reclaim the slot: unlink only drops the dentry/nlink, and a
                // leaked slot per iteration fills the inode table mid-warmup
                // once iterations get fast enough.
                backend_ref.destroy_inode(ino).await.unwrap();
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

/// A key-sorted bset image of `count` dentry records under one parent, with
/// real seeded 54-bit name hashes (PR K1 encodings, design §4.2).
fn dentry_bset_records(parent: u64, count: usize, hash_seed: u64) -> Vec<Record> {
    let mut keyed: Vec<([u8; 16], DentryValue)> = (0..count)
        .map(|i| {
            let name = format!("entry-{i:06}");
            let hash54 = dentry_name_hash54(name.as_bytes(), hash_seed);
            let value = DentryValue {
                child_ino: 100 + i as u64,
                file_type: 8, // DT_REG
                name: name.into_bytes(),
            };
            (dentry_key(parent, hash54, 0), value)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed
        .into_iter()
        .enumerate()
        .map(|(seq, (key, value))| {
            Record::put(
                key.to_vec(),
                seq as u64 + 1,
                value.encode().expect("encode"),
            )
        })
        .collect()
}

/// PR K1 micro-benches: bset build / search / merge / fold (design PR plan).
fn bench_kv_bset(c: &mut Criterion) {
    let mut group = c.benchmark_group("kv_bset");
    const HASH_SEED: u64 = 0x5EED_F00D;

    // Build: a compaction-output-sized bset of 2,048 dentry records.
    let records = dentry_bset_records(1, 2048, HASH_SEED);
    group.bench_function("build_2048_dentries", |b| {
        b.iter(|| build_bset(black_box(&records), 2048).expect("build"));
    });

    // Search: memcmp binary search over the parsed view, cycling keys.
    let image = build_bset(&records, 2048).expect("build");
    let view = BsetView::parse(&image).expect("parse");
    let probe_keys: Vec<Vec<u8>> = records.iter().map(|r| r.key.clone()).collect();
    group.bench_function("search_2048_dentries", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let key = &probe_keys[i % probe_keys.len()];
            i = i.wrapping_add(1);
            black_box(view.find(black_box(key)))
        });
    });

    // Merge: 8 sources × 256 interleaved inode keys — the compaction shape.
    let shard_images: Vec<Vec<u8>> = (0..8u64)
        .map(|shard| {
            let recs: Vec<Record> = (0..256u64)
                .map(|i| {
                    let ino = i * 8 + shard;
                    Record::put(
                        inode_key(ino).to_vec(),
                        ino + 1,
                        InodeValue {
                            size: ino,
                            ..InodeValue::default()
                        }
                        .encode(),
                    )
                })
                .collect();
            build_bset(&recs, 2048).expect("build")
        })
        .collect();
    let shard_views: Vec<BsetView<'_>> = shard_images
        .iter()
        .map(|im| BsetView::parse(im).expect("parse"))
        .collect();
    group.bench_function("merge_8x256", |b| {
        b.iter(|| merge(black_box(&shard_views)).count());
    });

    // Fold: point lookup across 4 sources with a Δtime chain over a base Put
    // — the fold algebra's hot lookup shape (§4.2).
    let base: Vec<Record> = (0..512u64)
        .map(|ino| {
            Record::put(
                inode_key(ino).to_vec(),
                ino + 1,
                InodeValue {
                    size: ino * 4096,
                    ..InodeValue::default()
                }
                .encode(),
            )
        })
        .collect();
    let base_image = build_bset(&base, 512).expect("build");
    let delta_images: Vec<Vec<u8>> = (1..=3u64)
        .map(|layer| {
            let recs: Vec<Record> = (0..512u64)
                .map(|ino| {
                    Record::delta(
                        inode_key(ino).to_vec(),
                        1000 * layer + ino,
                        &InodeDelta::times(layer, layer),
                    )
                })
                .collect();
            build_bset(&recs, 1000 * layer + 511).expect("build")
        })
        .collect();
    // Newest first: the three delta layers, then the base bset.
    let mut fold_views: Vec<BsetView<'_>> = delta_images
        .iter()
        .rev()
        .map(|im| BsetView::parse(im).expect("parse"))
        .collect();
    fold_views.push(BsetView::parse(&base_image).expect("parse"));
    group.bench_function("fold_lookup_4src_delta_chain", |b| {
        let mut ino = 0u64;
        b.iter(|| {
            let key = inode_key(ino % 512);
            ino = ino.wrapping_add(1);
            lookup(black_box(&fold_views), black_box(&key)).expect("fold")
        });
    });

    group.finish();
}

/// PR K5 micro-benches: btree point lookup (hot cache / cold demand page),
/// insert (commit-path resolve→lock→revalidate→apply), and range scan over
/// arc-swap snapshots (design PR plan; §4.5 read-path claims).
fn bench_kv_tree(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("kv_tree");
    const KEYS: u64 = 100_000;

    // A file-backed volume with a 100 K-key inode tree, fully written back.
    let build_volume = |budget_nodes: u64| -> (NamedTempFile, Arc<NodeCache>, KvTree, SmoContext) {
        let file = NamedTempFile::new().expect("temp volume");
        let node_size = DEFAULT_NODE_SIZE;
        let extents = 4096u64;
        file.as_file()
            .set_len(extents * node_size as u64)
            .expect("size volume");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout: NodeLayout::new(node_size).expect("layout"),
            heap_base: 0,
            budget_bytes: budget_nodes * node_size as u64,
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        let alloc = Arc::new(ExtentAllocator::format(extents, 0, 4096));
        let mut ctx = SmoContext::new(alloc);
        let tree = rt.block_on(async {
            let tree = KvTree::create(
                cache.clone(),
                &mut ctx,
                TREE_INODES,
                Arc::new(AtomicU64::new(0)),
            )
            .await
            .expect("create");
            for i in 0..KEYS {
                let mut v = vec![0u8; 48];
                v[..8].copy_from_slice(&i.to_le_bytes());
                tree.insert(&inode_key(i), v).await.expect("insert");
                if i % 4096 == 0 {
                    while tree.maintenance_pending() {
                        tree.run_maintenance(&mut ctx).await.expect("maintenance");
                    }
                }
            }
            tree.flush_dirty(&mut ctx).await.expect("flush");
            tree
        });
        (file, cache, tree, ctx)
    };

    // Hot point lookup: everything cached; latch-free snapshot fold.
    let (_hot_file, _hot_cache, hot_tree, _hot_ctx) = build_volume(4096);
    group.bench_function("point_lookup_hot_100k", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            let tree = &hot_tree;
            let key = inode_key(i % KEYS);
            i = i.wrapping_add(7919);
            async move {
                black_box(tree.lookup(black_box(&key)).await.expect("lookup"));
            }
        });
    });

    // Range scan: 1,024 records per iteration through held snapshots.
    group.bench_function("range_scan_1k_of_100k", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            let tree = &hot_tree;
            let start = inode_key((i * 1024) % (KEYS - 2048));
            i = i.wrapping_add(1);
            async move {
                let out = tree
                    .range(black_box(&start), &inode_key(KEYS), 1024)
                    .await
                    .expect("range");
                black_box(out.len());
            }
        });
    });

    // Cold point lookup: a 2-node budget forces demand paging (leaf
    // eviction) on nearly every probe — one 256 KiB uring read + snapshot
    // build per miss.
    let (_cold_file, _cold_cache, cold_tree, _cold_ctx) = build_volume(2);
    group.bench_function("point_lookup_cold_demand_page", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            let tree = &cold_tree;
            // Stride across leaves so consecutive probes never share one.
            let key = inode_key((i * 40_009) % KEYS);
            i = i.wrapping_add(1);
            async move {
                black_box(tree.lookup(black_box(&key)).await.expect("lookup"));
            }
        });
    });

    // Insert: resolve → lock → revalidate → apply + snapshot swap, with
    // maintenance amortized inline (the K6b cadence stand-in; the tokio
    // Mutex is uncontended — it exists to carry &mut SmoContext into the
    // async closure).
    let (_ins_file, _ins_cache, ins_tree, ins_ctx) = build_volume(4096);
    let ins_ctx = tokio::sync::Mutex::new(ins_ctx);
    group.bench_function("insert_48b", |b| {
        let mut i = KEYS;
        b.to_async(&rt).iter(|| {
            let tree = &ins_tree;
            let ctx = &ins_ctx;
            let key = inode_key(i);
            let mut v = vec![0u8; 48];
            v[..8].copy_from_slice(&i.to_le_bytes());
            i += 1;
            async move {
                tree.insert(black_box(&key), v).await.expect("insert");
                if tree.maintenance_pending() {
                    let mut ctx = ctx.lock().await;
                    tree.run_maintenance(&mut ctx).await.expect("maintenance");
                }
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_metalv_metadata, bench_kv_bset, bench_kv_tree);
criterion_main!(benches);
