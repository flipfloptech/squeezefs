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
    let active_inode_locks = Arc::new(StripeLocks::<tokio::sync::RwLock<()>>::new(4096));
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

    // S7 rows (feat/dlm-s7-data-fence) — the data-plane custody-epoch
    // fence and the dead-epoch quarantine.
    //
    // FIELD-DERIVED SHAPES (the microbench law):
    //
    // * `authorize_dma` runs ONCE PER DMA SUBMISSION — the per-op cost the
    //   whole mechanism has to justify. The field rate that sets its
    //   budget is the W1 random-write plateau, 61–67 k IOPS
    //   (`.benchmarks/2026-07-17-rand-write-program-closing.md`), plus the
    //   streaming block rate (~1,650 blocks/s displaced at 6.3–6.8 GB/s,
    //   `.benchmarks/2026-07-31-write-wall.md`). At 67 k submissions/s a
    //   100 ns gate would be 0.67 % of one core; the implementation is one
    //   relaxed `AtomicBool` load, one `AtomicU64` load and one compare,
    //   so the PREDICTION is single-digit nanoseconds, indistinguishable
    //   from the pre-S7 `data_dma_fence_refusals` latch load. Both arms are
    //   priced: `None` (authorize-at-submit — every pre-S7 call site) and
    //   `Some(current)` (the write-pipeline permit's carried epoch, the
    //   only arm that adds a comparison).
    // * The refused arm is priced too, because a fenced mount's cost
    //   matters for how fast it fail-stops, not for throughput: it adds a
    //   `fetch_add` on two counters and one `log::error!`.
    // * The quarantine probe rides `finish_free` — one lock-free lookup on
    //   an EMPTY `scc::HashMap` on every single-writer mount (the shipped
    //   posture), at the terminal-free rate (≈ the displaced-block rate
    //   above). The populated row uses 256 entries: a dead epoch's cohort
    //   is a recovery-window population — the job wire's shard is ≤ its
    //   `plan_blocks` count — never a per-op one.
    group.bench_function("authorize_dma_uncarried", |b| {
        b.iter(|| black_box(squeezefs::data_custody::authorize_dma(black_box(None))));
    });
    group.bench_function("authorize_dma_carried_current", |b| {
        let auth = squeezefs::data_custody::current_epoch();
        b.iter(|| {
            black_box(squeezefs::data_custody::authorize_dma(black_box(Some(
                auth,
            ))))
        });
    });
    group.bench_function("authorize_dma_carried_stale_refused", |b| {
        // An era that cannot be current (the durable term is 7 above, and
        // `term_base` composes it into the high bits).
        let stale = squeezefs::data_custody::CustodyEpoch::from_raw(1);
        b.iter(|| {
            black_box(squeezefs::data_custody::authorize_dma(black_box(Some(
                stale,
            ))))
        });
    });

    // The quarantine probe at the two populations that matter.
    {
        let quarantine = squeezefs::data_custody::BlockQuarantine::new();
        group.bench_function("quarantine_probe_empty", |b| {
            b.iter(|| black_box(quarantine.contains(black_box(4 << 20))));
        });
        let epoch = squeezefs::data_custody::declare_dead_epoch("bench: cohort population");
        for i in 0..256u64 {
            quarantine.admit(i * (4 << 20), epoch);
        }
        group.bench_function("quarantine_probe_pop256_miss", |b| {
            b.iter(|| black_box(quarantine.contains(black_box(1_000_000 * (4 << 20)))));
        });
        group.bench_function("quarantine_probe_pop256_hit", |b| {
            b.iter(|| black_box(quarantine.contains(black_box(128 * (4 << 20)))));
        });
        // Release restores the gauge (the bench must not leave
        // `dlm_quarantined_offsets` inflated for a stats-reading rig).
        quarantine.release(epoch);
    }

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
/// DLM **S9** — the multi-writer data plane's two hot decisions
/// (`docs/pre-rc-engineering-spec.md` §6.9 S9; execution-plan ruling
/// **D11**: WRITTEN AND NOT RUN — no number about S9 exists yet).
///
/// # Field-derived shapes (the microbench law: toy inputs are violations)
///
/// * **The custody probe on the DMA path.** `data_custody::authorize_dma`
///   runs on EVERY data-plane submission — the same census as
///   `NvmeBlockDev::write_block`'s gate, which the field capture puts at
///   ~2,600 block publishes/s against a 21–25 k device-writes/s namespace
///   ceiling (`.benchmarks/2026-08-01-rewrite-publish-drain.md` §3). Both
///   arms are measured: `carried = None` (every pre-S7 site) and
///   `carried = Some(current)` (the write-pipeline permit and S9's remote
///   grants), plus the STALE arm, because a refusal's cost is what a
///   revocation storm pays.
/// * **The grant/renew path, minus the fabric.** A grant is one authority
///   arbitration (`LocalLockManager::acquire_lock_mode`) plus one adoption
///   (`dlm::adopt_remote_grant`: era adopt, floor raise, custody record);
///   a renewal is one `scc` probe and two stores. The RTT is deliberately
///   NOT in these rows — S3 measured the wire (0.05–0.25 ms loopback,
///   235 µs on the fabric-latency venue) and §6.5 item 1's arithmetic is
///   about that term, so mixing it in would hide the CPU cost these rows
///   exist to bound. The shape is one grant per open-for-write episode on
///   DISTINCT inos (spec §6.2's lease census), and the shared-file row is
///   ruling D8's: disjoint 4 MiB block ranges of ONE large file, which is
///   the MPI-IO shape S11's gate names.
///
/// # Predictions, and what falsifies them
///
/// 1. **`authorize_dma` costs ≲ 15 ns** in both healthy arms — one relaxed
///    load of the poison latch, one `term_base()` load, one generation
///    load, one compare. FALSIFIED if either healthy arm exceeds ~25 ns or
///    if the `Some(current)` arm is more than ~3 ns above `None` (that
///    would mean the carried comparison is not the free branch it looks
///    like, and the write-pipeline permit is paying for the epoch it
///    carries).
/// 2. **The S9 composition is free for single-writer mounts**:
///    `authorize_dma` at generation 0 must be indistinguishable from
///    S7's (which was `term_base()` alone). FALSIFIED by any measurable
///    separation between `authorize_none` here and the S7-era row — that
///    would mean the `| (gen & MASK)` composition is not folded away, and
///    the "byte-identical on single-writer mounts" claim is wrong.
/// 3. **A grant's CPU is ≲ 2 µs** (arbitration + adoption), i.e. under 1 %
///    of the 235 µs fabric RTT it rides — so the fabric, not this code, is
///    what a shared-file custody row measures. FALSIFIED if the grant row
///    exceeds ~10 µs, which would make custody CPU a visible term at
///    15 k-shaped fan-out and would move the S10 delegation argument onto
///    the data plane too.
/// 4. **A renewal is ≲ 200 ns** and does not grow with the number of live
///    grants (it touches the client lease's row, never the grant table).
///    FALSIFIED by any slope against the seeded grant population — that
///    would mean the heartbeat is O(grants) and 15 k co-writers × their
///    grants would serialize on it, which is exactly the shape §6.5 item 3
///    measured and S6 deleted.
/// 5. **Disjoint-range adoption does not degrade with the live span
///    count** beyond the S11 stab-window bound (O(log n + c)). FALSIFIED
///    by super-logarithmic growth from 1 → 64 live spans on one ino, which
///    would mean the sorted interval list is being scanned rather than
///    stabbed.
fn bench_s9_custody(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("s9_custody");

    // ---------------------------------------------------------------
    // The DMA-path probe. Every data-plane submission pays exactly this.
    // ---------------------------------------------------------------
    group.bench_function("authorize_dma_none", |b| {
        b.iter(|| black_box(squeezefs::data_custody::authorize_dma(black_box(None)).is_ok()));
    });
    let current = squeezefs::data_custody::current_epoch();
    group.bench_function("authorize_dma_carried_current", |b| {
        b.iter(|| {
            black_box(squeezefs::data_custody::authorize_dma(black_box(Some(current))).is_ok())
        });
    });
    // The refusal arm: what a revocation storm pays per in-flight
    // authorization it kills. Deliberately a FOREIGN epoch rather than an
    // advanced generation — advancing is monotone and would poison the
    // other rows' baseline.
    let stale = squeezefs::data_custody::CustodyEpoch::from_raw(squeezefs::dlm::compose_token(
        squeezefs::dlm::durable_term(),
        1,
    ));
    group.bench_function("authorize_dma_carried_stale_refusal", |b| {
        b.iter(|| {
            black_box(squeezefs::data_custody::authorize_dma(black_box(Some(stale))).is_err())
        });
    });
    group.bench_function("current_epoch", |b| {
        b.iter(|| black_box(squeezefs::data_custody::current_epoch().raw()));
    });

    // ---------------------------------------------------------------
    // The authority's grant/renew path, fabric excluded (see the header).
    // ---------------------------------------------------------------
    let clocks = squeezefs::membership::LeaseClocks::with_params(
        std::time::Duration::from_secs(45),
        std::time::Duration::from_millis(23),
        std::time::Duration::from_millis(2_000),
    )
    .expect("the shipped clock shape");
    let owner = squeezefs::data_grant::WriteCustodyOwner::arm(
        "bench-authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        squeezefs::membership::LeaseClock::monotonic(),
        None,
    )
    .expect("the authority arms");
    let lease = owner
        .join(&squeezefs::data_grant::JoinFrame {
            schema: squeezefs::data_grant::CUSTODY_SCHEMA,
            client: "bench-co-writer".to_string(),
            pr_key: 0xbeef,
            prior_epoch: None,
        })
        .expect("join");

    // One grant per open-for-write episode on DISTINCT inos (§6.2's lease
    // census) — arbitration + insert, with the release retiring the
    // authority's own lease so the table stays at its live population.
    group.bench_function("grant_release_whole_file_distinct_ino", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i = i.wrapping_add(1);
            let req = squeezefs::data_grant::AcquireFrame {
                schema: squeezefs::data_grant::CUSTODY_SCHEMA,
                client: "bench-co-writer".to_string(),
                lease_epoch: lease.epoch,
                ino: 700_000_000 + (i % 65_536),
                span: None,
                concurrent_write: false,
                wait_ms: 0,
                desired: None,
            };
            let owner = &owner;
            async move {
                let grant = owner.grant(&req).await.expect("uncontended grant");
                black_box(owner.release("bench-co-writer", &[grant.grant_id]));
            }
        });
    });

    // Ruling D8's shape: disjoint 4 MiB block ranges of ONE large file
    // (the MPI-IO row S11's gate names). The stab window is what this
    // measures — prediction 5.
    group.bench_function("grant_release_disjoint_range_one_file", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i = i.wrapping_add(1);
            let block = i % 64;
            let req = squeezefs::data_grant::AcquireFrame {
                schema: squeezefs::data_grant::CUSTODY_SCHEMA,
                client: "bench-co-writer".to_string(),
                lease_epoch: lease.epoch,
                ino: 700_999_999,
                span: Some((block * (4 << 20), (block + 1) * (4 << 20))),
                concurrent_write: false,
                wait_ms: 0,
                desired: None,
            };
            let owner = &owner;
            async move {
                let grant = owner.grant(&req).await.expect("disjoint span granted");
                black_box(owner.release("bench-co-writer", &[grant.grant_id]));
            }
        });
    });

    // The heartbeat: one `scc` probe and two stores, carrying the
    // in-flight destination set the write pipeline's depth bounds (the
    // field's converged depth is tens of blocks — `write_pipeline_
    // depth_target`), so 32 offsets is the field shape, not a toy.
    let inflight: Vec<u64> = (0..32).map(|i| i * (4 << 20)).collect();
    group.bench_function("renew_lease_32_inflight", |b| {
        b.iter(|| {
            black_box(
                owner
                    .renew(
                        black_box("bench-co-writer"),
                        lease.epoch,
                        black_box(&inflight),
                    )
                    .is_ok(),
            )
        });
    });

    group.finish();
}

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
                let (tx, rx) = squeezefs_ipc::sqz_channel::mpsc::channel::<u64>(200_000);
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

    // e2e audit A (2026-09-02): `LatencyHistogram::record` grew from ONE
    // relaxed RMW (bucket) to THREE (bucket + count + sum_ns) so every
    // family's mean is exact. The pair below prices that at the field's
    // recording shape — one process-global histogram hit from every lane
    // with a tight distribution (one hot bucket line, like the
    // `global_one_line` arm): the bucket-only control vs the shipped
    // record. `hist_record_exact_1thread` is the uncontended per-record
    // cost the D lock-guard hooks are gated on (~50 ns/acquire rule).
    group.bench_function("hist_record_bucket_only", |b| {
        let hist: Arc<[AtomicU64; 26]> = Arc::new([const { AtomicU64::new(0) }; 26]);
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let hist = Arc::clone(&hist);
                hs.push(std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let d = std::time::Duration::from_nanos(100_000 + i as u64);
                        let idx =
                            squeezefs::latency_core::latency_bucket_index(d.as_micros() as u64);
                        black_box(hist[idx].fetch_add(1, Ordering::Relaxed));
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.bench_function("hist_record_exact", |b| {
        let hist = Arc::new(squeezefs::fuse_client::LatencyHistogram::default());
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let hist = Arc::clone(&hist);
                hs.push(std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        hist.record(std::time::Duration::from_nanos(100_000 + i as u64));
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.throughput(criterion::Throughput::Elements(1));

    // e2e audit D: the `INODE_META_LOCKS` (3.5) guard now records its wait
    // (a zero sample, no clock read, on the uncontended fast path) and its
    // HOLD (one `Instant` at acquire + one at drop) at every site. This
    // pair prices the uncontended acquire→drop cycle against a bare
    // `try_lock` guard on the same primitive — the ~50 ns/acquire rule
    // the hold half's always-on posture is gated on.
    group.bench_function("stripe_lock_bare_try_lock_1thread", |b| {
        let lock = squeezefs::sqz_sync::SqzMutex::new(());
        b.iter(|| {
            let g = lock.try_lock().expect("uncontended");
            black_box(&g);
            drop(g);
        });
    });
    group.bench_function("stripe_lock_guard_wait_only_1thread", |b| {
        squeezefs::fuse_client::set_stripe_hold_timing(false);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        b.iter_custom(|iters| {
            rt.block_on(async {
                let t0 = std::time::Instant::now();
                for _ in 0..iters {
                    let g = squeezefs::routing::meta_lock_acquire(0x5EED).await;
                    black_box(&g);
                    drop(g);
                }
                t0.elapsed()
            })
        });
    });
    group.bench_function("stripe_lock_guard_wait_hold_1thread", |b| {
        squeezefs::fuse_client::set_stripe_hold_timing(true);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        b.iter_custom(|iters| {
            rt.block_on(async {
                let t0 = std::time::Instant::now();
                for _ in 0..iters {
                    let g = squeezefs::routing::meta_lock_acquire(0x5EED).await;
                    black_box(&g);
                    drop(g);
                }
                t0.elapsed()
            })
        });
        squeezefs::fuse_client::set_stripe_hold_timing(false);
    });

    group.bench_function("hist_record_bucket_only_1thread", |b| {
        let hist: [AtomicU64; 26] = [const { AtomicU64::new(0) }; 26];
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let d = std::time::Duration::from_nanos(100_000 + i);
            let idx = squeezefs::latency_core::latency_bucket_index(d.as_micros() as u64);
            black_box(hist[idx].fetch_add(1, Ordering::Relaxed));
        });
    });
    group.bench_function("hist_record_exact_1thread", |b| {
        let hist = squeezefs::fuse_client::LatencyHistogram::default();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            hist.record(std::time::Duration::from_nanos(100_000 + i));
        });
    });

    group.finish();
}

/// L3 coherence campaign (2026-08-08, `.benchmarks/2026-08-08-moka-coherence.md`):
/// the read-mostly cache vs the classic moka value cache it replaced, at
/// the FIELD shape (the microbench law: toy inputs are violations) —
/// **32 hot inos** (the standing 32×1g fileset) hammered by concurrent
/// readers (the field box runs 12 svc + 12 dd threads; the bench uses 8
/// so the row is stable across dev boxes), values = striped
/// `CachedMetadata` with a 256-entry block map (1 GiB at 4 MiB blocks).
///
/// What the rows can and cannot see: a 1-node box prices moka's per-read
/// bookkeeping at its LOCAL cost (LLC-resident RMWs — the field ledger's
/// ~7 % face); the 5–8× cross-socket amplification (54.6 % svc) only a
/// 2-node venue manufactures. The contended rows are the mechanism
/// instrument (bookkeeping RMWs vs pure loads), NOT the field win.
fn bench_read_mostly_cache(c: &mut Criterion) {
    use squeezefs::read_mostly_cache::{ReadMostlyCache, RmConfig};
    let mut group = c.benchmark_group("read_mostly");

    fn field_meta() -> squeezefs::routing::CachedMetadata {
        let mut map = std::collections::HashMap::new();
        for b in 0..256u32 {
            map.insert(
                b,
                format!("blocks/vol-0123456789abcdef/{}", b as u64 * 4096),
            );
        }
        squeezefs::routing::CachedMetadata {
            file_type: "striped".into(),
            size: 1 << 30,
            block_map: Some(std::sync::Arc::new(map)),
            ..Default::default()
        }
    }
    fn build(
        read_mostly: bool,
    ) -> ReadMostlyCache<u64, squeezefs::routing::CachedMetadata, ahash::RandomState> {
        let cache = ReadMostlyCache::new(
            RmConfig {
                name: "bench",
                read_mostly,
                capacity: 10_000,
                tti: Some(std::time::Duration::from_secs(
                    squeezefs::routing::METADATA_CACHE_TTI_SECS,
                )),
                ttl: None,
                touch_secs: None,
                pin: None,
                clock: None,
            },
            ahash::RandomState::new(),
        );
        for ino in 0..32u64 {
            cache.insert(ino, field_meta());
        }
        cache
    }

    const KEYS: u64 = 32;
    for (label, read_mostly) in [("rm", true), ("moka", false)] {
        let cache = build(read_mostly);
        let mut k = 0u64;
        group.bench_function(format!("{label}_get_hot_1t"), |b| {
            b.iter(|| {
                k = (k + 1) % KEYS;
                black_box(cache.get(black_box(&k)))
            });
        });
        let mut k2 = 0u64;
        group.bench_function(format!("{label}_peek_size_1t"), |b| {
            b.iter(|| {
                k2 = (k2 + 1) % KEYS;
                black_box(cache.peek_with(black_box(&k2), |m| m.size))
            });
        });
    }

    // Contended rows: 8 threads × 4096 gets each over the 32 shared keys
    // (per-iteration thread spawn matches the sharded_counters pattern —
    // constant across arms, so the DELTA is the bookkeeping term).
    const THREADS: usize = 8;
    const PER_THREAD: u64 = 4096;
    for (label, read_mostly) in [("rm", true), ("moka", false)] {
        let cache = std::sync::Arc::new(build(read_mostly));
        group.bench_function(format!("{label}_get_hot_8t_x4096"), |b| {
            b.iter(|| {
                let mut hs = Vec::with_capacity(THREADS);
                for t in 0..THREADS {
                    let cache = std::sync::Arc::clone(&cache);
                    hs.push(std::thread::spawn(move || {
                        for i in 0..PER_THREAD {
                            let k = (t as u64 * 7 + i) % KEYS;
                            black_box(cache.peek_with(&k, |m| m.size));
                        }
                    }));
                }
                for h in hs {
                    h.join().unwrap();
                }
            });
        });
    }

    group.finish();
}

/// Op-registry claim/release under storm (2026-08-11 write-IOPS campaign,
/// `.benchmarks/2026-08-11-write-iops-campaign-day1.md` addendum 2): the
/// FIELD shape is 32 fused workers each claiming at op rate with ~32
/// in-flight per worker (qd32 rand-4k, 525 k ops/s) — the flat-slab claim
/// was 6.65 % of worker self-cycles. The bench uses 8 threads × 32 held
/// claims so the row is stable across dev boxes; the mechanism instrument
/// is claim+release ns/op with slots HELD (the storm probes past busy
/// slots exactly as the field does).
fn bench_op_registry(c: &mut Criterion) {
    use squeezefs::fuse_client::{FuseOpKind, OpProf};

    const THREADS: usize = 8;
    const HELD: usize = 32;
    const OPS: usize = 2_048;

    let mut group = c.benchmark_group("op_registry");
    group.throughput(criterion::Throughput::Elements((THREADS * OPS) as u64));

    group.bench_function("claim_release_storm_held32", |b| {
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for t in 0..THREADS {
                hs.push(std::thread::spawn(move || {
                    // The standing in-flight population (ring depth).
                    let held: Vec<OpProf> = (0..HELD)
                        .map(|i| OpProf::begin(FuseOpKind::Write, (t * HELD + i) as u64))
                        .collect();
                    // Op-rate claim/release churn against that population.
                    for i in 0..OPS {
                        black_box(OpProf::begin(FuseOpKind::Write, i as u64));
                    }
                    drop(held);
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.finish();
}

/// Memory-tier contended GET (write-IOPS campaign 2026-08-11): the FIELD
/// shape is 32 handler lanes probing/serving the same ~32-file hot key
/// population per op — with `get_sync` (exclusive bucket lock) the gets
/// excluded each other (20.1 % of il lane cycles in
/// `bucket::Writer::lock_sync_wait`). The row pins the reader-class get:
/// 8 threads × 4k gets over 32 hot keys on ONE LruCache.
fn bench_tier_get_contended(c: &mut Criterion) {
    use squeezefs::cache::lru::LruCache;

    const THREADS: usize = 8;
    const OPS: usize = 4_096;

    let cache = Arc::new(LruCache::with_capacity(64 * 1024 * 1024));
    let keys: Vec<String> = (0..32).map(|i| format!("blocks/vol-bench/{i}")).collect();
    for k in &keys {
        cache.put(k, bytes::Bytes::from(vec![7u8; 4096]));
    }

    let mut group = c.benchmark_group("tier_get");
    group.throughput(criterion::Throughput::Elements((THREADS * OPS) as u64));
    group.bench_function("contended_hot32_8t", |b| {
        b.iter(|| {
            let mut hs = Vec::with_capacity(THREADS);
            for t in 0..THREADS {
                let cache = Arc::clone(&cache);
                let keys = keys.clone();
                hs.push(std::thread::spawn(move || {
                    for i in 0..OPS {
                        let k = &keys[(t + i) % keys.len()];
                        black_box(cache.get(k));
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

/// e2e audit A2 (2026-09-02): the per-op trace ring's HOOK cost — the
/// number every always-on phase record now pays on top of its histogram
/// RMWs. The contract the rows price: DISARMED ≤ ~1 ns per hook (one
/// pointer load / one thread-local read / one field compare), ARMED
/// ≤ ~20 ns per stamp (selection multiply + ring-index read + three
/// relaxed slot stores + one Release head store + one per-ring counter
/// RMW). Field shape: every op leaves ~10–20 stamps, so at the 1 M
/// IOPS class the armed cost is what the `divisor` derivation amortizes.
fn bench_op_trace_hook(c: &mut Criterion) {
    use squeezefs::op_trace::{self, Stage};
    use std::time::{Duration, Instant};

    let mut group = c.benchmark_group("op_trace_hook");
    group.throughput(criterion::Throughput::Elements(1));
    let now = Instant::now();

    // ---- disarmed (the shipped default) -------------------------------
    op_trace::disarm();
    let _ = op_trace::drain();
    group.bench_function("stamp_disarmed", |b| {
        let mut id = 1u64;
        b.iter(|| {
            id += 2;
            op_trace::stamp(black_box(id), Stage::ReadRouted, now);
        });
    });
    group.bench_function("stamp_current_disarmed", |b| {
        b.iter(|| op_trace::stamp_current(Stage::ReadRouted, black_box(now)));
    });
    group.bench_function("traced_disarmed", |b| {
        let mut id = 1u64;
        b.iter(|| {
            id += 2;
            black_box(op_trace::traced(black_box(id)));
        });
    });
    // The per-poll cost of the scope wrapper when the op binds nothing
    // (disarmed / unsampled): a field compare over the bare future.
    group.bench_function("scope_poll_unbound", |b| {
        b.iter(|| {
            let fut = op_trace::scope(black_box(0u64), std::future::ready(7u32));
            black_box(squeezefs_ipc::sqz_blocking::block_on(fut))
        });
    });
    group.bench_function("bare_poll_control", |b| {
        b.iter(|| {
            black_box(squeezefs_ipc::sqz_blocking::block_on(std::future::ready(
                7u32,
            )))
        });
    });

    // ---- armed, every op sampled, one ring deep enough to never drop
    // inside a measured window (drained between windows) --------------
    const CAP: usize = 1 << 16;
    op_trace::arm_for_tests_with_geometry(1, 4, CAP);
    group.bench_function("stamp_armed", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            let mut id = 1u64;
            while left > 0 {
                let n = left.min((CAP / 2) as u64);
                let t0 = Instant::now();
                for _ in 0..n {
                    id += 2;
                    op_trace::stamp(black_box(id), Stage::ReadRouted, now);
                }
                total += t0.elapsed();
                let _ = op_trace::drain();
                left -= n;
            }
            total
        });
    });
    group.bench_function("stamp_current_armed", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min((CAP / 2) as u64);
                total += squeezefs_ipc::sqz_blocking::block_on(op_trace::scope(0x5EED, async {
                    let t0 = Instant::now();
                    for _ in 0..n {
                        op_trace::stamp_current(Stage::ReadRouted, black_box(now));
                    }
                    t0.elapsed()
                }));
                let _ = op_trace::drain();
                left -= n;
            }
            total
        });
    });
    // Armed but UNSAMPLED (divisor 1 << 20): the cost an untraced op pays
    // under an armed ring — the load + the selection multiply.
    op_trace::arm_for_tests_with_geometry(1 << 20, 4, CAP);
    group.bench_function("stamp_armed_unsampled", |b| {
        let mut id = 1u64;
        b.iter(|| {
            id += 2;
            op_trace::stamp(black_box(id), Stage::ReadRouted, now);
        });
    });
    op_trace::disarm();
    let _ = op_trace::drain();
    group.finish();
}

/// A future that is Pending on its first poll and Ready on the second —
/// the device-read completion oneshot's shape (the DMA always outlives
/// the first poll, so `Timeout::poll` registers the sleep's waker).
struct PendingOnce(bool);

impl std::future::Future for PendingOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            std::task::Poll::Pending
        }
    }
}

/// One device read's timer ceremony (`nvme_dev.rs` `read_block`): arm
/// `sqz_time::timeout(d, rx)` (global-lock registry insert + heap push +
/// `Box::pin`), first poll Pending (global lock, waker clone), completion
/// poll Ready, drop (global lock, live-map remove — the heap entry stays
/// as a tombstone until the service thread pops it at `d`).
fn timeout_cycle(d: std::time::Duration) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut t = squeezefs_ipc::sqz_time::timeout(d, PendingOnce(false));
    let mut p = std::pin::Pin::new(&mut t);
    assert!(std::future::Future::poll(p.as_mut(), &mut cx).is_pending());
    assert!(std::future::Future::poll(p.as_mut(), &mut cx).is_ready());
    drop(t);
}

/// The timer-less alternative's per-op face: stamp `Instant::now()` into
/// the in-flight slot at submit, compare against the deadline at reap.
fn deadline_stamp_cycle(slot: &mut Option<std::time::Instant>, d: std::time::Duration) {
    *slot = Some(std::time::Instant::now());
    let submitted = slot.take().expect("stamped");
    assert!(black_box(submitted.elapsed()) < d);
}

/// One device read's completion channel (`nvme_dev.rs` `read_block` ↔
/// the worker's `complete_one`): channel mint (Arc alloc), caller poll
/// Pending (mutex, waker clone), worker `send` (mutex, wake), caller poll
/// Ready (mutex), Sender drop (mutex), Receiver drop (mutex).
fn sqz_oneshot_cycle() {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let (tx, mut rx) = squeezefs_ipc::sqz_channel::oneshot::channel::<u64>();
    assert!(std::future::Future::poll(std::pin::Pin::new(&mut rx), &mut cx).is_pending());
    tx.send(black_box(7)).expect("receiver alive");
    assert!(std::future::Future::poll(std::pin::Pin::new(&mut rx), &mut cx).is_ready());
}

/// The lock-free reference for the same cycle (`futures::channel::oneshot`:
/// atomic state word + a locked-by-bit waker slot, no mutex).
fn futures_oneshot_cycle() {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let (tx, mut rx) = futures::channel::oneshot::channel::<u64>();
    assert!(std::future::Future::poll(std::pin::Pin::new(&mut rx), &mut cx).is_pending());
    tx.send(black_box(7)).expect("receiver alive");
    assert!(std::future::Future::poll(std::pin::Pin::new(&mut rx), &mut cx).is_ready());
}

/// **e2e audit R-1 (candidate finding 48) — the per-device-read executor
/// primitives**, priced in isolation before the in-daemon attribution.
///
/// FIELD shape (`.benchmarks/2026-09-02-e2e-audit-baseline.md`): rand-4k
/// kernel reads at 441 k IOPS, each one `NvmeBlockDev::read_block` →
/// `sqz_time::timeout(30 s, oneshot)` over a crossbeam lane channel. Three
/// arms per read touch ONE process-global registry mutex
/// (`crates/squeezefs-ipc/src/sqz_time.rs`), and every completed read
/// leaves a heap tombstone the `sqz-timer` thread pops 30 s later under
/// that same lock — at the field rate that heap holds ≈ 13 M entries.
///
/// Rows: the timer cycle uncontended vs 8 / 32 threads (the handler-lane
/// population), with a 30 s deadline (the shipped shape — tombstones
/// accumulate) and a 2 ms deadline (the service thread pops them
/// concurrently, the steady-state lock-sharing face compressed); the
/// tombstone drain itself (200 k pops, against an empty heap and against a
/// 4 M-entry resident heap — log₂n sift-downs through a 64 MiB array);
/// the completion oneshot (sqz mutex vs the lock-free reference); the
/// crossbeam lane channel (already lock-free — the ledger's "per-channel
/// Mutex" claim is checked here); and the timer-less deadline stamp.
fn bench_read_fill_executor_prims(c: &mut Criterion) {
    use std::time::{Duration, Instant};

    const PER_THREAD: usize = 8_192;
    const FIELD_DEADLINE: Duration = Duration::from_secs(30);
    const SHORT_DEADLINE: Duration = Duration::from_millis(2);

    let mut group = c.benchmark_group("read_fill_executor_prims");
    // The 30 s-deadline rows leave one tombstone per op in the process-wide
    // heap for 30 s; a bounded sample keeps that population bounded.
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(1));

    // The service thread's tombstone drain (FIRST — before any 30 s row
    // leaves long-lived tombstones that would pop mid-measurement): arm N
    // sleeps at one near deadline, cancel them all, then time deadline →
    // N tombstones popped. The service pops the whole batch in ONE
    // critical section, so the probe that sees the count advance marks
    // the batch's end.
    const DRAIN: usize = 200_000;
    let loaded = std::cell::Cell::new(0usize);
    let drain_batch = |resident: usize| {
        // Resident far-future tombstones deepen the heap for the whole
        // process lifetime (they never fire) — arm them once.
        while loaded.get() < resident {
            drop(squeezefs_ipc::sqz_time::sleep(Duration::from_secs(3600)));
            loaded.set(loaded.get() + 1);
        }
        let popped0 = squeezefs_ipc::sqz_time::TIMER_TOMBSTONES_SKIPPED
            .load(std::sync::atomic::Ordering::Relaxed);
        // Far enough out that every arm lands before the service wakes
        // (an arm against the deep heap sifts up log₂n levels).
        let deadline = Instant::now() + Duration::from_millis(300);
        for _ in 0..DRAIN {
            drop(squeezefs_ipc::sqz_time::sleep_until(deadline));
        }
        assert!(Instant::now() < deadline, "arm loop outran the deadline");
        loop {
            let popped = squeezefs_ipc::sqz_time::TIMER_TOMBSTONES_SKIPPED
                .load(std::sync::atomic::Ordering::Relaxed);
            if popped - popped0 >= DRAIN as u64 {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        Instant::now().saturating_duration_since(deadline)
    };
    group.throughput(criterion::Throughput::Elements(DRAIN as u64));
    group.bench_function("tombstone_drain_200k_heap_200k", |b| {
        b.iter_custom(|iters| (0..iters).map(|_| drain_batch(0)).sum());
    });
    group.bench_function("tombstone_drain_200k_heap_4m", |b| {
        b.iter_custom(|iters| (0..iters).map(|_| drain_batch(4_000_000)).sum());
    });

    group.throughput(criterion::Throughput::Elements(1));
    group.bench_function("timeout_30s_cycle_1t", |b| {
        b.iter(|| timeout_cycle(FIELD_DEADLINE));
    });
    group.bench_function("timeout_2ms_cycle_1t", |b| {
        b.iter(|| timeout_cycle(SHORT_DEADLINE));
    });
    group.bench_function("sqz_oneshot_cycle", |b| {
        b.iter(sqz_oneshot_cycle);
    });
    group.bench_function("futures_oneshot_cycle", |b| {
        b.iter(futures_oneshot_cycle);
    });
    group.bench_function("crossbeam_lane_try_send_recv", |b| {
        let (tx, rx) = crossbeam::channel::bounded::<u64>(4096);
        b.iter(|| {
            tx.try_send(black_box(7)).expect("room");
            assert_eq!(rx.try_recv().expect("queued"), 7);
        });
    });
    group.bench_function("deadline_stamp_cycle", |b| {
        let mut slot = None;
        b.iter(|| deadline_stamp_cycle(&mut slot, FIELD_DEADLINE));
    });

    // Contention slope: N threads × PER_THREAD cycles against the ONE
    // registry lock (the handler-lane population at the field rate).
    for &(threads, d, label) in &[
        (8usize, FIELD_DEADLINE, "timeout_30s_cycle_8t"),
        (32, FIELD_DEADLINE, "timeout_30s_cycle_32t"),
        (8, SHORT_DEADLINE, "timeout_2ms_cycle_8t"),
        (32, SHORT_DEADLINE, "timeout_2ms_cycle_32t"),
    ] {
        group.throughput(criterion::Throughput::Elements(
            (threads * PER_THREAD) as u64,
        ));
        group.bench_function(label, |b| {
            b.iter(|| {
                let hs: Vec<_> = (0..threads)
                    .map(|_| {
                        std::thread::spawn(move || {
                            for _ in 0..PER_THREAD {
                                timeout_cycle(d);
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.join().unwrap();
                }
            });
        });
    }
    // The stamp under the same thread population: no shared word at all.
    group.throughput(criterion::Throughput::Elements((32 * PER_THREAD) as u64));
    group.bench_function("deadline_stamp_cycle_32t", |b| {
        b.iter(|| {
            let hs: Vec<_> = (0..32)
                .map(|_| {
                    std::thread::spawn(move || {
                        let mut slot = None;
                        for _ in 0..PER_THREAD {
                            deadline_stamp_cycle(&mut slot, FIELD_DEADLINE);
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_op_trace_hook,
    bench_reclaim_enqueue,
    bench_sharded_counters,
    bench_op_registry,
    bench_tier_get_contended,
    bench_high_concurrency,
    bench_cluster_dlm,
    bench_s9_custody,
    bench_metadata_clone,
    bench_dynamic_meta_routing,
    bench_error_paths,
    bench_writeback_latch,
    bench_read_mostly_cache,
    // Last: its 4 M-entry resident heap load outlives the group.
    bench_read_fill_executor_prims
);
criterion_main!(benches);
