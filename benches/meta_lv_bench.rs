use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator;
use squeezefs::meta_backend::kv::backend::{xattr_name_allowed, KvMetaBackend};
use squeezefs::meta_backend::kv::bset::{build_bset, lookup, merge, BsetView};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::node::{NodeLayout, DEFAULT_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    NodeCache, NodeCacheConfig, RootEpoch, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::record::{
    dentry_key, dentry_name_hash54, inode_key, DentryValue, InodeDelta, InodeValue, Record,
    TREE_INODES,
};
use squeezefs::meta_backend::kv::tree::{KvTree, SmoContext};
use squeezefs::meta_backend::Metadata;
use std::hint::black_box;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;

/// PR K7 (§8 micro gate): the metadata trait benches against a
/// `KvMetaBackend` behind the `Metadata` trait (baseline names unchanged
/// — `kv_meta_metadata/*` — so criterion history stays comparable). Plus
/// the K7 streaming surface: one cookie-paged `readdir_page` step. (The
/// retired v2 `meta_lv_metadata` group was deleted with v2 support.)
fn bench_kv_meta_metadata(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(256 * 1024 * 1024).unwrap();
    let backend = rt.block_on(async {
        format_v3(
            file.path(),
            256 * 1024 * 1024,
            &FormatV3Options {
                node_size: DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: false,
                full_wipe: false,
                format_config_xattr: None,
            },
        )
        .await
        .expect("format v3");
        KvMetaBackend::open(file.path()).await.expect("mount v3")
    });

    let mut group = c.benchmark_group("kv_meta_metadata");

    group.bench_function("create_unlink_file", |b| {
        b.to_async(&rt).iter(|| {
            let name = format!("file_{}", rand::random::<u64>());
            let backend_ref = &backend;
            async move {
                let ino = backend_ref.create(1, &name, 0o644, 0, 0).await.unwrap().ino;
                backend_ref.unlink(1, &name).await.unwrap();
                // 3-op sequence: the destroy reaps the inode record —
                // monotonic inos never reuse (§4.8).
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

    // One §5.1 streaming step: a 100-entry cookie page out of a 1,000-entry
    // directory (the FUSE readdir hot shape after K7). Setup is hoisted
    // out of the bench closure — criterion may invoke the routine closure
    // more than once, and this population is not idempotent.
    let paged_dir = rt.block_on(async {
        let dir = backend
            .create(1, "paged_dir", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        for i in 0..1000u32 {
            backend
                .create(dir, &format!("e{i:04}"), 0o644, 0, 0)
                .await
                .unwrap();
        }
        dir
    });
    group.bench_function("readdir_page_100_of_1k", |b| {
        let dir = paged_dir;
        b.to_async(&rt).iter(|| {
            let backend_ref = &backend;
            async move {
                let page = backend_ref.readdir_page(dir, 0, 100).await.unwrap();
                assert_eq!(page.len(), 100);
                black_box(page);
            }
        });
    });

    group.finish();
    rt.block_on(async { backend.shutdown().await.expect("clean shutdown") });
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

/// POSIX-4 (`docs/pre-rc-engineering-spec.md` §5): the cost of resolving
/// `readdir`'s `..` entry, scan vs memo.
///
/// **Field-derived shape.** The scan arm is `find_parent_of_child` — an
/// unindexed range scan of a volume's ENTIRE dentry tree, so its input
/// size is the volume's total dentry count, not the directory's. Sizes
/// here (256 / 2,048 / 16,384 dentries) are the measurable low end of a
/// v3 volume whose caps are 1 M entries per directory and ≥ 100 M inodes
/// (AGENTS.md, design-cow-kv-metadata §4.2): the arm is LINEAR in that
/// count, and the shipped `..` path used to pay one per `readdir`, i.e.
/// once per directory of every `ls -R` / `find` / `du` / `rsync` / `tar`
/// walk. The worst position is measured deliberately — the probe child
/// is the LAST dentry inserted, which is where a full scan ends up on a
/// memcmp-ordered tree for a high ino.
///
/// The memo arm prices what ships instead: one `moka::sync::Cache<u64,
/// u64, ahash::RandomState>` get, built exactly as
/// `SqueezefsFilesystem::parent_memo` is (the field itself is private to
/// the daemon).
fn bench_readdir_parent(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("readdir_parent");
    group.throughput(criterion::Throughput::Elements(1));

    for dentries in [256u64, 2_048, 16_384] {
        let file = NamedTempFile::new().unwrap();
        file.as_file().set_len(512 * 1024 * 1024).unwrap();
        let (backend, probe) = rt.block_on(async {
            format_v3(
                file.path(),
                512 * 1024 * 1024,
                &FormatV3Options {
                    node_size: DEFAULT_NODE_SIZE,
                    journal_len_override: None,
                    force: false,
                    full_wipe: false,
                    format_config_xattr: None,
                },
            )
            .await
            .expect("format v3");
            let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
            // One directory of `dentries` children; the LAST one is the
            // probe (worst-case scan position).
            let dir = be
                .create(1, "walkdir", 0o755 | 0o040000, 0, 0)
                .await
                .unwrap();
            let mut last = dir.ino;
            for i in 0..dentries {
                last = be
                    .create(dir.ino, &format!("child_{i:07}"), 0o644, 0, 0)
                    .await
                    .unwrap()
                    .ino;
            }
            (be, last)
        });

        group.bench_with_input(
            BenchmarkId::new("scan_find_parent_of_child", dentries),
            &probe,
            |b, &child| {
                b.to_async(&rt).iter(|| {
                    let be = &backend;
                    async move { black_box(be.find_parent_of_child(child).await.unwrap()) }
                });
            },
        );
    }

    // The shipped path: a memo get. Sized/typed exactly like
    // `SqueezefsFilesystem::parent_memo`.
    let memo: moka::sync::Cache<u64, u64, ahash::RandomState> = moka::sync::Cache::builder()
        .max_capacity(50_000)
        .time_to_live(std::time::Duration::from_secs(300))
        .build_with_hasher(ahash::RandomState::new());
    for child in 0..10_000u64 {
        memo.insert(child + 2, 1);
    }
    group.bench_function("memo_hit", |b| {
        let mut n = 0u64;
        b.iter(|| {
            n = (n + 1) % 10_000;
            black_box(memo.get(black_box(&(n + 2))))
        });
    });
    group.bench_function("memo_miss", |b| {
        b.iter(|| black_box(memo.get(black_box(&u64::MAX))));
    });

    group.finish();
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

    // Cold point lookup: a 3-node budget forces demand paging (leaf
    // eviction) on nearly every probe — one 256 KiB uring read + snapshot
    // build per miss. (3, not 2, since PR M9: a node's charge is extent +
    // overlay + memo bytes, so pinned-interior + one dirty leaf already
    // fills a 2-node budget during the build and the writer's freshly
    // loaded leaf would evict-livelock — the §5.7 accounting needs one
    // slot of headroom here; probes still miss on nearly every stride.)
    let (_cold_file, _cold_cache, cold_tree, _cold_ctx) = build_volume(3);
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
/// **The node-cache hit path** — the hottest metadata path in the tree:
/// every traversal step of every lookup/create/unlink resolves its node
/// through [`NodeCache::try_get`], so anything added here is paid per
/// level per operation. It is also the venue for the spec §6.8 item-2
/// reader-revalidation epoch gate, whose acceptance bar is "~free when
/// nothing changed": the gate is one relaxed load of the cache's epoch
/// word plus one relaxed load of the node's stamp and a compare.
///
/// Field-derived shapes (`.benchmarks/2026-08-01-rewrite-publish-drain.md`
/// §3 — the conveyor's per-pass node population; `kv_tree`'s 100 K-key
/// tree is 3 levels deep):
/// * `try_get_hit` — one hot node, the repeat-resolve case (a create
///   storm hammering one parent's leaf).
/// * `try_get_hit_rotating16` — 16 distinct addresses in rotation, which
///   defeats the single-line locality of the first row and is the shape a
///   multi-directory walk (`find`) actually presents.
/// * `try_get_absent` — the unmapped-address probe (the demand-page
///   entry), so the miss classification cost is on the record too.
/// * `try_get_hit_armed` — the same probe on a cache that armed reader
///   revalidation, so the epoch compare is a real one (nonzero vs nonzero)
///   rather than `0 == 0`.
/// * `revalidate_inert_poll` — what the derived cadence costs when the
///   writer minted no record: the common case, and the one that must be
///   ~nothing.
/// * `revalidate_drop_pass/64` — a root flip releasing 64 mapped nodes:
///   the drop pass, the clock-ring drain, and the budget credits.
fn bench_kv_node_cache(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::node::{write_node, NodeWriteParams, MIN_NODE_SIZE};

    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("kv_node_cache");
    // 64 KiB nodes keep the fixture's device reads small; the hit path
    // never touches node bytes, so node size is irrelevant to the rows.
    const NODES: u64 = 64;
    let node_size = MIN_NODE_SIZE;
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file()
        .set_len((NODES + 1) * node_size as u64)
        .expect("size volume");
    let layout = NodeLayout::new(node_size).expect("layout");
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout,
        heap_base: 0,
        budget_bytes: (NODES + 1) * node_size as u64,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    let addrs: Vec<u64> = (0..NODES).map(|e| cache.extent_addr(e)).collect();
    rt.block_on(async {
        for (i, addr) in addrs.iter().enumerate() {
            write_node(
                cache.config().path.clone(),
                &cache.config().layout,
                &NodeWriteParams {
                    node_addr: *addr,
                    node_seq: i as u64 + 1,
                    tree_id: TREE_INODES,
                    level: 0,
                    min_key: b"",
                    max_key: &[0xff; 8],
                },
                &[],
                0,
            )
            .await
            .expect("write node image");
            cache.load(*addr).await.expect("load").expect("mapped");
        }
    });

    let hot = addrs[0];
    group.bench_function("try_get_hit", |b| {
        b.iter(|| black_box(cache.try_get(black_box(hot))).is_some());
    });

    group.bench_function("try_get_hit_rotating16", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 1) & 15;
            black_box(cache.try_get(black_box(addrs[i]))).is_some()
        });
    });

    let absent = cache.extent_addr(NODES); // written by nobody, never mapped
    group.bench_function("try_get_absent", |b| {
        b.iter(|| black_box(cache.try_get(black_box(absent))).is_none());
    });

    // ---- The spec §6.8 item-2 reader rows (this cache is ARMED from here
    // on, so the rows above must be measured first — the hit-path rows are
    // the un-armed, shipped posture by construction).
    let armed = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout,
        heap_base: 0,
        budget_bytes: (NODES + 1) * node_size as u64,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    armed
        .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
        .expect("arm");
    rt.block_on(async {
        for addr in &addrs {
            armed.load(*addr).await.expect("load").expect("mapped");
        }
    });

    // The armed hit path: the same probe with a NONZERO epoch in force, so
    // the compare is a real one instead of 0 == 0.
    group.bench_function("try_get_hit_armed", |b| {
        b.iter(|| black_box(armed.try_get(black_box(hot))).is_some());
    });

    // An inert poll — the cadence's cost when the writer minted nothing.
    // This is what a reader pays per interval in the common case.
    let same = RootEpoch::synthetic(1, 0, &[]);
    group.bench_function("revalidate_inert_poll", |b| {
        b.iter(|| black_box(armed.revalidate(black_box(&same))).advanced);
    });

    // The drop pass on a root flip: `NODES` mapped nodes released, the clock
    // ring drained and re-seeded, the budget credited.
    //
    // `iter_custom`, not `iter_batched`: criterion runs a batch's setups
    // BEFORE its timed routines, so with batching only the first pass of each
    // batch would find a populated map and the row would report the cost of
    // sweeping an empty cache (measured: ~1 ns/node — the tell). Here the
    // reload is re-run per iteration and explicitly excluded from the clock.
    let mut epoch = 1u64;
    group.bench_function(BenchmarkId::new("revalidate_drop_pass", NODES), |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                rt.block_on(async {
                    for addr in &addrs {
                        armed.load(*addr).await.expect("load").expect("mapped");
                    }
                });
                epoch += 1;
                let ep = RootEpoch::synthetic(epoch, 0, &[]);
                let t0 = std::time::Instant::now();
                let dropped = black_box(armed.revalidate(black_box(&ep))).dropped;
                total += t0.elapsed();
                assert_eq!(dropped, NODES, "the pass must sweep a populated cache");
            }
            total
        });
    });

    group.finish();
}

/// PR M9 micro-benches (design-metadata-throughput §5.7 D7): the **hot
/// parent-key probe** — the create storm's dominant fold shape (a parent
/// inode `Put` accumulating one Δtime per create until compaction), in its
/// three read regimes:
///
/// - `overlay_head`: the chain is in the open delta — D7.a serves the
///   materialized folded head (zero decodes);
/// - `bset_resident_memo`: the chain froze into the node's bset log —
///   D7.b's snapshot memo serves repeat folds (zero decodes after the
///   first);
/// - `from_scratch_fold`: the same records folded through the raw §4.2
///   algebra every time — the pre-M9 per-read price, kept as the in-tip
///   comparator (and the shape `InodeDelta::decode` charged 7.9 % of
///   daemon CPU for in the baseline profile).
fn bench_kv_fold(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::record::{fold_newest_first, RecordKind};

    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("kv_fold");
    const DELTAS: u64 = 16;

    // One single-leaf volume; the hot parent key with a Put + Δtime chain.
    let file = NamedTempFile::new().expect("temp volume");
    let node_size = DEFAULT_NODE_SIZE;
    file.as_file()
        .set_len(64 * node_size as u64)
        .expect("size volume");
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout: NodeLayout::new(node_size).expect("layout"),
        heap_base: 0,
        budget_bytes: 64 * node_size as u64,
        writeback_delta_bytes: usize::MAX, // freezes are driven explicitly
    });
    let alloc = Arc::new(ExtentAllocator::format(64, 0, 4096));
    let mut ctx = SmoContext::new(alloc);
    let seq = Arc::new(AtomicU64::new(0));
    let key = inode_key(42);

    let parent = InodeValue {
        mode: 0o40755,
        uid: 1000,
        gid: 1000,
        nlink: 2,
        flags: 0,
        rdev: 0,
        size: 4096,
        atime: 1,
        mtime: 2,
        ctime: 3,
    };
    let (tree, overlay_leaf) = rt.block_on(async {
        let tree = KvTree::create(cache.clone(), &mut ctx, TREE_INODES, seq.clone())
            .await
            .expect("create");
        let leaf = tree.resolve_leaf(&key).await.expect("resolve");
        tree.apply_at(
            &leaf,
            &key,
            RecordKind::Put,
            bytes::Bytes::from(parent.encode()),
        )
        .await
        .expect("put");
        for t in 1..=DELTAS {
            tree.apply_at(
                &leaf,
                &key,
                RecordKind::Delta,
                bytes::Bytes::from(InodeDelta::times(t, t).encode()),
            )
            .await
            .expect("delta");
        }
        (tree, leaf)
    });

    // Regime 1: open-delta chain — D7.a overlay head.
    let overlay_snap = overlay_leaf.snapshot();
    group.bench_function("hot_parent_key_probe_overlay_head", |b| {
        b.iter(|| black_box(overlay_snap.lookup(black_box(&key)).expect("lookup")));
    });

    // Regime 2: bset-resident chain — D7.b memo (steady-state repeat fold
    // on one held immutable snapshot, the stat-storm shape).
    let memo_snap = rt.block_on(async {
        let mut guard = overlay_leaf.lock().write().await;
        overlay_leaf
            .freeze_locked(&mut guard, &cache.config().layout)
            .expect("freeze");
        drop(guard);
        assert!(cache.append_frozen(&overlay_leaf).await.expect("append"));
        overlay_leaf.snapshot()
    });
    group.bench_function("hot_parent_key_probe_bset_resident_memo", |b| {
        b.iter(|| black_box(memo_snap.lookup(black_box(&key)).expect("lookup")));
    });
    drop(tree);

    // Regime 3: the raw from-scratch algebra over the same chain — what
    // every read paid before D7.
    let mut records: Vec<Record> = vec![Record::put(key.to_vec(), 1, parent.encode())];
    for t in 1..=DELTAS {
        records.push(Record::delta(key.to_vec(), 1 + t, &InodeDelta::times(t, t)));
    }
    records.reverse(); // newest-first, the fold input contract
    group.bench_function("hot_parent_key_probe_from_scratch_fold", |b| {
        b.iter(|| {
            let folded = fold_newest_first(black_box(&records).iter().map(|r| r.record_ref()))
                .expect("fold");
            black_box(folded.live_value().map(<[u8]>::len))
        });
    });

    group.finish();
}

/// Microbench program (2026-08-04,
/// `.benchmarks/2026-08-04-microbench-program.md`): the journal-entry
/// codec + the lock-free reservation core — the per-commit device-plane
/// price of design-cow-kv-metadata §4.1/§4.4 (one tx = one checksummed
/// entry; the entry checksum is `xxh3_64` over the whole entry).
///
/// Shapes:
/// * `create` — the D4 entry-economy shape (design-metadata-throughput):
///   one create's whole-tx entry = dentry Put + inode Put + parent
///   Δtime (measured ≈ 1.0 entries/op — this is what one op encodes).
/// * `publish_batch64` — the write-commit-economy lever-B group
///   (`.benchmarks/2026-07-30-write-commit-economy.md`): 64 delta-class
///   layout saves drained into ONE multi-ino entry
///   (`SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX` derives from the conveyor's
///   batch default 64); ~128 B xattr-delta values.
/// * `journal_core` — the §4.4 pt 5 wait-free admission/reservation
///   cycle on a production-geometry ring (8 MiB: 2,048 × 4,072 B data
///   pages, the `clamp(volume/64, 8 MiB, 32 MiB)` floor).
fn bench_kv_journal(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::journal::{
        checkpoint_reserve_bytes, decode_entry_payload, encode_entry_payload, entry_len_for,
        JOURNAL_PAGE_DATA_LEN,
    };
    use squeezefs::meta_backend::kv::journal_core::{AdmissionClass, CoreGeometry, JournalCore};
    use squeezefs::meta_backend::kv::record::{
        xattr_key, xattr_name_hash56, DentryValue, InodeDelta, XattrValue, TREE_DENTRIES,
        TREE_XATTRS,
    };

    let mut group = c.benchmark_group("kv_journal");
    const HASH_SEED: u64 = 0x5EED_F00D;

    // The create-shape whole-tx entry: dentry Put + inode Put + parent Δtime.
    let create_records: Vec<(u8, Record)> = vec![
        (
            TREE_DENTRIES,
            Record::put(
                dentry_key(1, dentry_name_hash54(b"file_000042", HASH_SEED), 0).to_vec(),
                101,
                DentryValue::encode_parts(42, 8, b"file_000042").expect("dentry encode"),
            ),
        ),
        (
            TREE_INODES,
            Record::put(
                inode_key(42).to_vec(),
                101,
                InodeValue {
                    mode: 0o100644,
                    nlink: 1,
                    ..InodeValue::default()
                }
                .encode(),
            ),
        ),
        (
            TREE_INODES,
            Record::delta(inode_key(1).to_vec(), 101, &InodeDelta::times(7, 7)),
        ),
    ];
    group.bench_function("entry_encode_checksum_create", |b| {
        b.iter(|| {
            let len = entry_len_for(black_box(&create_records)).expect("fits");
            let payload = encode_entry_payload(black_box(&create_records));
            let sum = xxhash_rust::xxh3::xxh3_64(&payload);
            black_box((len, payload.len(), sum))
        });
    });

    // The lever-B publish group: 64 per-ino layout xattr deltas in ONE entry.
    let delta_value = vec![0x4C; 128]; // ~128 B per-ino LayoutDelta image
    let publish_records: Vec<(u8, Record)> = (0..64u64)
        .map(|i| {
            (
                TREE_XATTRS,
                Record::put(
                    xattr_key(1000 + i, xattr_name_hash56(b"layout", HASH_SEED), 0).to_vec(),
                    2000 + i,
                    XattrValue::encode_parts(b"layout", &delta_value).expect("xattr encode"),
                ),
            )
        })
        .collect();
    let publish_image = encode_entry_payload(&publish_records);
    group.throughput(criterion::Throughput::Bytes(publish_image.len() as u64));
    group.bench_function("entry_encode_checksum_publish_batch64", |b| {
        b.iter(|| {
            let payload = encode_entry_payload(black_box(&publish_records));
            black_box(xxhash_rust::xxh3::xxh3_64(&payload))
        });
    });
    group.bench_function("entry_decode_publish_batch64", |b| {
        b.iter(|| black_box(decode_entry_payload(black_box(&publish_image)).expect("decode")));
    });

    // The wait-free reservation core: admit → reserve → (instant
    // durability) watermark advance — the commit path's §4.4 pt 5 cycle;
    // and admit → release — the failed-before-reservation give-back.
    group.throughput(criterion::Throughput::Elements(1));
    let ring_len = 2048 * JOURNAL_PAGE_DATA_LEN;
    let geo = CoreGeometry {
        page_data_len: JOURNAL_PAGE_DATA_LEN,
        pages: 2048,
        reserve_bytes: checkpoint_reserve_bytes(ring_len),
    };
    let core = JournalCore::new(geo, 0, 0);
    let entry_len = entry_len_for(&create_records).expect("fits");
    group.bench_function("core_admit_reserve_advance", |b| {
        b.iter(|| {
            let adm = core
                .try_admit(entry_len, AdmissionClass::User)
                .expect("ring has space");
            let res = core.reserve(adm);
            core.advance_reusable_upto(res.end());
            black_box(res.seq())
        });
    });
    group.bench_function("core_admit_release", |b| {
        b.iter(|| {
            let adm = core
                .try_admit(entry_len, AdmissionClass::User)
                .expect("ring has space");
            core.release(adm);
        });
    });

    group.finish();
}

/// VAL-2 (pre-RC engineering spec §3): the xattr-name ALLOWLIST screen,
/// which every `getxattr`/`setxattr`/`removexattr` pays TWICE — once at
/// the FUSE boundary and once at this backend's `Metadata` entry points —
/// and once per listed name in `listxattr`.
///
/// Shapes are the field's actual names, not synthetic strings:
/// * `allow_user` — `user.mime_type`: the ordinary user xattr; matches
///   `user.` then walks the 10-byte `squeezefs.` non-match. The common
///   allow path.
/// * `allow_security_capability` — `security.capability`: the killpriv
///   name the kernel probes per write(2) on pre-`HANDLE_KILLPRIV_V2`
///   kernels (`.benchmarks/2026-07-28-fuse-killpriv-v2.md`) — the
///   highest-frequency screened name on such a fleet.
/// * `refuse_layout` / `refuse_writer_claim` — the internal records the
///   screen exists for (`layout` = the per-inode block map + wrapped
///   data-key material, `writer_claim` = the D0 guard): the SHORT refuse
///   path, no prefix match at all.
/// * `refuse_user_squeezefs` — `user.squeezefs.format_config`: the
///   worst-case classification (matches `user.`, then must walk the full
///   `squeezefs.` prefix to refuse).
/// * `refuse_client_record` — `client:{uuid}`: the live-client
///   registration shape (`fuse_client.rs` heartbeat).
/// * `list_page_32` — one `listxattr` reply's worth of names (31 user
///   names beside one internal record on ino 1), the per-name filter
///   cost the listing pays.
fn bench_xattr_name_screen(c: &mut Criterion) {
    let mut group = c.benchmark_group("kv_xattr_screen");
    group.throughput(criterion::Throughput::Elements(1));

    for (label, name) in [
        ("allow_user", "user.mime_type"),
        ("allow_security_capability", "security.capability"),
        ("refuse_layout", "layout"),
        ("refuse_writer_claim", "writer_claim"),
        ("refuse_user_squeezefs", "user.squeezefs.format_config"),
        (
            "refuse_client_record",
            "client:6f1c2b3a-9d4e-4a71-8c0f-2b5e7d9a1c33",
        ),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| black_box(xattr_name_allowed(black_box(name))))
        });
    }

    // One listing page: 31 permitted names + the one internal record
    // that must be filtered out of it.
    let page: Vec<String> = (0..31)
        .map(|i| format!("user.attr_{i:02}"))
        .chain(std::iter::once("layout".to_string()))
        .collect();
    group.throughput(criterion::Throughput::Elements(page.len() as u64));
    group.bench_function("list_page_32", |b| {
        b.iter(|| {
            let kept = page.iter().filter(|n| xattr_name_allowed(n)).count();
            black_box(kept)
        })
    });

    group.finish();
}

/// **DUR-5 · the superblock encode/verify cycle.** The redundant-copy
/// design put sector 0 on a commit-adjacent path: `layout_deltas_ready()`
/// stamps `KV_LAYOUT_DELTAS` during ordinary write traffic, and every
/// stamp now costs one extra encode plus the classify passes that resolve
/// primary vs backup. This group prices the CPU half of that cycle (the
/// device half is two 4 KiB writes + two barriers, measured by the
/// durability rigs, not here).
///
/// Field shape: the geometry `SuperblockV3::plan` produces for the
/// shipped default metadata volume — 256 KiB nodes on a 64 GiB volume,
/// the `dev_substrate.sh` mds namespace size.
fn bench_superblock_cycle(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::superblock::{
        backup_offset, classify_sector0, sector_generation, SuperblockV3,
    };

    const VOL_LEN: u64 = 64 * 1024 * 1024 * 1024;
    let sb = SuperblockV3::plan(VOL_LEN, DEFAULT_NODE_SIZE, None, [0x5A; 16], 0x5EED_F00D)
        .expect("plan the shipped default geometry");
    let img = sb
        .encode_sector_at_generation(7)
        .expect("encode the sector");

    let mut group = c.benchmark_group("kv_superblock");
    group.throughput(criterion::Throughput::Bytes(img.len() as u64));

    // The write half: geometry validation + field pack + whole-sector
    // xxh3. Paid TWICE per stamp (primary + redundant copy).
    group.bench_function("encode_sector", |b| {
        b.iter(|| {
            black_box(
                black_box(&sb)
                    .encode_sector_at_generation(black_box(7))
                    .expect("encode"),
            )
        })
    });

    // The read half: magic/version gate + whole-sector checksum verify +
    // bounds-checked geometry + the feature gate. Paid once per mount on
    // the primary, twice when the primary is torn.
    group.bench_function("classify_sector0", |b| {
        b.iter(|| black_box(classify_sector0(black_box(&img)).expect("classify")))
    });

    // Slot arbitration: the generation read that decides newest-valid-wins.
    group.bench_function("sector_generation", |b| {
        b.iter(|| black_box(sector_generation(black_box(&img))))
    });

    // The backup-slot derivation every write and every fallback performs.
    group.bench_function("backup_offset", |b| {
        b.iter(|| black_box(backup_offset(black_box(VOL_LEN))))
    });

    group.finish();
}

/// **The single-appender structures' hot paths** — the three §6.2
/// assumptions the multi-writer append partitioning breaks (pre-RC
/// engineering spec §6.2 items 2/3/4, ruling D9): the journal ring's
/// per-page header stamp, the A/B extent bitmap's claim/park/release
/// protocol, and the root ledger's slot encode + round-robin placement.
///
/// This group exists to price **solo mode** — the shipped
/// single-appender path — before and after the partitioned forms land:
/// spec §6.9's S4 gate ("solo mode is indistinguishable from today")
/// arriving early for the format layer. Evidence note:
/// `.benchmarks/2026-08-05-mw-partitioned-append.md`.
///
/// FIELD shapes (never toys):
/// * allocator — a 64 GiB metadata volume at the shipped 256 KiB extent
///   (the `dev_substrate.sh` mds namespace size, the same shape
///   `kv_superblock` prices): 262,144 extents, 9 bitmap pages, the §4.7
///   `max(8, 2 %)` compaction reserve, the shipped
///   `PENDING_FREE_CAP` FIFO. `claim_park_release` is the whole §4.7
///   per-extent protocol an SMO pays (claim the successor, park the
///   predecessor gated on its free-record seq, release it when the
///   durable tail covers it).
/// * ledger — the record `checkpoint_cycle` actually writes: three tree
///   roots + a live membership stamp (one stride run, one guest cursor
///   — the shape a slot-mapped volume carries).
fn bench_append_partition(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::alloc_ext::compaction_reserve_extents;
    use squeezefs::meta_backend::kv::backend::PENDING_FREE_CAP;
    use squeezefs::meta_backend::kv::checkpoint::{LedgerRecord, MembershipStamp, TreeRoot};
    use squeezefs::meta_backend::kv::record::{TREE_DENTRIES, TREE_XATTRS};
    use squeezefs::meta_backend::kv::slot_set::{SlotRun, SlotSet};

    const VOL_LEN: u64 = 64 * 1024 * 1024 * 1024;
    const EXTENT_LEN: u64 = 256 * 1024;
    let total_extents = VOL_LEN / EXTENT_LEN;

    let mut group = c.benchmark_group("kv_append_partition");
    group.throughput(criterion::Throughput::Elements(1));

    // ---- The A/B extent bitmap (§6.2 item 3): one whole-volume
    // single-appender bitmap + one `advance_durable` tail today.
    let alloc = ExtentAllocator::format(
        total_extents,
        compaction_reserve_extents(total_extents),
        PENDING_FREE_CAP,
    );
    group.bench_function("alloc_claim_release", |b| {
        b.iter(|| {
            let e = alloc.claim_user().expect("heap has space");
            alloc.release_unpublished(black_box(e));
        })
    });
    let mut gate = 1u64;
    group.bench_function("alloc_claim_park_release", |b| {
        b.iter(|| {
            let e = alloc.claim_internal().expect("heap has space");
            gate += 1;
            alloc.free_pending(black_box(e), gate).expect("FIFO room");
            black_box(alloc.advance_durable(gate));
        })
    });

    // ---- The A/B root ledger (§6.2 item 4): 32 round-robin slots,
    // `slot = seq % 32`, newest-valid-wins.
    let stamp = MembershipStamp {
        set_uuid: [0x5A; 16],
        set_epoch: 3,
        member_position: 0,
        member_count: 2,
        routing_width: 1 << 16,
        slots_hosted: SlotSet::from_runs(vec![SlotRun {
            start: 0,
            stride: 2,
            count: 32_768,
        }])
        .expect("one stride run"),
        native_slot: Some(0),
        slot_cursors: vec![(0, 4096)],
    };
    let rec = LedgerRecord {
        seq: 4_242,
        tree_roots: vec![
            TreeRoot {
                tree_id: TREE_INODES,
                node_addr: 0x40_0000,
                node_seq: 7_001,
            },
            TreeRoot {
                tree_id: TREE_DENTRIES,
                node_addr: 0x80_0000,
                node_seq: 7_002,
            },
            TreeRoot {
                tree_id: TREE_XATTRS,
                node_addr: 0xC0_0000,
                node_seq: 7_003,
            },
        ],
        journal_tail_seq: 9_876_543,
        next_ino: 1_000_000,
        alloc_bitmap_generation: 4_242,
        node_seq_watermark: 7_100,
        membership_stamp: Some(stamp),
        // The solo (un-stamped) shape — the shipped record. The
        // partitioned shape is priced separately below.
        append_partition: None,
    };
    group.bench_function("ledger_encode_slot", |b| {
        b.iter(|| black_box(black_box(&rec).encode_slot().expect("fits a slot")))
    });
    let image = rec.encode_slot().expect("fits a slot");
    group.bench_function("ledger_decode_slot", |b| {
        b.iter(|| black_box(LedgerRecord::decode_slot(black_box(&image)).expect("decode")))
    });
    group.bench_function("ledger_slot_index", |b| {
        b.iter(|| black_box(black_box(&rec).slot_index()))
    });
}

/// **Ino minting** — the create path's allocation cursor (pre-RC
/// engineering spec §6.2 **item 5**: `next_ino` is a per-mount atomic over
/// a shared namespace, so two writers mint duplicate inos, which alias
/// files immediately and — because the daemon's IPC binding table rests on
/// the monotonic never-reused ino law — alias fd bindings too).
///
/// This group prices the **solo** forms FIRST: they are the numbers the
/// per-writer lane partitioning must not move (ruling D9 keeps the format
/// un-stamped, so solo IS the shipped path).
///
/// **No numbers yet — ruling D11** (benches and brackets are deferred until
/// the DLM admits N readers and writers); the coverage must exist, so the
/// claim it will be measured against is stated instead:
///
/// * **prediction** — `mint_native` stays a bare `fetch_add` and holds its
///   pre-item-5 value; `mint_guest` holds its VL5b value; and
///   `live_inodes_64_cursors` stays linear in the cursor count with no
///   per-lane term (lane cursors are empty on an un-stamped volume, which
///   is every volume — the read side is one relaxed latch load);
/// * **falsification** — any of the three moving past its group threshold
///   with the format un-stamped, which would mean the lane machinery is
///   being consulted on the solo path (it must not be) rather than gated
///   behind the `lanes_live` latch.
///
/// FIELD shapes (never toys):
/// * `mint_native` — one `allocate_ino()`: the §4.8 `fetch_add` every
///   create pays on the volume's legacy keyspace.
/// * `mint_guest` — one `allocate_guest_ino(slot)`: what a create pays on
///   any of the other ~63 slots of the dynamic-routing `MINT_SPREAD` rotor
///   (`docs/design-dynamic-meta-routing.md` §5.4) — i.e. ~63 of every 64
///   real creates.
/// * `live_inodes_64_cursors` — the POSIX-1 `statfs` `f_ffree` derivation
///   over a fully spread volume's cursor set (`MINT_SPREAD` = 64 live
///   guest cursors plus the native watermark), which every `df -i` and
///   every capacity-gating tool pays.
fn bench_ino_cursors(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(256 * 1024 * 1024).unwrap();
    let backend = rt.block_on(async {
        format_v3(
            file.path(),
            256 * 1024 * 1024,
            &FormatV3Options {
                node_size: DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: false,
                full_wipe: false,
                format_config_xattr: None,
            },
        )
        .await
        .expect("format v3");
        KvMetaBackend::open(file.path()).await.expect("mount v3")
    });

    let mut group = c.benchmark_group("kv_ino_cursors");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("mint_native", |b| {
        b.iter(|| black_box(backend.allocate_ino()))
    });
    group.bench_function("mint_guest", |b| {
        b.iter(|| black_box(backend.allocate_guest_ino(black_box(7)).expect("mint")))
    });

    // The spread volume's cursor set: MINT_SPREAD slots ever minted into.
    for slot in 0..squeezefs::meta_backend::MINT_SPREAD as u16 {
        backend.allocate_guest_ino(slot).expect("mint");
    }
    group.bench_function("live_inodes_64_cursors", |b| {
        b.iter(|| black_box(backend.live_inodes()))
    });

    group.finish();
    rt.block_on(async { backend.shutdown().await.expect("shutdown") });
}

/// **Durable block-reference accounting** (pre-RC engineering spec §6.2
/// item 1, incompat bit 9) — the publish path's added cost, priced.
///
/// Input shapes are FIELD-derived, not toys:
///
/// * **`delta_apply_*`** — the accounting work one block publish adds:
///   translate the merge's `(map index, block key, taken?)` changes into
///   records and stage them. Widths 1 (a streaming append: one block
///   gained) and 2 (an overwrite: one gained, one displaced) are the two
///   shapes the field's rewrite row runs (`.benchmarks/2026-08-01-
///   rewrite-publish-drain.md` §3: ~2,600 block-publishes/s, publish
///   coalescing at 1–3 blocks per batch), and 64 is the
///   `SQUEEZEFS_PUBLISH_COALESCE_MAX` default window — the worst case a
///   single aggregated transaction carries.
/// * **`entry_encode_publish_with_refs`** — the journal-byte cost of the
///   same records inside the entry the publish was already writing,
///   measured against the accounting-free entry so the delta is the
///   answer (no second entry exists to measure: the records ride the
///   layout tx by construction).
/// * **`recovery_scan_decode`** — the mount path that REPLACES the
///   inode-tree walk: decode + validate every scanned reference. 4,096
///   references ≈ a 16 GiB file set at the shipped 4 MiB block.
/// * **`census_fold`/`census_compare`** — the oracle's arithmetic: fold
///   the durable population into a per-block census and diff it against
///   the derived one (the fsck C8 / `SQUEEZEFS_BLOCK_REFS_VERIFY` pass,
///   sized at the same 4,096 references, half of them shared by a clone).
fn bench_block_refs(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::block_refs::{
        block_ref_key, decode_block_ref_key, decode_block_ref_value, volume_range, volume_tag,
        BlockRef, BlockRefOp,
    };
    use squeezefs::meta_backend::kv::journal::{encode_entry_payload, entry_len_for};
    use squeezefs::meta_backend::kv::record::{
        xattr_key, xattr_name_hash56, XattrValue, TREE_BLOCK_REFS, TREE_XATTRS,
    };

    let mut group = c.benchmark_group("block_refs");
    const HASH_SEED: u64 = 0x5EED_F00D;
    let vol_tag = volume_tag("vol-00000000000000a1");

    // --- The staging cost, per publish width. ---------------------------
    for width in [1usize, 2, 64] {
        let ops: Vec<BlockRefOp> = (0..width)
            .map(|i| {
                let r = BlockRef {
                    vol_tag,
                    block_idx: 4096 + i as u64,
                    owner_ino: 900_001,
                    block_index: i as u32,
                };
                if i % 2 == 0 {
                    BlockRefOp::taken(r)
                } else {
                    BlockRefOp::released(r)
                }
            })
            .collect();
        group.throughput(criterion::Throughput::Elements(width as u64));
        group.bench_with_input(BenchmarkId::new("delta_apply", width), &ops, |b, ops| {
            b.iter(|| {
                // Exactly what `KvTx::stage_block_refs` does: one key
                // + (for a take) one value per changed reference.
                let mut staged: Vec<(u8, Record)> = Vec::with_capacity(ops.len());
                for op in ops {
                    let key = op.reference.key().to_vec();
                    staged.push((
                        TREE_BLOCK_REFS,
                        if op.take {
                            Record::put(key, 0, op.reference.value().to_vec())
                        } else {
                            Record::delete(key, 0)
                        },
                    ));
                }
                black_box(staged)
            });
        });
    }

    // --- The journal-byte cost, in situ. --------------------------------
    // A 64-ino publish group's layout deltas, with and without the two
    // accounting records each publish adds (take + release = the overwrite
    // shape).
    let delta_value = vec![0x4C; 128];
    let mut plain: Vec<(u8, Record)> = (0..64u64)
        .map(|i| {
            (
                TREE_XATTRS,
                Record::put(
                    xattr_key(1000 + i, xattr_name_hash56(b"layout", HASH_SEED), 0).to_vec(),
                    2000 + i,
                    XattrValue::encode_parts(b"layout", &delta_value).expect("xattr encode"),
                ),
            )
        })
        .collect();
    let mut accounted = plain.clone();
    for i in 0..64u64 {
        let gained = BlockRef {
            vol_tag,
            block_idx: 8192 + i,
            owner_ino: 1000 + i,
            block_index: 0,
        };
        let lost = BlockRef {
            block_idx: 4096 + i,
            ..gained
        };
        accounted.push((
            TREE_BLOCK_REFS,
            Record::put(gained.key().to_vec(), 3000 + i, gained.value().to_vec()),
        ));
        accounted.push((
            TREE_BLOCK_REFS,
            Record::delete(lost.key().to_vec(), 3100 + i),
        ));
    }
    let plain_len = entry_len_for(&plain).expect("fits");
    let accounted_len = entry_len_for(&accounted).expect("fits");
    // Printed once so the evidence note can cite the byte delta directly.
    println!(
        "block_refs: publish-batch-64 entry {plain_len} B without accounting, \
         {accounted_len} B with it (+{} B, +{:.2} %)",
        accounted_len - plain_len,
        100.0 * (accounted_len - plain_len) as f64 / plain_len as f64
    );
    group.throughput(criterion::Throughput::Bytes(accounted_len));
    group.bench_function("entry_encode_publish_batch64_plain", |b| {
        b.iter(|| {
            let payload = encode_entry_payload(black_box(&plain));
            black_box(xxhash_rust::xxh3::xxh3_64(&payload))
        });
    });
    group.bench_function("entry_encode_publish_batch64_with_refs", |b| {
        b.iter(|| {
            let payload = encode_entry_payload(black_box(&accounted));
            black_box(xxhash_rust::xxh3::xxh3_64(&payload))
        });
    });
    plain.clear();
    accounted.clear();

    // --- The recovery + verify pass. ------------------------------------
    const REFS: u64 = 4096;
    // Half the blocks carry a second (clone) reference — the shared shape
    // the durable ledger exists to preserve.
    let scanned: Vec<(Vec<u8>, Vec<u8>)> = (0..REFS)
        .map(|i| {
            let r = BlockRef {
                vol_tag,
                block_idx: i / 2,
                owner_ino: 500_000 + (i % 2),
                block_index: (i / 2) as u32,
            };
            (r.key().to_vec(), r.value().to_vec())
        })
        .collect();
    group.throughput(criterion::Throughput::Elements(REFS));
    group.bench_function("recovery_scan_decode", |b| {
        b.iter(|| {
            // The `block_ref_scan` inner loop: decode + validate both
            // halves of every record (a malformed accounting record is
            // loud corruption, never a silently skipped reference).
            let mut out: Vec<u64> = Vec::with_capacity(scanned.len());
            for (k, v) in black_box(&scanned) {
                let r = decode_block_ref_key(k).expect("key");
                decode_block_ref_value(v).expect("value");
                out.push(r.block_idx);
            }
            out.sort_unstable();
            black_box(out)
        });
    });

    let refs: Vec<BlockRef> = scanned
        .iter()
        .map(|(k, _)| decode_block_ref_key(k).expect("key"))
        .collect();
    group.bench_function("census_fold", |b| {
        b.iter(|| {
            let mut census: std::collections::BTreeMap<u64, u32> =
                std::collections::BTreeMap::new();
            for r in black_box(&refs) {
                *census.entry(r.block_idx).or_insert(0) += 1;
            }
            black_box(census)
        });
    });

    let durable: std::collections::BTreeMap<u64, u32> =
        refs.iter()
            .fold(std::collections::BTreeMap::new(), |mut m, r| {
                *m.entry(r.block_idx).or_insert(0) += 1;
                m
            });
    let derived = durable.clone();
    group.bench_function("census_compare", |b| {
        b.iter(|| {
            let (durable, derived) = (black_box(&durable), black_box(&derived));
            let mut blocks: Vec<u64> = durable.keys().chain(derived.keys()).copied().collect();
            blocks.sort_unstable();
            blocks.dedup();
            let mut drift = 0usize;
            for idx in blocks {
                if durable.get(&idx).copied().unwrap_or(0)
                    != derived.get(&idx).copied().unwrap_or(0)
                {
                    drift += 1;
                }
            }
            black_box(drift)
        });
    });

    // The scan's range bounds (one allocation pair per volume scan).
    group.throughput(criterion::Throughput::Elements(1));
    group.bench_function("scan_range_bounds", |b| {
        b.iter(|| black_box(volume_range(black_box(vol_tag))));
    });
    group.bench_function("key_build", |b| {
        b.iter(|| black_box(block_ref_key(black_box(vol_tag), 4096, 900_001, 7)));
    });
    group.finish();
}

/// **DLM S3.5 — the cross-volume intent-record path** (design-cow-kv-
/// metadata §4.11; DUR-7). The transaction's per-op CPU cost that is NOT
/// already a journal entry is exactly this: mint the id, encode the plan,
/// checksum it, and (at recovery) decode + verify it. Everything else the
/// protocol adds is device work — one extra small entry and up to two
/// coalesced barriers — which no microbench can honestly price.
///
/// Input shapes are FIELD-derived, not toys:
///
/// * **`intent_encode_unlink`** — the 2-step plan every cross-volume
///   `unlink`/`link` builds, at the field's name length. Cross-volume
///   unlink is the HOT shape: since the 2026-07-30 meta-plane
///   distribution fix, regular-file inodes stripe across the set, so a
///   `rm -rf` storm's unlinks are mostly cross-volume. The rate to beat
///   is the D4 journal-economy row's unlink throughput
///   (`.benchmarks/2026-07-15-metadata-throughput-closing.md`).
/// * **`intent_encode_rename5`** — the 5-step cross-parent directory
///   rename (both parents' `nlink`, source removal, destination insert,
///   moved-inode ctime), and **`intent_encode_rename9`** the worst plan
///   the converted ops build (destination replacement + `RENAME_WHITEOUT`).
/// * **`intent_decode_verify_*`** — the mount-recovery path, per open
///   intent. A healthy set decodes ZERO of these; the crash path decodes
///   one per interrupted transaction.
/// * **`intent_key`** — the probe-free key derivation that is the reason
///   the machinery needs no new lock (an ordinary xattr write would pay a
///   collision-chain scan here).
///
/// **Prediction:** encode + checksum ≲ 300 ns and key derivation ≲ 10 ns
/// for every shape, i.e. under 1 % of the ~30–60 µs a cross-volume
/// namespace op costs at the D4 rates — so the protocol's cost is its
/// device work, not its record.
///
/// **Falsification:** any row above 1 µs, or `intent_key` above 50 ns,
/// falsifies "the record is free" and moves the plan encoding off the
/// critical path (encode once per plan SHAPE, or shrink the wire).
///
/// NOT RUN (ruling D11: no benches until the DLM can serve N readers and
/// writers). Numbers get measured on the sanctioned venue
/// (`tests/run_bench_baseline.sh`), never here.
/// **fsck class C9 — the referenced-ino set** (`src/fsck.rs`
/// [`squeezefs::fsck::InoBitmap`]): the price of asking "which inodes are
/// NAMED" once, instead of "who names me" per inode (POSIX-4's unindexed
/// reverse-dentry scan, which is O(total dentries) PER INODE).
///
/// **Field-derived shape.** The design cap is >= 100 M inodes
/// (`docs/design-cow-kv-metadata.md` §4.2 caps) and minting rotates over
/// `MINT_SPREAD = 64` slots per metadata volume
/// (`docs/design-dynamic-meta-routing.md` §5.4), so a volume's inos are
/// spread across 64 dense keyspaces — the shape here is **1 M dentry
/// targets over 64 slots** at the DERIVED routing width (65536), scaled
/// down from the cap only because a Criterion sample must fit a
/// measurement window; the per-reference cost is what extrapolates.
/// Access order is deliberately RANDOM: dentry keys are
/// `(parent, hash54(name))`-ordered, so a sequential dentry walk delivers
/// child inos in hash order — i.e. random word touches across each slot's
/// bit vector. Sequential-order marking is included only as the
/// cache-friendly bound, never as the claim.
///
/// The difference pass is priced at the same population with 8 unnamed
/// inodes (the damaged-volume shape: a handful of orphans in a healthy
/// tree), because that is the case that must be nearly free — a healthy
/// volume's whole C9 verdict is one word-parallel `AND NOT` scan.
///
/// **Prediction** (dev box, release, single thread — RECORD, do not
/// assert; the fsck-scan anchor is `tests/fsck_tests.rs`'s measured
/// 117,518 inodes/s C1-C6 scan and 1,142,885 inodes/s census walk):
///
/// * `mark/random` <= ~40 ns/ino (one per-slot map lookup + one word
///   read-modify-write; a 1 M-ino slot set is ~125 KB of bits, so random
///   touches miss L2 and hit L3), i.e. >= 25 M marks/s. At the 100 M cap
///   that is <= ~4 s of CPU for the whole referenced pass, against the
///   ~14 min the same volume's inode walk already costs at the measured
///   scan rate — under 1 %.
/// * `mark/sequential` <= ~5 ns/ino (same work, cache-resident).
/// * `difference/8_unnamed` <= ~1 ns per LIVE ino (population/64 word
///   AND-NOTs plus 8 emits) — the healthy-volume verdict is a linear
///   scan, not per-inode work.
///
/// **Falsification.** If `mark/random` exceeds ~100 ns/ino, or
/// `difference` exceeds ~4 ns per live ino, the per-slot `HashMap` lookup
/// (not the bit math) dominates and the representation must change — a
/// slot-indexed `Vec` keyed by slot id, or a single flat bit vector per
/// volume — and `src/fsck.rs`'s cost claim ("a healthy volume pays only
/// the bitmap scan") must be restated with the measured numbers. If
/// `difference` instead scales with the number of MARKED inos rather than
/// the population, the word-parallel AND-NOT regressed to a per-bit
/// `contains` (the shape this group exists to prevent).
fn bench_fsck_c9_refset(c: &mut Criterion) {
    use squeezefs::fsck::InoBitmap;

    const WIDTH: u64 = 65536;
    const SLOTS: u64 = 64;
    const POPULATION: u64 = 1_000_000;
    // The set's two bounds, as a mount supplies them: the raw-local ino
    // ceiling (nothing can exist at or above the volume's watermark, so a
    // dentry value is never an allocation authority) and the derived byte
    // budget. Sized for this shape: `POPULATION / SLOTS` raw locals per
    // slot, and bits for the whole population.
    let ceiling = 2 + POPULATION / SLOTS + 1;
    let budget = 16 * 1024 * 1024u64;

    // Global inos as the mint rotor produces them: raw local `r` in slot
    // `s` encodes to `(r - 2) * W + s + 2`.
    let ino_of = |raw: u64, slot: u64| (raw - 2) * WIDTH + slot + 2;
    let sequential: Vec<u64> = (0..POPULATION)
        .map(|i| ino_of(2 + i / SLOTS, i % SLOTS))
        .collect();
    // Hash order == random order for this purpose (xxh3 of the name is
    // what orders the dentry tree); a fixed permutation keeps the bench
    // deterministic.
    let random: Vec<u64> = {
        let mut v = sequential.clone();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for i in (1..v.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
        v
    };

    let mut group = c.benchmark_group("fsck_c9_refset");
    group.throughput(criterion::Throughput::Elements(POPULATION));
    for (name, order) in [("sequential", &sequential), ("random", &random)] {
        group.bench_with_input(BenchmarkId::new("mark", name), order, |b, inos| {
            b.iter(|| {
                let mut refs = InoBitmap::new(WIDTH, ceiling, budget);
                for &ino in inos.iter() {
                    refs.mark(black_box(ino));
                }
                black_box(refs.marked())
            });
        });
    }

    // The difference: a healthy tree with 8 unnamed inodes.
    let mut live = InoBitmap::new(WIDTH, ceiling, budget);
    for &ino in &sequential {
        live.mark(ino);
    }
    let mut referenced = InoBitmap::new(WIDTH, ceiling, budget);
    for (i, &ino) in sequential.iter().enumerate() {
        if i % 125_000 != 0 {
            referenced.mark(ino);
        }
    }
    group.bench_function("difference/8_unnamed", |b| {
        b.iter(|| {
            let mut found = 0u64;
            live.each_absent_from(&referenced, |ino| found += black_box(ino) & 1);
            black_box(found)
        });
    });
    group.finish();
}

/// **fsck class C10 — the price of the counting extension** on top of
/// C9's referenced-ino pass (`src/fsck.rs`, `RefPass`): C9 asks "is this
/// inode named", C10 asks "how many names", and the whole design claim is
/// that the second question is nearly free because
/// [`squeezefs::fsck::InoBitmap::mark`] already *returns* whether the bit
/// was newly set — so only a record whose ino was ALREADY named pays
/// anything.
///
/// **What this prices, honestly.** `RefPass` is private, so the rows below
/// MIRROR its loop body over the public [`squeezefs::fsck::InoBitmap`]:
/// mark, and on `false` one `contains` plus a `HashMap` probe/insert. That
/// makes this a bench of the REPRESENTATION, not of the function — if the
/// loop body changes, this bench must change with it, which is exactly the
/// falsification criterion's first clause.
///
/// **Field-derived shape.** Same population and access order as
/// `fsck_c9_refset` (1 M dentry targets over the `MINT_SPREAD = 64` slots
/// at the derived routing width 65536, marked in hash — i.e. random —
/// order, because dentry keys are `(parent, hash54(name))`-ordered), so
/// the rows are directly comparable to C9's. The hardlink fraction is the
/// variable: **0 %** is the field norm (hardlinks are the exception in
/// every real tree — the case the "costs nothing" claim rests on) and
/// **5 %** is a deliberately pessimistic ceiling for a tree that really
/// does use them (source checkouts, package trees, Maildir).
///
/// The reverse difference is priced at the same population with 8 damaged
/// entries — C10's DANGEROUS arms (`nlink == 0` with a live name, a name
/// whose inode does not exist) are exactly `referenced \ live`, so a
/// healthy volume's entire loss-direction verdict is one word-parallel
/// `AND NOT` scan in the other direction from C9's.
///
/// **Prediction** (dev box, release, single thread — RECORD, do not
/// assert; anchored on `fsck_c9_refset`'s own predicted basis of
/// `mark/random` ≤ ~40 ns/ino):
///
/// * `mark_with_counts/random/hardlinks_0pct` ≤ ~44 ns/ino — within ~10 %
///   of C9's `mark/random`, because every record takes the `first_name`
///   branch and nothing else runs.
/// * `mark_with_counts/random/hardlinks_5pct` ≤ ~60 ns/ino — 5 % of
///   records add one `contains` (a per-slot map lookup + one word read)
///   and one `HashMap` probe.
/// * `reverse_difference/8_damaged` ≤ ~1 ns per REFERENCED ino, matching
///   C9's difference row: the dangerous arms cost the scan, not per-inode
///   work.
///
/// **Falsification.** If `hardlinks_0pct` exceeds C9's `mark/random` by
/// more than ~15 %, the extension is NOT free on the field norm and the
/// second-name branch must leave the hot path (count only the inos the
/// census already flagged, in a second cheap pass). If `hardlinks_5pct`
/// exceeds ~150 ns/ino, the `HashMap` probe dominates and the multi-name
/// map must become a different structure (a sorted vector built from the
/// flagged set). If `reverse_difference` scales with the number of DAMAGED
/// inos rather than the population, the word-parallel `AND NOT` regressed
/// to a per-bit `contains` — the same regression C9's row exists to catch.
///
/// NOT RUN (ruling D11: no benches until the DLM can serve N readers and
/// writers). Numbers get measured on the sanctioned venue
/// (`tests/run_bench_baseline.sh`), never here.
fn bench_fsck_c10_name_counts(c: &mut Criterion) {
    use squeezefs::fsck::InoBitmap;
    use std::collections::HashMap;

    const WIDTH: u64 = 65536;
    const SLOTS: u64 = 64;
    const POPULATION: u64 = 1_000_000;
    let ceiling = 2 + POPULATION / SLOTS + 1;
    let budget = 16 * 1024 * 1024u64;
    let ino_of = |raw: u64, slot: u64| (raw - 2) * WIDTH + slot + 2;
    let inos: Vec<u64> = (0..POPULATION)
        .map(|i| ino_of(2 + i / SLOTS, i % SLOTS))
        .collect();
    // A fixed xorshift permutation: hash order == random order for this
    // purpose, and determinism keeps the bench comparable run to run.
    let shuffle = |v: &mut Vec<u64>| {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for i in (1..v.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    };

    // The dentry-record streams: every ino once, plus a second record for
    // the hardlinked fraction (the shape the pass actually walks).
    let mut group = c.benchmark_group("fsck_c10_name_counts");
    for pct in [0u64, 5] {
        let mut records: Vec<u64> = inos.clone();
        // The hardlinked fraction contributes a SECOND dentry record for
        // every `100/pct`-th ino; `0 %` contributes none.
        if let Some(stride) = (100u64).checked_div(pct) {
            records.extend(inos.iter().step_by(stride as usize));
        }
        shuffle(&mut records);
        let count = records.len() as u64;
        group.throughput(criterion::Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("mark_with_counts/random", format!("hardlinks_{pct}pct")),
            &records,
            |b, records| {
                b.iter(|| {
                    let mut refs = InoBitmap::new(WIDTH, ceiling, budget);
                    let mut multi: HashMap<u64, u32> = HashMap::new();
                    for &ino in records.iter() {
                        let first = refs.mark(black_box(ino));
                        if !first && refs.contains(ino) {
                            *multi.entry(ino).or_insert(1) += 1;
                        }
                    }
                    black_box((refs.marked(), multi.len()))
                });
            },
        );
    }

    // The dangerous arms: `referenced \ live` with 8 damaged entries.
    let mut referenced = InoBitmap::new(WIDTH, ceiling, budget);
    for &ino in &inos {
        referenced.mark(ino);
    }
    let mut live = InoBitmap::new(WIDTH, ceiling, budget);
    for (i, &ino) in inos.iter().enumerate() {
        // 8 named inos whose record is missing or `nlink == 0`.
        if i % 125_000 != 0 {
            live.mark(ino);
        }
    }
    group.throughput(criterion::Throughput::Elements(POPULATION));
    group.bench_function("reverse_difference/8_damaged", |b| {
        b.iter(|| {
            let mut found = 0u64;
            referenced.each_absent_from(&live, |ino| found += black_box(ino) & 1);
            black_box(found)
        });
    });
    group.finish();
}

fn bench_crossvol_tx(c: &mut Criterion) {
    use squeezefs::meta_backend::crossvol_tx::{intent_key, IntentRecord, XvOp, XvStep};

    let exclusive = 2u8; // RoutedParentUpdate::ExclusiveTimes
    let now = 1_760_000_000_000_000_000u64;
    let unlink = IntentRecord {
        tx_id: 0x0000_0007_dead_beef,
        op: XvOp::Unlink,
        steps: vec![
            XvStep::RemoveDentry {
                parent: 4_000_002,
                name: "checkpoint_00042.safetensors".to_string(),
                expect_child: 9_000_003,
                parent_update: 1,
            },
            XvStep::SetNlink {
                ino: 9_000_003,
                pre: 1,
                post: 0,
                ctime: Some(now),
            },
        ],
    };
    let rename5 = IntentRecord {
        tx_id: 0x0000_0007_dead_bef0,
        op: XvOp::Rename,
        steps: vec![
            XvStep::SetNlink {
                ino: 4_000_002,
                pre: 9,
                post: 8,
                ctime: None,
            },
            XvStep::SetNlink {
                ino: 4_000_003,
                pre: 3,
                post: 4,
                ctime: None,
            },
            XvStep::RemoveDentry {
                parent: 4_000_002,
                name: "epoch_0042".to_string(),
                expect_child: 9_000_007,
                parent_update: exclusive,
            },
            XvStep::InsertDentry {
                parent: 4_000_003,
                name: "epoch_0042".to_string(),
                child: 9_000_007,
                ft_bits: libc::S_IFDIR,
                parent_update: exclusive,
            },
            XvStep::TouchCtime {
                ino: 9_000_007,
                ctime: now,
            },
        ],
    };
    let mut rename9 = rename5.clone();
    rename9.steps.extend([
        XvStep::SetNlink {
            ino: 9_000_011,
            pre: 2,
            post: 0,
            ctime: Some(now),
        },
        XvStep::RemoveDentry {
            parent: 4_000_003,
            name: "epoch_0042".to_string(),
            expect_child: 9_000_011,
            parent_update: exclusive,
        },
        XvStep::MintInode {
            ino: 4_000_015,
            mode: libc::S_IFCHR,
            uid: 0,
            gid: 0,
            rdev: 0,
        },
        XvStep::InsertDentry {
            parent: 4_000_002,
            name: "epoch_0042".to_string(),
            child: 4_000_015,
            ft_bits: libc::S_IFCHR,
            parent_update: exclusive,
        },
    ]);

    let mut group = c.benchmark_group("crossvol_tx");
    for (label, rec) in [
        ("unlink", &unlink),
        ("rename5", &rename5),
        ("rename9", &rename9),
    ] {
        group.bench_function(format!("intent_encode_{label}"), |b| {
            b.iter(|| black_box(black_box(rec).encode().expect("encode")));
        });
        let image = rec.encode().expect("encode");
        group.bench_function(format!("intent_decode_verify_{label}"), |b| {
            b.iter(|| black_box(IntentRecord::decode(black_box(&image)).expect("decode")));
        });
    }
    group.bench_function("intent_key", |b| {
        b.iter(|| black_box(intent_key(black_box(0x0000_0007_dead_beef))));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_kv_meta_metadata,
    bench_readdir_parent,
    bench_kv_bset,
    bench_kv_tree,
    bench_kv_node_cache,
    bench_kv_fold,
    bench_kv_journal,
    bench_ino_cursors,
    bench_block_refs,
    bench_fsck_c9_refset,
    bench_fsck_c10_name_counts,
    bench_crossvol_tx,
    bench_xattr_name_screen,
    bench_superblock_cycle,
    bench_append_partition
);
criterion_main!(benches);
