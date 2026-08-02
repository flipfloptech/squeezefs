use criterion::{black_box, criterion_group, criterion_main, Criterion};
use squeezefs::cache::lru::LruCache;
use squeezefs::fuse_client::StripeLocks;
use squeezefs::meta_backend::dlm::DlmLockManager;
use std::sync::Arc;
use tokio::runtime::Runtime;

fn bench_high_concurrency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("high_concurrency_contention");

    // 1. DlmLockManager lock contention
    let dlm = Arc::new(DlmLockManager::new());
    group.bench_function("dlm_lock_contention", |b| {
        b.to_async(&rt).iter(|| {
            let dlm = dlm.clone();
            async move {
                let futures = (0..10).map(|_| {
                    let dlm = dlm.clone();
                    async move {
                        let _guard = dlm.lock_inode_shared(42).await;
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    // 2. StripeLocks contention
    let active_inode_locks = Arc::new(StripeLocks::<tokio::sync::RwLock<()>, 4096>::new());
    group.bench_function("stripe_locks_contention", |b| {
        b.to_async(&rt).iter(|| {
            let active_inode_locks = active_inode_locks.clone();
            async move {
                let futures = (0..10).map(|_| {
                    let active_inode_locks = active_inode_locks.clone();
                    async move {
                        let lock = active_inode_locks.get_inode_lock(42);
                        let _guard = lock.read().await;
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    // 3. LRU Cache contention
    let cache = Arc::new(LruCache::with_capacity(1024 * 1024 * 10)); // 10MB
    let test_bytes = bytes::Bytes::from(vec![0u8; 4096]); // 4KB block
    group.bench_function("lru_cache_contention", |b| {
        b.to_async(&rt).iter(|| {
            let cache = cache.clone();
            let test_bytes = test_bytes.clone();
            async move {
                let futures = (0..10).map(|i| {
                    let cache = cache.clone();
                    let test_bytes = test_bytes.clone();
                    async move {
                        let key = format!("block_{}", i);
                        cache.put(&key, test_bytes);
                        let _ = cache.get(&key);
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    group.finish();
}

/// Cluster-DLM (local backend) hot-path costs: lease acquire/release cycles
/// and fencing-token reads. The typed binary `ObjectKey` (`Ino(u64)` fast
/// path) must keep these free of `format!`/parse allocations — regressions
/// show up here as step changes.
fn bench_cluster_dlm(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("cluster_dlm");

    let dlm = squeezefs::dlm::DlmClient::new().unwrap();

    // Uncontended acquire+release on rotating inode keys (the
    // get_or_acquire_lease shape).
    group.bench_function("acquire_release_uncontended_ino", |b| {
        let dlm = dlm.clone();
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i = i.wrapping_add(1);
            let path = format!("inode_{}", 800_000 + (i % 1024));
            let dlm = dlm.clone();
            async move {
                let lease = dlm
                    .acquire_lock(&path, None, std::time::Duration::from_secs(1))
                    .await
                    .expect("uncontended acquire");
                lease.release().await.expect("release");
            }
        });
    });

    // Fencing-token read via the path API (prefix parse, zero alloc) — the
    // per-save_metadata fencing check shape.
    let seeded = rt.block_on(async {
        dlm.acquire_lock("inode_800042", None, std::time::Duration::from_secs(1))
            .await
            .expect("seed acquire")
    });
    group.bench_function("get_fencing_token_path", |b| {
        b.iter(|| black_box(dlm.get_fencing_token(black_box("inode_800042"))));
    });
    group.bench_function("get_fencing_token_ino", |b| {
        b.iter(|| black_box(dlm.get_fencing_token_ino(black_box(800_042u64))));
    });
    drop(seeded);

    // S1 rows (feat/dlm-s0-s1) — before/after-comparable across the
    // FENCING_MAP → grant_seq swap. Field shapes (microbench law):
    // leases are per-open on DISTINCT inos (spec §6.2 — one lease per
    // ino per open-for-write episode; the RES-2 growth shape the S1 RSS
    // gate in tests/dlm_grant_seq_tests.rs measures), and the unheld
    // read is the post-release writeback-credential shape (FIND-M11-A:
    // flush units re-read the ino's current generation per attempt,
    // which can outlive the last close).

    // Token mint + lock-table churn across distinct objects (64 Ki
    // rotating window bounds the pre-S1 map footprint in-bench).
    group.bench_function("acquire_release_distinct_ino_walk", |b| {
        let dlm = dlm.clone();
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i = i.wrapping_add(1);
            let path = format!("inode_{}", 900_000_000 + (i % 65_536));
            let dlm = dlm.clone();
            async move {
                let lease = dlm
                    .acquire_lock(&path, None, std::time::Duration::from_secs(1))
                    .await
                    .expect("walk acquire");
                lease.release().await.expect("walk release");
            }
        });
    });

    // Unheld fencing read: released-object generation (stripe-floor arm
    // post-S1; generator-map hit pre-S1).
    rt.block_on(async {
        dlm.acquire_lock("inode_800777001", None, std::time::Duration::from_secs(1))
            .await
            .expect("unheld seed acquire")
            .release()
            .await
            .expect("unheld seed release");
    });
    group.bench_function("get_fencing_token_ino_unheld", |b| {
        b.iter(|| black_box(dlm.get_fencing_token_ino(black_box(800_777_001u64))));
    });

    // Contended handoff: 8 tasks fight over one key, each holding briefly.
    group.bench_function("acquire_release_contended_1key_8tasks", |b| {
        let dlm = dlm.clone();
        b.to_async(&rt).iter(|| {
            let dlm = dlm.clone();
            async move {
                let futures = (0..8).map(|_| {
                    let dlm = dlm.clone();
                    async move {
                        let lease = dlm
                            .acquire_lock("inode_800777", None, std::time::Duration::from_secs(5))
                            .await
                            .expect("contended acquire");
                        tokio::task::yield_now().await;
                        lease.release().await.expect("release");
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    group.finish();
}

/// Quantifies the inline small-write zero-copy win: `CachedMetadata` is cloned
/// ~3x per small write (moka `get`, `meta.clone()`). With `data_key: Bytes` the
/// inline-payload clone is an O(1) refcount bump; the old `Vec<u8>` layout paid a
/// full deep copy of the payload on every clone. Both are benched side by side.
fn bench_metadata_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_clone");

    let inline_4k = squeezefs::routing::CachedMetadata {
        file_type: "inline".into(),
        size: 4096,
        data_key: Some(bytes::Bytes::from(vec![0xABu8; 4096])),
        ..Default::default()
    };
    group.bench_function("cached_metadata_clone_inline_4k_bytes", |b| {
        b.iter(|| {
            let m = black_box(&inline_4k).clone();
            black_box(m);
        });
    });

    // Reference point: the per-clone cost the old `data_key: Vec<u8>` layout paid.
    let payload_vec = vec![0xABu8; 4096];
    group.bench_function("vec_u8_deep_copy_4k", |b| {
        b.iter(|| {
            let v = black_box(&payload_vec).clone();
            black_box(v);
        });
    });

    group.finish();
}

/// Dynamic meta routing (docs/design-dynamic-meta-routing.md §5.9): the
/// derived-width hot-path arithmetic and the SlotSet control-plane ops —
/// the idle-cost claims' standing regression instrument.
fn bench_dynamic_meta_routing(c: &mut Criterion) {
    use squeezefs::meta_backend::kv::slot_set::SlotSet;
    use squeezefs::meta_backend::{
        make_global_ino_width, plan_meta_slot_set, route_ino_width, DERIVED_ROUTING_WIDTH,
    };

    let mut group = c.benchmark_group("dynamic_meta_routing");
    let w = u64::from(DERIVED_ROUTING_WIDTH);

    // The per-op routing arithmetic at the derived width (§5.9: ~1 ns).
    group.bench_function("route_ino_round_trip_w65536", |b| {
        let mut ino = 2u64;
        b.iter(|| {
            let (slot, local) = route_ino_width(black_box(ino), w);
            ino = ino.wrapping_add(7919).max(2);
            black_box(make_global_ino_width(local, slot, w))
        });
    });

    // The O(W) format-time plan (one stride run per member).
    group.bench_function("plan_meta_slot_set_v4", |b| {
        b.iter(|| black_box(plan_meta_slot_set(black_box(4)).unwrap()));
    });

    // SlotSet membership over a fresh half-width run.
    let half: Vec<u16> = (0..=u16::MAX).step_by(2).collect();
    let set = SlotSet::from_slots(&half);
    group.bench_function("slot_set_contains_half_width", |b| {
        let mut s = 0u16;
        b.iter(|| {
            s = s.wrapping_add(31);
            black_box(set.contains(black_box(s)))
        });
    });

    // Control-plane normalization: coalesce 32768 slots back to runs
    // (the migration-mutation cost shape).
    group.bench_function("slot_set_from_slots_half_width", |b| {
        b.iter(|| black_box(SlotSet::from_slots(black_box(&half))));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_high_concurrency,
    bench_cluster_dlm,
    bench_metadata_clone,
    bench_dynamic_meta_routing
);
criterion_main!(benches);
