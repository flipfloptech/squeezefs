use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator;
use squeezefs::meta_backend::kv::backend::{xattr_name_allowed, KvMetaBackend};
use squeezefs::meta_backend::kv::bset::{build_bset, lookup, merge, BsetView};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::node::{NodeLayout, DEFAULT_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    NodeCache, NodeCacheConfig, DEFAULT_WRITEBACK_DELTA_BYTES,
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

criterion_group!(
    benches,
    bench_kv_meta_metadata,
    bench_readdir_parent,
    bench_kv_bset,
    bench_kv_tree,
    bench_kv_fold,
    bench_kv_journal,
    bench_xattr_name_screen,
    bench_superblock_cycle
);
criterion_main!(benches);
