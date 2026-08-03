//! Criterion micro-benches for the write-path hot machinery — the
//! microbench program (2026-08-04, `.benchmarks/2026-08-04-microbench-program.md`).
//!
//! Field-shape sources:
//! * coverage-union `record_write` — RW3b / the FIND-L1-A fix
//!   (`.benchmarks/2026-07-17-rand-write-program-closing.md`; pinned by
//!   `tests/write_through_coverage_tests.rs`): block completeness is the
//!   ACCUMULATED union, and kernel-split out-of-order O_DIRECT segments
//!   are the NORMAL case (the instrument-alignment lesson: an unaligned
//!   1 MiB buffer splits into 2 concurrent out-of-order FUSE WRITEs).
//!   Shapes: 4 MiB block covered by four in-order 1 MiB writes (the
//!   elbencho/fio seq shape) and by 32 × 128 KiB segments arriving
//!   even-then-odd (worst-case extras-run fragmentation → coalesce).
//! * extent overlay park/fold — design-random-small-writes §5.2 (W2):
//!   16 × 4 KiB parked extents (the `fold_fill` median ≥ 16 amortization
//!   gate) folded seed-once via `fill_complement_from`.
//! * supersession snapshot + CoW — the Idea-2 stamp
//!   (`ActiveBlockBuf::write_epoch`, snapshot-under-lock / revalidate-
//!   before-merge) and the `BLOCK_FLUSH_LOCKS` stripe route every block
//!   mutation pays (P1-9 order, `src/stripe_locks.rs`).
//! * layout publish encode — the write-commit-economy campaign
//!   (`.benchmarks/2026-07-30-write-commit-economy.md`): the convicted
//!   O(file-size)-per-publish full-save (18.2 KiB mean journal entry
//!   field-wide) vs the O(batch) `LayoutDelta` (batch = 64, the
//!   `SQUEEZEFS_PUBLISH_COALESCE_MAX` default) and the fold-side
//!   `apply` (delta onto a 1,024-block base — a 4 GiB file at 4 MiB
//!   blocks).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use squeezefs::cache::active_block::ActiveBlockBuf;
use squeezefs::fuse_client::BLOCK_FLUSH_LOCKS;
use squeezefs::layout_wire::{encode_layout, LayoutDelta, LayoutMetadata, LayoutMetadataRef};
use std::collections::HashMap;
use std::hint::black_box;

const BLOCK: usize = 4 * 1024 * 1024;

/// DUR-2: the price of the data-device durability barrier, and what the
/// `SyncCoalescer` takes back.
///
/// Field shape: `fsync`-heavy mixed workloads issue many concurrent
/// per-inode barriers against ONE data device (the D5 group-commit
/// conveyor's shape on the metadata side — same discipline, reused here
/// rather than re-derived). The lever this group prices is the coalescing
/// WIDTH: N concurrent callers must cost far fewer than N device
/// barriers, and the counters `data_device_sync_requests` /
/// `data_device_syncs` are the live instrument for the same ratio.
///
/// Substrate note (honesty): a Criterion box measures this against a
/// file-backed volume, where `Fsync/DATASYNC` is an `fdatasync` on the
/// host filesystem. On a real VWC-enabled NVMe device the per-barrier
/// cost is a device flush — larger, and exactly why the width matters.
/// Correctness is NOT gated on this number.
fn bench_flush_coalescing(c: &mut Criterion) {
    use squeezefs::fuse_client::METRICS;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    let rt = Runtime::new().expect("bench runtime");
    let backing = tempfile::NamedTempFile::new().expect("bench backing file");
    backing
        .as_file()
        .set_len(64 * 1024 * 1024)
        .expect("size backing file");
    let dev = Arc::new(NvmeBlockDev::new(
        backing.path().to_str().expect("backing path"),
    ));
    // One durable-ish write so the barrier has something to order.
    rt.block_on(async {
        dev.write_block(0, bytes::Bytes::from(vec![0x5Au8; 4096]))
            .await
            .expect("seed write");
    });

    let mut group = c.benchmark_group("data_device_barrier");

    group.bench_function("flush_serial", |b| {
        let dev = dev.clone();
        b.to_async(&rt).iter(|| {
            let dev = dev.clone();
            async move {
                dev.flush().await.expect("barrier");
                black_box(&dev);
            }
        });
    });

    // N concurrent callers per iteration: the coalescing width. The
    // reported time is per BATCH; barriers-per-batch is printed once so
    // the width is legible next to the cost.
    for n in [8usize, 64] {
        group.bench_function(format!("flush_concurrent_{n}"), |b| {
            let dev = dev.clone();
            b.to_async(&rt).iter(|| {
                let dev = dev.clone();
                async move {
                    let mut set = Vec::with_capacity(n);
                    for _ in 0..n {
                        let d = dev.clone();
                        set.push(tokio::spawn(async move { d.flush().await }));
                    }
                    for h in set {
                        h.await.expect("join").expect("barrier");
                    }
                }
            });
        });

        // Width probe (one measured batch, outside the timing loop).
        let syncs0 = METRICS.data_device_syncs.load(Ordering::Relaxed);
        rt.block_on(async {
            let mut set = Vec::with_capacity(n);
            for _ in 0..n {
                let d = dev.clone();
                set.push(tokio::spawn(async move { d.flush().await }));
            }
            for h in set {
                h.await.expect("join").expect("barrier");
            }
        });
        let syncs = METRICS.data_device_syncs.load(Ordering::Relaxed) - syncs0;
        println!("  [coalescing width] {n} concurrent flushes -> {syncs} device barriers");
    }

    group.finish();
}

fn bench_coverage_union(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_coverage_union");
    group.throughput(Throughput::Bytes(BLOCK as u64));

    // In-order: the seq-write fast path (primary-run extension ×4).
    group.bench_function("record_write_4x1m_inorder", |b| {
        b.iter_batched(
            || ActiveBlockBuf::fresh(BLOCK),
            |mut buf| {
                let mut fired = false;
                for i in 0..4usize {
                    fired |= buf.record_write(i * 1024 * 1024, (i + 1) * 1024 * 1024);
                }
                assert!(fired, "union must complete exactly once");
                buf
            },
            BatchSize::PerIteration,
        );
    });

    // Kernel-split OOO: 32 × 128 KiB, all even segments then all odd —
    // 16 disjoint extras runs built, then bridged (the coalesce path).
    group.bench_function("record_write_32x128k_ooo", |b| {
        const SEG: usize = 128 * 1024;
        b.iter_batched(
            || ActiveBlockBuf::fresh(BLOCK),
            |mut buf| {
                let mut fired = false;
                for i in (0..32usize).step_by(2) {
                    fired |= buf.record_write(i * SEG, (i + 1) * SEG);
                }
                for i in (1..32usize).step_by(2) {
                    fired |= buf.record_write(i * SEG, (i + 1) * SEG);
                }
                assert!(fired, "union must complete exactly once");
                buf
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_extent_overlay(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_extent_overlay");
    let chunk = vec![0xE5u8; 4096];

    // Park 16 disjoint 4 KiB extents (coverage + slab merge, in lockstep).
    group.throughput(Throughput::Bytes(16 * 4096));
    group.bench_function("park_16x4k", |b| {
        b.iter_batched(
            || ActiveBlockBuf::extent(BLOCK, true),
            |mut buf| {
                for i in 0..16usize {
                    // 64 KiB stride: disjoint, non-abutting runs.
                    black_box(buf.merge_extent(i * 64 * 1024, &chunk));
                }
                buf
            },
            BatchSize::PerIteration,
        );
    });

    // The fold's RAM cost: escalate the parked overlay to a full backing
    // and seed the complement from the old block image (`fold_fill` = 16
    // extents amortized over ONE seed).
    let seed = vec![0x11u8; BLOCK];
    group.throughput(Throughput::Bytes(BLOCK as u64));
    group.bench_function("fold_escalate_seed_16x4k", |b| {
        b.iter_batched(
            || {
                let mut buf = ActiveBlockBuf::extent(BLOCK, true);
                for i in 0..16usize {
                    buf.merge_extent(i * 64 * 1024, &chunk);
                }
                buf
            },
            |mut buf| {
                buf.fill_complement_from(black_box(&seed));
                buf
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_supersession(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_supersession");

    // Snapshot mint while the buffer is quiescent (the upload-capture arm).
    let mut buf = ActiveBlockBuf::fresh(BLOCK);
    buf.record_write(0, BLOCK);
    buf.make_mut()[..8].copy_from_slice(b"benchpay");
    group.throughput(Throughput::Elements(1));
    group.bench_function("snapshot_mint", |b| {
        b.iter(|| black_box(buf.snapshot()).len());
    });

    // Snapshot + writer CoW: a live snapshot (the in-flight upload)
    // forces the next merge's `make_mut` to copy the whole block — the
    // priced supersession cost when a writer lands mid-upload.
    group.throughput(Throughput::Bytes(BLOCK as u64));
    group.bench_function("snapshot_then_cow_make_mut_4m", |b| {
        b.iter(|| {
            let snap = buf.snapshot();
            let slice = buf.make_mut(); // CoW: snapshot alive
            black_box(slice[0]);
            drop(snap);
        });
    });

    // The BLOCK_FLUSH_LOCKS stripe route + uncontended acquire every
    // block mutation pays (P1-9 lock order, level 3).
    group.throughput(Throughput::Elements(1));
    group.bench_function("block_flush_stripe_route_trylock", |b| {
        let mut i = 0u64;
        b.iter(|| {
            i = i.wrapping_add(0x9E37_79B9);
            let lock = BLOCK_FLUSH_LOCKS.get_lock(black_box(i), (i >> 32) as u32);
            let guard = lock.try_lock().expect("uncontended stripe");
            drop(guard);
        });
    });

    group.finish();
}

/// RES-1 (pre-RC engineering spec §7): the price of hoisting terminal
/// frees OUT of the `INODE_META_LOCKS` (level 3.5) critical section.
///
/// `BackendRouter::free_block`'s at-cap reclaim enqueue parks up to
/// `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000) **per key**, so every
/// layout commit now COLLECTS its displaced keys under the guard and
/// frees them after it drops. What the collect costs is exactly this
/// group; what it buys is `keys × cap_park_ms` of a per-inode stripe
/// nobody else can take.
///
/// Field-shape sources:
/// * `epoch_displaced_drain` — `close_rewrite_epoch`'s `displaced`
///   `SegQueue` (design-rewrite-program §5, KD-1.4): one displaced A key
///   per rewritten block. 1 = a lone RMW; 64 = the
///   `SQUEEZEFS_PUBLISH_COALESCE_MAX` window
///   (`.benchmarks/2026-07-30-write-commit-economy.md`); 1,024 = a 4 GiB
///   file's whole map at 4 MiB blocks (the truncate-to-0 / full-rewrite
///   epoch — the shape whose in-lock free loop could hold the stripe for
///   ~17 minutes at the default park bound).
/// * `superseded_collect` — `release_superseded_staged`'s dedup pass over
///   the last-published map (staged layouts carry `block_map[0]`; the
///   striped-promotion arm carries the whole map).
fn bench_deferred_free_collect(c: &mut Criterion) {
    use std::collections::HashSet;

    let mut group = c.benchmark_group("write_deferred_free");

    let key = |b: u32| format!("sqz:vol-00aa11bb:blk_{b:012x}_0000");

    for &n in &[1usize, 64, 1024] {
        // The close's drain: SegQueue<String> → Vec<String>, which is
        // what replaced the in-lock `while let Some(k) = pop { free(k) }`.
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("epoch_displaced_drain_{n}"), |b| {
            b.iter_batched(
                || {
                    let q = crossbeam::queue::SegQueue::new();
                    for i in 0..n as u32 {
                        q.push(key(i));
                    }
                    q
                },
                |q| {
                    let mut deferred = Vec::new();
                    while let Some(k) = q.pop() {
                        deferred.push(k);
                    }
                    black_box(deferred.len())
                },
                BatchSize::SmallInput,
            );
        });
    }

    // The superseded-release dedup pass (unchanged in shape — only its
    // `free_block` await moved out): iterate the published map, skip the
    // kept key, dedup, collect.
    for &n in &[1usize, 1024] {
        let map: HashMap<u32, String> = (0..n as u32).map(|b| (b, key(b))).collect();
        let keep = key(0);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("superseded_collect_{n}"), |b| {
            b.iter(|| {
                let mut seen = HashSet::new();
                let mut deferred = Vec::new();
                for bk in black_box(&map).values() {
                    if bk.as_str() == keep || !seen.insert(bk.clone()) {
                        continue;
                    }
                    deferred.push(bk.clone());
                }
                black_box(deferred.len())
            });
        });
    }

    group.finish();
}

/// RES-8 (pre-RC engineering spec §7): the price of the unwind-catching
/// detached-spawn wrapper on the write ACK path.
///
/// The completing WRITE replies with the block's custody parked and the
/// durable upload rides a detached handler-lane task — one per
/// coverage-complete block, so this wrapper is paid at the block rate of
/// the write path (the field's rewrite rows displace ~1,650 blocks/s per
/// `.benchmarks/2026-07-31-write-wall.md`). What it costs is one
/// `catch_unwind` frame around the body; what it buys is that a panic
/// there is COUNTED instead of vanishing into an under-reported phase
/// histogram.
fn bench_detached_guard(c: &mut Criterion) {
    use tokio::runtime::Runtime;

    let rt = Runtime::new().expect("bench runtime");
    let mut group = c.benchmark_group("write_detached_guard");
    group.throughput(Throughput::Elements(1));

    // Baseline: the bare body, awaited (the pre-RES-8 shape minus the
    // spawn, which both arms pay identically).
    group.bench_function("bare_body", |b| {
        b.iter(|| {
            rt.block_on(async {
                black_box(async { black_box(0u64) }.await);
            })
        });
    });

    // The guarded body: `contain` = catch_unwind + the (not-taken)
    // counting arm.
    group.bench_function("contained_body", |b| {
        b.iter(|| {
            rt.block_on(async {
                squeezefs::detached::contain("bench", async {
                    black_box(0u64);
                })
                .await;
            })
        });
    });

    group.finish();
}

/// **PERF-13 · the write-pipeline admission gate.**
///
/// Every coverage-complete block passes `WritePipeline::admit` before its
/// WRITE ACKs, so this gate is on the write path at the block rate (the
/// field's rewrite rows displace ~1,650 blocks/s per
/// `.benchmarks/2026-07-31-write-wall.md`, and a saturated ingest fleet
/// parks here by design — `write_pipeline_admission_waits`).
///
/// Two shapes, because PERF-13 moves cost between them:
/// * `admit_release_open_pipe` — the pipe below target (the common case).
///   The registration is deliberately AFTER this attempt, so this arm must
///   show no regression from the fix (no `Notify` wait-list traffic).
/// * `park_wake_cycle_8_writers` — 8 concurrent writers against a 2-block
///   target, i.e. the saturated shape where admissions actually park. This
///   is the arm the fix is for: pre-fix, a completion landing between a
///   Full re-check and the park's first poll was lost and the writer
///   resumed only on the 5 ms liveness tick.
fn bench_admission_gate(c: &mut Criterion) {
    use squeezefs::write_pipeline::{set_depth_override, WritePipeline};
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    const BS: u64 = 4 * 1024 * 1024;
    let rt = Runtime::new().expect("bench runtime");
    let mut group = c.benchmark_group("write_admission_gate");
    group.throughput(Throughput::Elements(1));

    // Open pipe: admit + release, no park.
    set_depth_override(Some(64));
    let pipe = WritePipeline::with_caps(Arc::new(|| false), Some(1 << 40));
    group.bench_function("admit_release_open_pipe", |b| {
        b.iter(|| {
            rt.block_on(async {
                let p = pipe.admit(black_box(BS)).await;
                black_box(&p);
                drop(p);
            })
        });
    });

    // Saturated: 8 writers, 2-block target — every writer parks and is
    // resumed by a completion wake.
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 16;
    set_depth_override(Some(2));
    let saturated = WritePipeline::with_caps(Arc::new(|| false), Some(1 << 40));
    group.throughput(Throughput::Elements((WRITERS * PER_WRITER) as u64));
    group.bench_function("park_wake_cycle_8_writers", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut set = tokio::task::JoinSet::new();
                for _ in 0..WRITERS {
                    let pipe = saturated.clone();
                    set.spawn(async move {
                        for _ in 0..PER_WRITER {
                            let p = pipe.admit(BS).await;
                            // Hold custody across scheduler turns so the
                            // pipe stays AT target (the parking shape).
                            for _ in 0..4 {
                                tokio::task::yield_now().await;
                            }
                            drop(p);
                        }
                    });
                }
                while let Some(r) = set.join_next().await {
                    r.expect("writer task");
                }
            })
        });
    });
    set_depth_override(None);

    group.finish();
}

fn bench_layout_publish(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_layout_publish");

    // A 4 GiB file's inline map: 1,024 blocks × ~40-char keys.
    let base_map: HashMap<u32, String> = (0..1024u32)
        .map(|b| (b, format!("sqz:vol-00aa11bb:blk_{b:012x}_0000")))
        .collect();
    let base_layout = LayoutMetadata {
        file_type: "striped".to_string(),
        size: 4 << 30,
        block_map_id: Some("bm-00aa11bb".to_string()),
        block_prefix: Some("sqz:vol-00aa11bb".to_string()),
        file_id: None,
        data_key: None,
        block_map: Some(base_map),
    };
    let base_bytes = encode_layout(&base_layout).expect("encode base");

    // The pre-campaign per-publish price: re-encode the ENTIRE layout.
    group.throughput(Throughput::Bytes(base_bytes.len() as u64));
    group.bench_function("full_save_encode_1024_blocks", |b| {
        b.iter(|| black_box(encode_layout(black_box(&base_layout)).expect("encode")));
    });

    // **PERF-8 · the save path's O(file-size) passes.**
    //
    // Every layout save (publish-class or not) used to build an OWNED
    // `LayoutMetadata` first, which clones the whole block map — one heap
    // allocation PER BLOCK — purely to hand bincode something to walk, and
    // then serialized it whole even on the indirect arm that throws those
    // bytes away. The three arms below price the two removed passes against
    // the one that remains:
    //
    // * `owned_clone_then_encode_1024` — the pre-fix shape (map clone +
    //   encode).
    // * `borrowed_view_encode_1024` — the shipped shape (encode only, from
    //   the live map).
    // * `borrowed_view_size_probe_1024` — the `needs_indirect` decision
    //   input alone: what the indirect arm now pays instead of a full
    //   encode it discards.
    let live_map = base_layout.block_map.as_ref().expect("map").clone();
    group.bench_function("owned_clone_then_encode_1024", |b| {
        b.iter(|| {
            let owned = LayoutMetadata {
                file_type: "striped".to_string(),
                size: 4 << 30,
                block_map_id: Some("bm-00aa11bb".to_string()),
                block_prefix: Some("sqz:vol-00aa11bb".to_string()),
                file_id: None,
                data_key: None,
                block_map: Some(black_box(&live_map).clone()),
            };
            black_box(encode_layout(&owned).expect("encode"))
        });
    });
    group.bench_function("borrowed_view_encode_1024", |b| {
        b.iter(|| {
            let view = LayoutMetadataRef {
                file_type: "striped",
                size: 4 << 30,
                block_map_id: Some("bm-00aa11bb"),
                block_prefix: Some("sqz:vol-00aa11bb"),
                file_id: None,
                data_key: None,
                block_map: Some(black_box(&live_map)),
            };
            black_box(view.encode().expect("encode"))
        });
    });
    group.bench_function("borrowed_view_size_probe_1024", |b| {
        b.iter(|| {
            let view = LayoutMetadataRef {
                file_type: "striped",
                size: 4 << 30,
                block_map_id: Some("bm-00aa11bb"),
                block_prefix: Some("sqz:vol-00aa11bb"),
                file_id: None,
                data_key: None,
                block_map: Some(black_box(&live_map)),
            };
            black_box(view.encoded_len().expect("size"))
        });
    });

    // The shipped O(batch) delta: 64 map inserts (the
    // SQUEEZEFS_PUBLISH_COALESCE_MAX default window).
    let delta = LayoutDelta::from_final_state(
        "striped",
        (4 << 30) + (64 << 22),
        Some("bm-00aa11bb"),
        Some("sqz:vol-00aa11bb"),
        None,
        None,
        (1024..1088u32)
            .map(|b| (b, format!("sqz:vol-00aa11bb:blk_{b:012x}_0000")))
            .collect(),
    );
    let delta_bytes = delta.encode();
    group.throughput(Throughput::Bytes(delta_bytes.len() as u64));
    group.bench_function("delta_encode_64_entries", |b| {
        b.iter(|| black_box(delta.encode()));
    });
    group.bench_function("delta_decode_64_entries", |b| {
        b.iter(|| black_box(LayoutDelta::decode(black_box(&delta_bytes)).expect("decode")));
    });

    // The fold side: delta onto the 1,024-block base (decode base +
    // 64 inserts + canonical re-encode) — replay / read-fold price.
    group.throughput(Throughput::Bytes(base_bytes.len() as u64));
    group.bench_function("delta_apply_64_on_1024_base", |b| {
        b.iter(|| black_box(delta.apply(black_box(&base_bytes)).expect("apply")));
    });

    group.finish();
}

/// **Block-key mint and parse** — pre-RC engineering spec §6.2 **item 6**
/// (block keys are bare reusable device offsets, which makes a stale map
/// binding structurally UNDETECTABLE — §6.3's serve proof rests on two
/// process-local premises).
///
/// Every one of these is on a hot path: the mint runs once per published
/// block, and the parse runs on **every publish and every read serve**
/// (`BackendRouter::read_block_with_dest` / `read_block_range` /
/// `free_block` / `increment_refcount` / the durable-refcount resolution —
/// one shared extraction path). The `_bare` rows are the shipped
/// (un-stamped) forms and are the numbers the `offset ‖ incarnation` key
/// must not move; ruling D9 keeps the bit un-stamped, so bare IS the
/// shipped path.
///
/// **No numbers yet — ruling D11** ("no cargo test, benches or release gate
/// yet until we are done implementing the DLM and can have N Readers and
/// Writers"): this group is coverage that must EXIST, and its measurement
/// is deferred to the first post-DLM window. Stated so the deferred run has
/// something to refute:
///
/// * **prediction** — every `*_bare*` row lands within the harness's own
///   group threshold of its pre-item-6 value (the added work on those rows
///   is one relaxed load in `persist_block_key` and no added branch in the
///   parse's success path), and each `*_stamped*` row costs strictly more
///   than its bare twin but by a constant (one map read; one ≤ 13-char
///   base-36 render; one `@` split on the parse);
/// * **falsification** — a `*_bare*` row past the threshold (the item-6
///   gate leaked onto the shipped path — hoist the gate to the publish
///   batch, never drop the lifetime), or a `*_stamped*` row scaling with
///   key length rather than sitting at a constant delta (the codec is
///   allocating or re-scanning per digit).
///
/// FIELD shapes: the default-slot bare offset (`4194304` — what
/// `persist_block_key` emits for every single-volume filesystem), the
/// named-volume form (`vol-00aa11bb://4194304` — every multi-volume set),
/// and the decorated 3-part form (`…:0:4194304`, promoted staged / spill /
/// clip publishes, the W1-ineligible population).
fn bench_block_key_codec(c: &mut Criterion) {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::meta_backend::kv::journal::AppendPartition;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::{
        block_key_with_incarnation, clean_block_key, is_whole_block_mapping, BackendRouter,
    };
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    let rt = Runtime::new().unwrap();
    let backing = tempfile::NamedTempFile::new().expect("backing");
    let (alloc, router) = rt.block_on(async {
        let alloc = Arc::new(
            BlockAllocator::new("bench_block_key")
                .await
                .expect("allocator"),
        );
        let dev = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
        let router = BackendRouter::new(alloc.clone(), dev, Arc::new(AtomicU64::new(BLOCK as u64)));
        (alloc, router)
    });
    alloc.set_capacity_bytes(64 * 1024 * 1024 * 1024);

    let mut group = c.benchmark_group("write_block_key");
    group.throughput(Throughput::Elements(1));

    group.bench_function("persist_bare_default_slot", |b| {
        b.iter(|| black_box(router.persist_block_key(black_box("backend_0"), black_box(4 << 20))))
    });
    group.bench_function("parse_bare_default_slot", |b| {
        b.iter(|| black_box(router.parse_block_key(black_box("4194304")).expect("parse")))
    });
    group.bench_function("parse_bare_named_volume", |b| {
        b.iter(|| {
            black_box(
                router
                    .parse_block_key(black_box("vol-00aa11bb://4194304"))
                    .expect("parse"),
            )
        })
    });
    group.bench_function("clean_bare_decorated", |b| {
        b.iter(|| {
            black_box(clean_block_key(black_box(
                "vol-00aa11bb://4194304:0:4194304",
            )))
        })
    });
    group.bench_function("whole_block_predicate_bare", |b| {
        b.iter(|| black_box(is_whole_block_mapping(black_box("vol-00aa11bb://4194304"))))
    });

    // ---- The ENGAGED (incompat bit 13) forms: what a Phase-8-stamped
    // volume pays. Nothing stamps the bit today, so these are the cost of
    // the upgrade, not of the shipped path.
    router
        .engage_incarnation_keys(7, AppendPartition::SOLO)
        .expect("engage era 7");
    let stamped_offset = rt.block_on(async { alloc.allocate_block().await.expect("allocate") });
    let stamped_key = router.persist_block_key("backend_0", stamped_offset);
    let stamped_named = block_key_with_incarnation(
        "vol-00aa11bb://4194304",
        squeezefs::routing::compose_incarnation(7, 4_242).expect("stamp"),
    );
    group.bench_function("persist_stamped_default_slot", |b| {
        b.iter(|| {
            black_box(router.persist_block_key(black_box("backend_0"), black_box(stamped_offset)))
        })
    });
    group.bench_function("parse_stamped_default_slot", |b| {
        b.iter(|| {
            black_box(
                router
                    .parse_block_key_parts(black_box(&stamped_key))
                    .expect("parse"),
            )
        })
    });
    group.bench_function("parse_stamped_named_volume", |b| {
        b.iter(|| {
            black_box(
                router
                    .parse_block_key_parts(black_box(&stamped_named))
                    .expect("parse"),
            )
        })
    });
    // The read/free path's validation: the offset's live lifetime matches
    // the key's (the only outcome a healthy mount ever produces).
    group.bench_function("validate_stamped_hit", |b| {
        b.iter(|| black_box(router.block_key_incarnation_ok(black_box(&stamped_key))))
    });
    let stamped_decorated = format!("{stamped_named}:0:4194304");
    group.bench_function("clean_stamped_decorated", |b| {
        b.iter(|| black_box(clean_block_key(black_box(&stamped_decorated))))
    });
    // The key-BYTE delta — STRUCTURAL, not timed: it is what a layout
    // publish pays per map entry (the journal-byte term the
    // write-commit-economy campaign collapsed), and it is deterministic, so
    // it is reportable under ruling D11 while the ns/op rows are not.
    let stamped_inc = squeezefs::routing::block_key_incarnation(&stamped_key)
        .expect("a stamped key names a lifetime");
    println!(
        "  [key bytes] bare {} -> stamped {} ({:+} B/entry, era {} seq {})",
        stamped_offset.to_string().len(),
        stamped_key.len(),
        stamped_key.len() as i64 - stamped_offset.to_string().len() as i64,
        squeezefs::routing::incarnation_era(stamped_inc),
        stamped_inc & squeezefs::routing::INCARNATION_SEQ_MAX,
    );

    group.finish();
}

/// **DUR-6 · the indirect block-map blob's encode + digest.** The spill
/// path re-serializes the WHOLE map per publish and now checksums it, and
/// since the same campaign made the publish copy-on-write, that cost is
/// paid on every indirect publish — commit-adjacent by construction.
///
/// Field shapes, both from `tests/indirect_map_backend_keys_tests.rs`
/// (which derives them from the real spill boundary): 700 entries — the
/// first map that spills past the 64 KiB-node inline cap — and 4,096
/// entries, a ~16 GiB file at the shipped 4 MiB block size.
fn bench_indirect_map_codec(c: &mut Criterion) {
    use squeezefs::routing::{decode_indirect_block_map, encode_indirect_block_map};

    let mut group = c.benchmark_group("write_indirect_map");
    for entries in [700usize, 4096] {
        let map: HashMap<u32, String> = (0..entries as u32)
            .map(|b| (b, format!("vol-00aa11bb://{}", (b as u64) * (4 << 20))))
            .collect();
        let img = encode_indirect_block_map(&map).expect("encode");
        group.throughput(Throughput::Bytes(img.len() as u64));
        group.bench_function(format!("encode_checksum_{entries}"), |b| {
            b.iter(|| black_box(encode_indirect_block_map(black_box(&map)).expect("encode")));
        });
        group.bench_function(format!("verify_decode_{entries}"), |b| {
            b.iter(|| black_box(decode_indirect_block_map(black_box(&img)).expect("decode")));
        });
    }
    group.finish();
}

/// **RES-17** (pre-RC spec §7): staging-shard removal under the shard
/// WRITE lock.
///
/// Field shape: the staging segment ring holds one live entry per staged
/// object / active block. Removal happens on every flush-promotion,
/// same-key replace and eviction victim — i.e. on the write path — and the
/// pre-fix implementation maintained a `VecDeque<Bytes>` beside the
/// authoritative map, so each removal paid `retain` = an O(n) equality
/// scan over `Bytes` keys plus an O(n) element shift, holding the shard's
/// write lock throughout. Occupancy is what makes that quadratic: the
/// shard sizes come from the R5 staging budget, so a 128 MiB shard holding
/// 4 KiB-class staged extents carries thousands of live keys.
///
/// This group **drains** an `occupancy`-key shard and reports per-key cost.
/// That is the honest shape for the claim: with the queue, draining N keys
/// from an N-occupancy shard was O(N²) (each `retain` scans and shifts the
/// remaining queue); map-only it is O(N), so the **per-key** figure must be
/// flat across occupancies.
///
/// Measurement notes (both learned the hard way on this row): the shard is
/// RETURNED from the routine, because `iter_batched` drops outputs outside
/// the measured region and dropping it inline times the shard's
/// occupancy-sized mmap teardown instead of the removals; and the drain is
/// per-batch rather than one-key-per-iteration, because a per-iteration
/// batch holds many live mappings at once and charges their page-fault cost
/// to the removal.
fn bench_staging_shard_removal(c: &mut Criterion) {
    use bytes::Bytes;
    use squeezefs::tiering::nvme::NvmeCache;

    let mut group = c.benchmark_group("staging_shard_removal");
    // Value size = the W2 parked-extent class (4 KiB), the shape that
    // actually produces high key counts per shard.
    const VAL: usize = 4096;
    for occupancy in [64usize, 1024, 4096] {
        let cap = (occupancy * (VAL + 4096)) * 2;
        let keys: Vec<Bytes> = (0..occupancy)
            .map(|i| Bytes::from(format!("active_block:{i:08}:0")))
            .collect();
        group.throughput(Throughput::Elements(occupancy as u64));
        group.bench_function(format!("drain_{occupancy}_live_keys"), |b| {
            b.iter_batched(
                || {
                    let cache = NvmeCache::new(&[], &[cap], 1).expect("anon shard");
                    for k in &keys {
                        cache.put(k.clone(), Bytes::from(vec![0xa5u8; VAL]));
                    }
                    cache
                },
                |cache| {
                    let mut n = 0usize;
                    for k in &keys {
                        if cache.remove(black_box(k)).is_some() {
                            n += 1;
                        }
                    }
                    (cache, black_box(n))
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// **VAL-7e** (pre-RC spec §3): the `copy_file_range` staged-sibling probe
/// range.
///
/// Field shape: `cp`/`rsync`-class copies issue `copy_file_range` in
/// chunks against files whose size is unrelated to the chunk. The pre-fix
/// probe looped `0..blocks` over the whole SOURCE FILE — four times per
/// call — and pushed matches into an unbounded `Vec<u32>`; at the shipped
/// 4 MiB block a 1 TiB source is 262 144 iterations × 4 per call, each one
/// a formatted `active_block:` key plus a staged-key lookup. The bound is
/// the copied extent.
///
/// The two rows are the same CALL against the same 1 TiB file: the
/// per-chunk range (what the fix scans) and the whole-file range (what the
/// clone fast path legitimately needs, and what the pre-fix code did for
/// EVERY call).
fn bench_copy_probe_range(c: &mut Criterion) {
    use squeezefs::fuse_client::copy_probe_block_range;

    let bs = BLOCK as u64;
    let file = 1u64 << 40; // 1 TiB source
    let blocks = file.div_ceil(bs) as u32;

    let mut group = c.benchmark_group("copy_probe_range");
    // The arithmetic itself (must be free).
    group.bench_function("range_1mib_chunk", |b| {
        b.iter(|| {
            black_box(copy_probe_block_range(
                black_box(512 * bs),
                black_box(1024 * 1024),
                black_box(blocks),
                black_box(bs),
            ))
        });
    });
    // The probe LOOP the range governs: key formatting per block is the
    // real per-iteration cost, so the row prices the work the bound
    // removes. `chunk` = the fix's scan, `whole_file` = the pre-fix scan
    // for the same request.
    for (label, (lo, hi)) in [
        (
            "loop_chunk",
            copy_probe_block_range(512 * bs, 1024 * 1024, blocks, bs),
        ),
        ("loop_whole_file", (0u32, 4096u32)),
    ] {
        group.throughput(Throughput::Elements((hi - lo).max(1) as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let mut n = 0usize;
                for blk in lo..hi {
                    let key = squeezefs::keys::active_block(42, blk as u64).to_string();
                    n += key.len();
                }
                black_box(n)
            });
        });
    }
    group.finish();
}

/// Writer-scoped staging keys — spec §6.2 items 8/10.
///
/// Field shape: staging key mint is on the small-write path (the W1/W2
/// predicates and the §5.5.1 warm serve prelude mint one per op — the
/// op-economy campaign's zero-heap `StackKey` exists for exactly that),
/// and record classification is on the MOUNT path (one pass over every
/// recovered staging key: the field's kill-9 residue population, sized
/// here at 4,096 records ≈ a 16 GiB dirty ring at 4 MiB blocks).
///
/// The rows that matter: `disengaged_*` is the SOLO case — every shipped
/// volume, since ruling D9 stamps nothing — and must be at par with the
/// pre-change mint (one relaxed-load gate + a predicted branch);
/// `engaged_*` prices what a writer-scoped set pays.
fn bench_writer_scope(c: &mut Criterion) {
    use squeezefs::writer_scope as ws;

    let mut group = c.benchmark_group("writer_scope");
    // The A/B control for "the solo case must not regress": the PRE-change
    // mint, byte-for-byte (the historical body, no scope hook), against
    // `disengaged_mint_heap` (the shipped path — ruling D9 stamps nothing).
    group.bench_function("prechange_mint_heap", |b| {
        b.iter(|| {
            use std::fmt::Write as _;
            let (ino, block): (u64, u64) = (black_box(4242), black_box(7));
            let mut s = compact_str::CompactString::with_capacity(40);
            let _ = write!(s, "active_block:inode_{ino}:block_{block}");
            black_box(s)
        });
    });
    for (label, scope) in [
        ("disengaged", None),
        ("engaged", Some(0x0123_4567_89ab_cdefu64)),
    ] {
        ws::engage(scope);
        // Mint: heap (the layout/flush paths) and zero-heap stack (the
        // warm serve prelude / W1 predicate probes).
        group.bench_function(format!("{label}_mint_heap"), |b| {
            b.iter(|| black_box(squeezefs::keys::active_block(black_box(4242), black_box(7))));
        });
        group.bench_function(format!("{label}_mint_stack"), |b| {
            b.iter(|| {
                black_box(squeezefs::keys::active_block_stack(
                    black_box(4242),
                    black_box(7),
                ))
            });
        });
        group.bench_function(format!("{label}_mint_ext_heap"), |b| {
            b.iter(|| {
                black_box(squeezefs::keys::active_block_ext(
                    black_box(4242),
                    black_box(7),
                ))
            });
        });
        // Parse: every historical `(ino, block)` parser now strips the
        // scope first (`strip_key_scope`), on the fold/flush/reclaim paths.
        let key = squeezefs::keys::active_block(4242, 7).to_string();
        group.bench_function(format!("{label}_strip_scope"), |b| {
            b.iter(|| black_box(ws::strip_key_scope(black_box(&key))));
        });
        group.bench_function(format!("{label}_classify"), |b| {
            b.iter(|| black_box(ws::classify_key(black_box(&key))));
        });

        // Mount-path classification sweep: the recovery pass's per-record
        // decision over a dirty-ring-sized key population.
        const RECORDS: usize = 4096;
        let keys: Vec<String> = (0..RECORDS)
            .map(|i| squeezefs::keys::active_block(4242, i as u64).to_string())
            .collect();
        group.throughput(Throughput::Elements(RECORDS as u64));
        group.bench_function(format!("{label}_recovery_scan_4096"), |b| {
            b.iter(|| {
                let mut mine = 0usize;
                for k in &keys {
                    if ws::key_is_mine(k) {
                        mine += 1;
                    }
                }
                black_box(mine)
            });
        });
    }
    ws::engage(None);

    // The item-10 root-level decision (once per staging root per mount).
    let set = "v3:00112233445566778899aabbccddeeff|v3:ffeeddccbbaa99887766554433221100";
    let scoped = ws::staging_generation(set, Some(0x0123_4567_89ab_cdef));
    group.bench_function("classify_generation_match", |b| {
        b.iter(|| {
            black_box(ws::classify_generation(
                black_box(&scoped),
                black_box(&scoped),
                black_box(Some(0x0123_4567_89ab_cdef)),
            ))
        });
    });
    group.finish();
}

/// **DLM S5 — the price a READER feature charges WRITERS** (pre-RC
/// engineering spec §6.8 items 1/6; contracts in
/// `tests/readonly_mount_tests.rs`).
///
/// The number that matters here is the one on the LEFT of the A/B: the
/// read-only gate is a single relaxed atomic load feeding a never-taken,
/// perfectly-predicted branch on every write-path ownership transition
/// (allocation, terminal free, the W1 sole-owner patch). A reader mount
/// must not tax the writer it reads behind, so this group prices the gate
/// on a WRITE mount (latch off — the shipped posture) against the same
/// call with the latch armed (the reader's refusal path, which short-
/// circuits before any map work and is therefore *faster*, not slower —
/// the honest shape of a gate that refuses early).
///
/// Field shape: `begin_patch_sole_owner` is the hottest gated site by two
/// orders of magnitude — W1 is the primary random-write path at 61–67 k
/// IOPS (`.benchmarks/2026-07-17-rand-write-program-closing.md`), i.e. one
/// gated call per patched 4 KiB write. `allocate_block` is once per 4 MiB
/// block. So W1 is the row that decides whether the latch is free.
///
/// The second group prices the item-5 purge-on-revalidation pass at the
/// census size a reader accumulates between two checkpoints: with a 50 ms
/// cadence and the measured 622 k–1.0 M IOPS il read ceiling
/// (`.benchmarks/2026-07-19-l4-interception-closing.md`), a reader's warm
/// block-key census is thousands of keys, and the pass runs at most once
/// per epoch that observed the writer advance.
fn bench_ro_gate(c: &mut Criterion) {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::fuse_client::{read_only_mount, set_read_only_mount};
    use squeezefs::nvme_dev::NvmeBlockDev;
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    let rt = Runtime::new().expect("bench runtime");
    let ba = rt.block_on(async { BlockAllocator::new("ro_gate_bench").await.unwrap() });
    ba.set_capacity_bytes(64 * 1024 * 1024 * 1024);
    let off = rt.block_on(async { ba.allocate_block().await.expect("seed allocation") });

    let mut group = c.benchmark_group("ro_gate");
    // The gate ITSELF, isolated: one relaxed load + the not-taken branch.
    // This is the number the "a reader feature must not tax writers"
    // claim rests on — it is what every gated write-path site added.
    group.bench_function("latch_probe", |b| {
        b.iter(|| black_box(read_only_mount()));
    });
    // THE row: the gated W1 predicate on a write mount (latch off).
    group.bench_function("w1_patch_predicate_write_mount", |b| {
        assert!(
            !read_only_mount(),
            "the write-mount row needs the latch off"
        );
        b.iter(|| {
            let ok = ba.begin_patch_sole_owner(black_box(off));
            ba.publish_block(off);
            black_box(ok)
        });
    });
    // The refusal path (a reader) — short-circuits before the incarnation
    // word is touched.
    group.bench_function("w1_patch_predicate_reader", |b| {
        set_read_only_mount(true);
        b.iter(|| black_box(ba.begin_patch_sole_owner(black_box(off))));
        set_read_only_mount(false);
    });
    group.finish();

    // Item 5: the purge-on-revalidation pass over a warm census.
    let backing = tempfile::NamedTempFile::new().expect("bench backing file");
    backing.as_file().set_len(64 * 1024 * 1024).expect("size");
    let dev = Arc::new(NvmeBlockDev::new(backing.path().to_str().expect("path")));
    let pba = Arc::new(rt.block_on(async { BlockAllocator::new("ro_purge_bench").await.unwrap() }));
    let cache = rt.block_on(async {
        TieredCache::new(
            Vec::new(),
            Some("256MB"),
            Some("64MB"),
            Some("64MB"),
            Some("64MB"),
            pba,
            dev,
            None,
        )
        .await
        .unwrap()
    });
    let payload = bytes::Bytes::from(vec![0u8; 4096]);
    let mut group = c.benchmark_group("ro_revalidate_purge");
    for keys in [1024usize, 8192] {
        group.throughput(Throughput::Elements(keys as u64));
        group.bench_function(format!("purge_census_{keys}"), |b| {
            b.iter_batched(
                || {
                    for i in 0..keys {
                        cache
                            .read_lru
                            .put(&format!("ro_purge_bench://{}", i * BLOCK), payload.clone());
                    }
                },
                |_| black_box(squeezefs::ro_coherence::purge_reader_block_keys(&cache)),
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// **Spec §6.8 item 3 — the freed-offset grace period's per-operation
/// cost** (`src/free_grace.rs`; contracts
/// `tests/reader_free_grace_tests.rs`).
///
/// This group exists because the gate is **per allocation and per terminal
/// free**, i.e. on the same ownership-transition path the `ro_gate` group
/// above prices, and the mount that must not pay for it is the shipped
/// default one (`SQUEEZEFS_MEMBERSHIP_BIND=off`, so no reader plane, so
/// `free_grace::armed()` is false forever).
///
/// **Field shape.** Two rows per arm, both derived from measured field
/// numbers rather than invented:
///
/// * the *unarmed* arm is the whole population of shipped mounts. Its
///   frequency is one call per 4 MiB block allocated and one per block
///   freed — at the write-wall campaign's 12.7 GB/s saturated ingest
///   (`.benchmarks/2026-07-28-ingest-economy.md`) that is ≈ 3,100
///   allocations + 3,100 frees per second per volume, and at the rewrite
///   shape every overwritten block pays both.
/// * the *armed* arm's ring depth is the field's own churn over one
///   acknowledgement cycle: 12.7 GB/s ÷ 4 MiB × ≈ 38 s ≈ 120 k entries,
///   which is exactly why `RING_CAP_FLOOR` is 131,072. The rows below
///   walk 1 k → 128 k so the FIFO's claim (O(1) defer, O(released)
///   harvest — *not* O(held), which is what a quarantine-shaped release
///   would have cost) is visible as a flat line rather than asserted.
///
/// **Prediction** (written, not measured — ruling D11 defers every number;
/// this is the falsifiable claim the bench exists to test):
///
/// 1. `defer_unarmed` and `harvest_unarmed` are **≤ 2 ns** and
///    indistinguishable from `ro_gate/latch_probe` — one relaxed load of
///    `ARMED` / of the ring's published length, feeding a not-taken branch.
/// 2. `defer_armed` is **≤ 60 ns** (an uncontended `parking_lot` lock, one
///    `VecDeque` push, one clock read, four relaxed counter adds) and
///    **flat across ring depth**.
/// 3. `harvest_armed_none_eligible` is **flat in ring depth** and within
///    2 ns of the unarmed row plus one lock+peek (≈ 25 ns): the front peek
///    decides, never a scan.
/// 4. `harvest_armed_all_eligible` is **linear in the number RELEASED**
///    with a per-offset cost ≈ that of one `DashSet::insert`, capped at
///    `HARVEST_BATCH` (64) per call regardless of depth.
///
/// **Falsification.** Any of these instead scaling with the number of HELD
/// entries falsifies the FIFO design and sends the ring back to a
/// different structure; `defer_armed` past ~60 ns would say the lock is
/// wrong for the free path (the next candidate being a per-CPU sharded
/// ring, at the cost of the exact label ordering the peek relies on); and
/// an unarmed row measurably above `latch_probe` falsifies the
/// "zero cost when unarmed" claim that the whole feature's default-mount
/// acceptability rests on.
fn bench_free_grace_gate(c: &mut Criterion) {
    use squeezefs::free_grace::{self, GraceRing};
    use squeezefs::membership::{
        JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    use std::sync::Arc;
    use std::time::Duration;

    const CHUNK: u64 = BLOCK as u64;

    // --- unarmed: the shipped default mount ---------------------------
    free_grace::reset_for_test();
    let ring = GraceRing::new(131_072);
    let mut group = c.benchmark_group("free_grace_unarmed");
    group.bench_function("defer_unarmed", |b| {
        b.iter(|| black_box(ring.defer(black_box(0), CHUNK)));
    });
    group.bench_function("harvest_unarmed", |b| {
        b.iter(|| black_box(ring.harvest(64).len()));
    });
    group.finish();

    // --- armed: an owner with one reader that has acknowledged nothing -
    let clock = LeaseClock::monotonic();
    let clocks = LeaseClocks::derive(Duration::from_micros(250)).expect("shipped clocks");
    let owner = MembershipOwner::arm("free-grace-bench", 2, 1, clocks.clone(), clock.clone())
        .expect("arm the authority");
    squeezefs::membership::install_owner(Arc::clone(&owner));
    // A grace bound long enough that no bench iteration can trip the
    // fence: this group prices the STEADY state, never the eviction path.
    free_grace::arm_owner_plane_with(
        clock,
        Duration::from_secs(3_600),
        Duration::from_secs(3_600),
    );
    match owner.join(JoinRequest {
        id: "bench-reader".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "bench".to_string(),
        prior_epoch: None,
        pr_key: 0,
    }) {
        JoinOutcome::Granted(_) => {}
        JoinOutcome::Refused { reason, .. } => panic!("bench join refused: {reason}"),
    }
    owner.refresh_free_grace_bound();
    assert!(free_grace::armed(), "the armed arm needs the gate live");

    let mut group = c.benchmark_group("free_grace_armed");
    for depth in [1_024usize, 16_384, 131_072] {
        let ring = GraceRing::new(1 << 20);
        for i in 0..depth as u64 {
            ring.defer(i * CHUNK, CHUNK);
        }
        group.bench_function(format!("defer_armed_depth_{depth}"), |b| {
            b.iter(|| black_box(ring.defer(black_box(u64::MAX - CHUNK), CHUNK)));
        });
        // Nothing is acknowledged (the reader's bound is 0), so this is
        // the "front peek says no" path — the one every free pays.
        group.bench_function(format!("harvest_none_eligible_depth_{depth}"), |b| {
            b.iter(|| black_box(ring.harvest(64).len()));
        });
    }
    group.finish();

    // Everything acknowledged: the release path, batched at HARVEST_BATCH.
    let mut group = c.benchmark_group("free_grace_release");
    group.throughput(Throughput::Elements(
        squeezefs::free_grace::HARVEST_BATCH as u64,
    ));
    group.bench_function("harvest_all_eligible_batch", |b| {
        b.iter_batched(
            || {
                let ring = GraceRing::new(1 << 20);
                for i in 0..4_096u64 {
                    ring.defer(i * CHUNK, CHUNK);
                }
                // Acknowledge past every label: `u64::MAX` is what the
                // plane publishes when no reader can hold a binding.
                free_grace::publish_bound(u64::MAX, 1);
                ring
            },
            |ring| black_box(ring.harvest(squeezefs::free_grace::HARVEST_BATCH).len()),
            BatchSize::SmallInput,
        );
    });
    group.finish();
    free_grace::reset_for_test();
    squeezefs::membership::uninstall();
/// DLM **S9** blocker #3 — the **data-plane allocation partition**
/// (`src/data_alloc_lane.rs`, `docs/design-mw-data-alloc-partition.md`).
///
/// **WRITTEN AND NOT RUN — ruling D11.** *"No cargo test, benches or
/// release gate yet until we are done implementing the DLM and can have N
/// Readers and Writers."* Every claim below is a PREDICTION with a
/// falsification criterion, to be adjudicated by the first measured pass
/// once the D11 window opens. No baseline may be refreshed from it before
/// then.
///
/// ## Field-derived input shapes (no toy inputs — program rule)
///
/// * **fresh-mint rate**: the write-wall campaign
///   (`.benchmarks/2026-07-31-write-wall.md`) measured ~**1,650
///   blocks/s displaced at 6.3–6.8 GB/s** on the shipped 4 MiB block, so a
///   saturated streaming writer mints on the order of 10³ blocks/s per
///   volume — allocation is **once per 4 MiB**, never per op. That is why
///   the mint rows are priced in ns/op but judged as a fraction of a
///   block's pipeline lifetime, not as an IOPS term.
/// * **reuse rate**: the rewrite regime allocates at the same block rate
///   from a free list that a displaced-block workload keeps thousands of
///   entries deep (the same campaign's `block_free_reclaim_queue_bytes`
///   shape) — priced here at **1,024 free entries**, of which only 1/W
///   belong to this lane.
/// * **the free path**: W1's in-place patch runs at **61–67 k IOPS**
///   (`.benchmarks/2026-07-17-rand-write-program-closing.md`) and
///   **allocates nothing**; a CoW rewrite's terminal free runs at the
///   block rate. So the free rows exist to prove a NEGATIVE — that the
///   partition put no probe on the free path — which is the design's
///   central claim (the owning lane is derivable from the offset, so a
///   free needs no lookup).
///
/// ## Predictions, and what refutes each
///
/// | Claim | Structural reason | Falsified by |
/// |---|---|---|
/// | the **unpartitioned** mint is unchanged | one `OnceLock` probe before the shipped CAS loop; `lanes` is `None` on every mount today | `mint_fresh_unpartitioned` past the group threshold vs the committed reference |
/// | the **laned** mint costs a CONSTANT more | the lane step tests at most `W − 1` mask bits (`W ≤ 16`), then runs the same CAS | `mint_fresh_lane_of_4` scaling with anything but `W` (⇒ the step is scanning, not stepping) |
/// | the **free** path is byte-identical | `begin_free`/`finish_free` contain no lane probe at all | ANY separation between `free_unpartitioned` and `free_lane_of_4` ⇒ a probe leaked onto the free path, and the design's premise is broken |
/// | **reuse** under a partition costs ≈ `W`× the scan | only 1/W of the free list qualifies, and the scan is a filter over the same iterator | worse than ≈ `W`× (⇒ the filter re-scans per candidate) or no different (⇒ the filter is not running); a genuine `W`× at field occupancy is what would justify a per-lane free list, which this design deliberately did NOT build |
/// | the reservation is **amortized** | one commit per `reserve_grain_blocks` fresh blocks, none for reuse | `alloc_lane_reservations` ÷ fresh blocks exceeding 1/grain — deterministic, so reportable even under D11 |
fn bench_alloc_lane(c: &mut Criterion) {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::data_alloc_lane as lane;
    use squeezefs::meta_backend::kv::journal::AppendPartition;
    use tokio::runtime::Runtime;

    const FREE_LIST: u64 = 1024;
    const W: u16 = 4;

    let rt = Runtime::new().expect("bench runtime");
    let plain = rt.block_on(async { BlockAllocator::new("alloc_lane_plain").await.unwrap() });
    let laned = rt.block_on(async { BlockAllocator::new("alloc_lane_laned").await.unwrap() });
    laned
        .engage_alloc_lanes(AppendPartition::new(W, 1).expect("partition"))
        .expect("engage");
    // No durable sink is wired: this group prices the ALLOCATOR, and the
    // reservation's cost is one metadata commit whose price belongs to
    // `meta_lv_bench::kv_journal` (the same entry encode + barrier every
    // record pays). Amortization is the claim here, not the commit.

    let mut group = c.benchmark_group("alloc_lane");

    // The arithmetic core: attribution (what a free would need if the lane
    // were NOT derivable) and the mint step.
    group.bench_function("lane_of", |b| {
        b.iter(|| black_box(lane::block_lane_of(black_box(1_234_567), W)))
    });
    group.bench_function("owned_step_lane_of_4", |b| {
        b.iter(|| black_box(lane::next_owned_index_at_or_above(black_box(97), 0b0010, W)))
    });

    // Fresh mint: the A/B that says whether the partition is free when off
    // and constant when on.
    group.bench_function("mint_fresh_unpartitioned", |b| {
        b.iter(|| black_box(rt.block_on(plain.allocate_block())))
    });
    group.bench_function("mint_fresh_lane_of_4", |b| {
        b.iter(|| black_box(rt.block_on(laned.allocate_block())))
    });

    // The free path — the negative both rows must prove.
    let chunk = plain.chunk_size();
    group.bench_function("free_unpartitioned", |b| {
        b.iter_batched(
            || rt.block_on(plain.allocate_block()).expect("seed"),
            |off| black_box(rt.block_on(plain.free_block(off))),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("free_lane_of_4", |b| {
        b.iter_batched(
            || rt.block_on(laned.allocate_block()).expect("seed"),
            |off| black_box(rt.block_on(laned.free_block(off))),
            BatchSize::SmallInput,
        )
    });

    // Reuse at field free-list occupancy: 1,024 entries, 1/W of them ours.
    let reuse_plain =
        rt.block_on(async { BlockAllocator::new("alloc_lane_reuse_p").await.unwrap() });
    let reuse_laned =
        rt.block_on(async { BlockAllocator::new("alloc_lane_reuse_l").await.unwrap() });
    reuse_laned
        .engage_alloc_lanes(AppendPartition::new(W, 1).expect("partition"))
        .expect("engage");
    for i in 0..FREE_LIST {
        reuse_plain.return_from_trim(i * chunk);
        reuse_laned.return_from_trim(i * chunk);
    }
    group.throughput(Throughput::Elements(FREE_LIST));
    group.bench_function("reuse_freelist_unpartitioned", |b| {
        b.iter_batched(
            || (),
            |_| {
                let off = rt.block_on(reuse_plain.allocate_block()).expect("reuse");
                reuse_plain.return_from_trim(off);
                black_box(off)
            },
            BatchSize::SmallInput,
        )
    });
    group.bench_function("reuse_freelist_lane_of_4", |b| {
        b.iter_batched(
            || (),
            |_| {
                let off = rt.block_on(reuse_laned.allocate_block()).expect("reuse");
                reuse_laned.return_from_trim(off);
                black_box(off)
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();

    // The durable half's CPU (the record itself; the commit is priced in
    // `meta_lv_bench::kv_journal`).
    let mut group = c.benchmark_group("alloc_lane_record");
    let rec = lane::LaneReservation {
        writers: W,
        lane: 1,
        reserved_upto: 1 << 30,
    };
    group.bench_function("encode", |b| b.iter(|| black_box(rec.encode())));
    let raw = rec.encode();
    group.bench_function("decode", |b| {
        b.iter(|| black_box(lane::LaneReservation::decode(black_box(&raw))))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_writer_scope,
    bench_ro_gate,
    bench_free_grace_gate,
    bench_alloc_lane,
    bench_staging_shard_removal,
    bench_copy_probe_range,
    bench_coverage_union,
    bench_extent_overlay,
    bench_supersession,
    bench_deferred_free_collect,
    bench_detached_guard,
    bench_admission_gate,
    bench_layout_publish,
    bench_block_key_codec,
    bench_indirect_map_codec,
    bench_flush_coalescing
);
criterion_main!(benches);
