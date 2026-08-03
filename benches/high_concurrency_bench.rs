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

    // S2 rows (feat/dlm-s2-durable-term) — the composed-token cost
    // against S1, measured as an in-bench A/B: every row ABOVE ran with
    // no durable term adopted (term 0 ⇒ `(0 << 40) | seq` is bit-for-bit
    // the S1 token and the S1 read path), and the two rows below repeat
    // the same shapes after a mount-shaped adoption. Field shape
    // (microbench law): a write mount publishes ONE term per volume at
    // the D0 gate and then mints per open-for-write episode (spec §6.2),
    // so the steady-state cost is entirely on the mint + read sides —
    // one shift/or in the mint, one atomic load + max on the unheld
    // read (`LAST_GRANT_FLOOR` vs the era base).
    squeezefs::dlm::adopt_durable_term(7);

    // The durable-term read itself: what every unheld fencing read now
    // folds in (and what the mount-time sweep consults per record).
    group.bench_function("durable_term_read", |b| {
        b.iter(|| black_box(squeezefs::dlm::term_base()));
    });

    // Composed mint: same distinct-ino walk shape as the S1 row above.
    group.bench_function("acquire_release_distinct_ino_walk_composed", |b| {
        let dlm = dlm.clone();
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i = i.wrapping_add(1);
            let path = format!("inode_{}", 910_000_000 + (i % 65_536));
            let dlm = dlm.clone();
            async move {
                let lease = dlm
                    .acquire_lock(&path, None, std::time::Duration::from_secs(1))
                    .await
                    .expect("composed walk acquire");
                lease.release().await.expect("composed walk release");
            }
        });
    });

    // Composed unheld read: the released-object generation under an
    // adopted era (floor vs era-base max — the FIND-M11-A credential
    // shape a fresh process now serves from the era, never from 0).
    rt.block_on(async {
        dlm.acquire_lock("inode_800777002", None, std::time::Duration::from_secs(1))
            .await
            .expect("composed unheld seed acquire")
            .release()
            .await
            .expect("composed unheld seed release");
    });
    group.bench_function("get_fencing_token_ino_unheld_composed", |b| {
        b.iter(|| black_box(dlm.get_fencing_token_ino(black_box(800_777_002u64))));
    });

    // S11 rows (feat/mw-range-custody) — byte-range custody. Field shape
    // (microbench law): acquisition is per open-for-write EPISODE, not per
    // op (`get_or_acquire_lease` caches in `active_leases` — spec §6.2), so
    // what these rows price is the arbitration itself at the live-range
    // POPULATION a hot shared file accumulates: one grant per writing
    // application/rank (execution-plan ruling D8's MPI-IO shape), which is
    // tens, not millions. `_pop{N}` = N foreign live ranges already held on
    // the same file while the measured span is acquired and released.
    //
    // The row that matters for regressions is the whole-file
    // `acquire_release_uncontended_ino` ABOVE (every production verb takes
    // it): its ID is unchanged so the S0/S1/S2 series stays comparable.
    for pop in [1u64, 16, 256] {
        let ino_base = 920_000_000 + pop * 1_000_000;
        let path = format!("inode_{ino_base}");
        // Seed the population: `pop` disjoint 1 MiB spans, held for the row.
        let held: Vec<_> = rt.block_on(async {
            let mut v = Vec::new();
            for i in 0..pop {
                v.push(
                    dlm.acquire_lock(
                        &path,
                        Some((i << 20, (i + 1) << 20)),
                        std::time::Duration::from_secs(5),
                    )
                    .await
                    .expect("population seed acquire"),
                );
            }
            v
        });

        // Disjoint-span acquire+release above the live population (the
        // sorted-interval insert + stab-window probe).
        group.bench_function(format!("acquire_release_range_disjoint_pop{pop}"), |b| {
            let dlm = dlm.clone();
            let path = path.clone();
            let mut i = 0u64;
            b.to_async(&rt).iter(|| {
                i = i.wrapping_add(1);
                let start = (pop + (i % 64)) << 20;
                let dlm = dlm.clone();
                let path = path.clone();
                async move {
                    let lease = dlm
                        .acquire_lock(
                            &path,
                            Some((start, start + (1 << 20))),
                            std::time::Duration::from_secs(1),
                        )
                        .await
                        .expect("disjoint range acquire");
                    lease.release().await.expect("release");
                }
            });
        });

        // The CONFLICT DECISION itself is priced by the
        // `span_range_shared_probe_pop{N}` row below: it runs the same
        // stab-window scan over the same population. A refused *acquire*
        // is deliberately NOT a row — measured at every population it
        // costs ~1.08 ms whatever the population is, i.e. it prices the
        // tokio timer registration behind the wait budget, not the
        // arbitration. The handoff cost of contended spans is the
        // `acquire_release_contended_1span_8tasks` row.

        // The W1 seventh clause's probe (`patch_range_shared` → the DLM
        // custody read) at the same population — this one IS on the patch
        // hot path (61–67 k IOPS), once per candidate block.
        group.bench_function(format!("span_range_shared_probe_pop{pop}"), |b| {
            b.iter(|| {
                black_box(squeezefs::dlm::span_range_shared(
                    black_box(ino_base),
                    black_box(4 << 20),
                    black_box(8 << 20),
                    black_box(0),
                ))
            });
        });

        rt.block_on(async {
            for lease in held {
                lease.release().await.expect("population release");
            }
        });
    }

    // The clause-7 probe on the SHIPPED shape: a live whole-file lease
    // (whole-inode custody, zero live ranges) — the cost every W1 patch
    // pays on a production mount.
    let whole_held = rt.block_on(async {
        dlm.acquire_lock("inode_921000001", None, std::time::Duration::from_secs(5))
            .await
            .expect("whole-file seed acquire")
    });
    group.bench_function("span_range_shared_probe_whole_file_custody", |b| {
        b.iter(|| {
            black_box(squeezefs::dlm::span_range_shared(
                black_box(921_000_001u64),
                black_box(0),
                black_box(4 << 20),
                black_box(0),
            ))
        });
    });
    rt.block_on(async { whole_held.release().await.expect("release") });

    // Serialized handoff over ONE overlapping span: 8 tasks contend for the
    // same bytes (the range analogue of the contended whole-file row).
    group.bench_function("acquire_release_contended_1span_8tasks", |b| {
        let dlm = dlm.clone();
        b.to_async(&rt).iter(|| {
            let dlm = dlm.clone();
            async move {
                let futures = (0..8).map(|_| {
                    let dlm = dlm.clone();
                    async move {
                        let lease = dlm
                            .acquire_lock(
                                "inode_922000001",
                                Some((0, 1 << 20)),
                                std::time::Duration::from_secs(5),
                            )
                            .await
                            .expect("contended span acquire");
                        tokio::task::yield_now().await;
                        lease.release().await.expect("release");
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    // S4 rows (feat/dlm-s4-slot-locks) — the slot-homed lock authority's
    // ADDED cost, in solo mode. FIELD SHAPE (microbench law): `DlmClient`
    // above IS `SlotLockManager` since S4, so every acquire row in this
    // group already runs through homing + ownership; these two rows
    // decompose what that added, at the shape the field runs it —
    // `crate::keys::inode_path`'s `inode_{N}` path form (the ONE product
    // key form, `get_or_acquire_lease`, once per open-for-write episode,
    // spec §6.2) homed over the DERIVED width `W = 65536` that every
    // stamped set freezes, and the ownership probe on the solo answer
    // (no owner table installed — the only production posture).
    //
    // PREDICTION (to be tested when the D11 bench freeze lifts; NOT
    // measured on this branch): homing is one `strip_prefix` + digit scan
    // + `u64` parse + one modulo, and the ownership probe is one
    // `ArcSwapOption` load with a null fast path — together predicted
    // ≲ 20 ns, i.e. under ~2 % of the ~1 µs `acquire_release_uncontended_
    // ino` row above, which is the row the S4 gate's "within noise"
    // verdict actually rests on (its ID is deliberately unchanged so the
    // S0/S1/S2/S4 series stays comparable in the baseline reference).
    // Falsification: if `s4_lock_home_slot_ino_path` alone lands anywhere
    // near the acquire row, homing belongs in the lease object (computed
    // once per episode) rather than at the entry point.
    squeezefs::dlm_slot::publish_routing_width(u64::from(
        squeezefs::meta_backend::DERIVED_ROUTING_WIDTH,
    ));
    group.bench_function("s4_lock_home_slot_ino_path", |b| {
        b.iter(|| {
            black_box(squeezefs::dlm_slot::lock_home_slot(black_box(
                "inode_900123456",
            )))
        });
    });
    group.bench_function("s4_is_local_slot_solo", |b| {
        b.iter(|| black_box(squeezefs::dlm_slot::is_local_slot(black_box(42u64))));
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

/// **POSIX-6 / POSIX-16 — the two hot error-path primitives.**
///
/// FIELD SHAPE (why these inputs): `to_errno` runs on EVERY error return
/// the daemon makes — the dominant live shape is the ENOENT grammar of
/// POSIX lookups (negative dentries, unlink/stat probes: thousands per
/// `rsync`/bench run, which is why `map_squeezefs_err` logs it at debug
/// rather than error), followed by the structured refusals the create
/// path mints on collision. The pre-POSIX-6 mapping ran up to three
/// `String::contains` scans over the message before answering; the
/// structured mapping is a match arm. Both are measured here so the
/// substring form can never come back "for convenience".
///
/// The POSIX-16 latch sits on EVERY `flush`/`fsync` — the close path of
/// every file — so its healthy-mount cost (the `writeback_error_count`
/// gate: one relaxed load, no map touch) is the number that matters;
/// the latch/report cycle is priced beside it for the failing case.
fn bench_error_paths(c: &mut Criterion) {
    use squeezefs::error::SqueezefsError;

    let mut group = c.benchmark_group("error_paths");

    // The lookup grammar: an OS errno passes through verbatim.
    let enoent = SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
    group.bench_function("to_errno_io_raw_os", |b| {
        b.iter(|| black_box(black_box(&enoent).to_errno()));
    });

    // An in-process io error: the kind table (no raw_os_error).
    let kind = SqueezefsError::Io(std::io::Error::new(
        std::io::ErrorKind::StorageFull,
        "staging full",
    ));
    group.bench_function("to_errno_io_kind_table", |b| {
        b.iter(|| black_box(black_box(&kind).to_errno()));
    });

    // The structured refusal (POSIX-6): one match arm, message untouched.
    let refused = SqueezefsError::already_exists("File already exists");
    group.bench_function("to_errno_structured_refusal", |b| {
        b.iter(|| black_box(black_box(&refused).to_errno()));
    });

    // The generic refusal: the class that used to pay the substring
    // scans (this message is the shape those rules searched).
    let invalid = SqueezefsError::InvalidOperation(
        "kv metadata: no space: 3 free extents with the 8-extent compaction reserve \
         intact — allocation refused (ENOSPC)"
            .to_string(),
    );
    group.bench_function("to_errno_invalid_operation_long_msg", |b| {
        b.iter(|| black_box(black_box(&invalid).to_errno()));
    });

    group.finish();
}

/// The POSIX-16 latch, priced on the FS the whole daemon shares.
fn bench_writeback_latch(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let fs = rt.block_on(bench_fs());

    let mut group = c.benchmark_group("writeback_error_latch");

    // The always-case: a healthy mount's flush/fsync probe. One relaxed
    // load; the map is never touched.
    group.bench_function("probe_clean_mount", |b| {
        b.iter(|| black_box(fs.take_writeback_error(black_box(42))));
    });

    // The failing case: latch + report, the full errseq cycle.
    group.bench_function("latch_and_report_cycle", |b| {
        let mut ino = 100u64;
        b.iter(|| {
            ino = ino.wrapping_add(1).max(100);
            fs.note_writeback_error(black_box(ino), libc::EIO);
            black_box(fs.take_writeback_error(black_box(ino)))
        });
    });

    // A latched mount probing an UNRELATED inode (the per-inode law's
    // cost once the gate is open — one hash probe).
    fs.note_writeback_error(7, libc::EIO);
    group.bench_function("probe_other_inode_while_latched", |b| {
        b.iter(|| black_box(fs.take_writeback_error(black_box(999_999))));
    });

    group.finish();
}

/// A minimal in-process filesystem for the latch bench (no metadata
/// backend needed — the latch is FS-local state).
async fn bench_fs() -> squeezefs::fuse_client::SqueezefsFilesystem {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    let dlm = DlmClient::new().unwrap();
    let backing = tempfile::NamedTempFile::new().unwrap();
    backing.as_file().set_len(64 * 1024 * 1024).unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("bench_latch").await.unwrap());
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    // The temp files outlive the bench through the closure's captures.
    std::mem::forget(backing);
    std::mem::forget(staging);
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000)
}

/// **RES-20** (pre-RC spec §7): the FORGET → reclaim-queue enqueue.
///
/// Field shape: a `drop_caches` storm — or any unlink/`find`-heavy
/// workload — delivers FORGET/BATCH_FORGET in bulk on the classical
/// sideband, and each forgotten `nlink == 0` ino must reach the reclaim
/// queue. The pre-fix enqueue did `tokio::spawn(async move { tx.send(ino)
/// .await })`: **one task per ino**, dispatched onto the current fuse3
/// handler lane's `LocalSet`, whose only work was a channel send that
/// would have succeeded immediately (the queue is 100 000 deep). This
/// group prices the enqueue itself at the storm's cadence — 1 024 inos,
/// the batch-forget-class burst.
///
/// The `try_send` shape must be flat and allocation-free; the spawning
/// shape pays a task allocation + a lane hop + a wake per ino.
fn bench_reclaim_enqueue(c: &mut Criterion) {
    use squeezefs::fuse_client::ReclaimEnqueue;

    const BURST: u64 = 1024;
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("reclaim_enqueue");
    group.throughput(criterion::Throughput::Elements(BURST));

    // The shipped path: room in the queue ⇒ zero tasks. Measured INSIDE a
    // runtime (the daemon's venue) so the comparison is honest.
    group.bench_function("try_send_burst_1024", |b| {
        b.iter_batched(
            || {
                let (tx, rx) = tokio::sync::mpsc::channel::<u64>(200_000);
                (ReclaimEnqueue::new(tx), rx)
            },
            |(q, rx)| {
                let _g = rt.enter();
                for ino in 2..(BURST + 2) {
                    q.enqueue(black_box(ino));
                }
                black_box(rx)
            },
            criterion::BatchSize::LargeInput,
        );
    });

    // The pre-fix shape, kept as the A0 control: one spawned task per ino.
    group.bench_function("spawn_per_ino_burst_1024", |b| {
        b.iter_batched(
            || tokio::sync::mpsc::channel::<u64>(200_000),
            |(tx, rx)| {
                let _g = rt.enter();
                for ino in 2..(BURST + 2) {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(ino).await;
                    });
                }
                black_box(rx)
            },
            criterion::BatchSize::LargeInput,
        );
    });
    group.finish();
}

/// **PERF-3 · sharded vs process-global per-op counting.**
///
/// The transport phase histograms record ~5 spans per request and a latency
/// distribution is tight by construction, so nearly every op of a class hits
/// the SAME bucket word: the counting cost is a cache line ping-ponging
/// between every queue worker and handler lane. `Align64` prevents false
/// sharing but not TRUE sharing.
///
/// This is the primitive both arms reduce to, at the field's concurrency
/// (one recording thread per core): N threads × M increments against ONE
/// atomic vs against per-thread shards (the shipped shape in
/// `crates/fuse3/src/raw/read_phase.rs`, where a shard is a whole
/// `PhaseTable` so shards never share a line). Correctness is unaffected —
/// snapshots sum the shards.
fn bench_sharded_counters(c: &mut Criterion) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const THREADS: usize = 8;
    const PER_THREAD: usize = 4_096;

    let mut group = c.benchmark_group("perop_counter_sharding");
    group.throughput(criterion::Throughput::Elements(
        (THREADS * PER_THREAD) as u64,
    ));

    group.bench_function("global_one_line", |b| {
        let ctr = Arc::new(AtomicU64::new(0));
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let ctr = Arc::clone(&ctr);
                hs.push(std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        black_box(ctr.fetch_add(1, Ordering::Relaxed));
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.bench_function("sharded_per_thread", |b| {
        // One cache-line-isolated shard per thread (the read_phase layout:
        // a shard is a whole table, so no two shards share a line).
        #[repr(align(64))]
        struct Shard(AtomicU64);
        let shards: Arc<Vec<Shard>> =
            Arc::new((0..THREADS).map(|_| Shard(AtomicU64::new(0))).collect());
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for t in 0..THREADS {
                let shards = Arc::clone(&shards);
                hs.push(std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        black_box(shards[t].0.fetch_add(1, Ordering::Relaxed));
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_reclaim_enqueue,
    bench_sharded_counters,
    bench_high_concurrency,
    bench_cluster_dlm,
    bench_metadata_clone,
    bench_dynamic_meta_routing,
    bench_error_paths,
    bench_writeback_latch
);
criterion_main!(benches);
