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

criterion_group!(
    benches,
    bench_coverage_union,
    bench_extent_overlay,
    bench_supersession,
    bench_layout_publish
);
criterion_main!(benches);
