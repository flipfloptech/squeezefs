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
use squeezefs::layout_wire::{encode_layout, LayoutDelta, LayoutMetadata};
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

criterion_group!(
    benches,
    bench_coverage_union,
    bench_extent_overlay,
    bench_supersession,
    bench_deferred_free_collect,
    bench_layout_publish,
    bench_indirect_map_codec,
    bench_flush_coalescing
);
criterion_main!(benches);
