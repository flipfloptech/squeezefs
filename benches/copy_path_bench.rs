//! Criterion micro-benches for the hot-path COPY primitives — the
//! microbench program (2026-08-04, `.benchmarks/2026-08-04-microbench-program.md`).
//!
//! Field-shape sources (documented per group):
//! * `nt_copy` — the near-zero-copy campaign
//!   (`.benchmarks/2026-07-31-near-zero-copy.md`): the two DMA-destined
//!   copies (lease→`ActiveBlockBuf` merge, placed-sever arena→assembly)
//!   ride 1 MiB-class chunks of 4 MiB blocks; the policy floor is
//!   256 KiB (`nt_copy::DEFAULT_MIN_BYTES`), below which cached copies
//!   win (latency-bound small writes, lines re-read soon). The NT-vs-std
//!   ladder here is the standing A/B for that floor.
//! * `severed_pool` — the ingest-economy campaign
//!   (`.benchmarks/2026-07-28-ingest-economy.md`): the §5.5.2 ring-write
//!   sever recycled through `SeveredPool` deleted the per-op slab-alloc
//!   engine (1.76 M minor faults/s across saturated svc threads at
//!   12.7 GB/s, ~70 % %system → ~1.5 %). Buffers are `max_op_bytes`
//!   (production default 1 MiB, `DEFAULT_MAX_OP_BYTES`); the hit-vs-miss
//!   pair prices exactly the pooled-vs-alloc delta that campaign shipped.
//! * `serve_prelude` — the ipc op-economy campaign
//!   (`.benchmarks/2026-07-28-ipc-op-economy.md`): the warm §5.5.1 serve
//!   prelude is allocation-free (12 → ≈0 allocs/op; the allocs/op law is
//!   pinned by `tests/ipc_op_economy_tests.rs` `SQZ_ALLOC_TRACE=1`) —
//!   these are the ns/op benches beside that law: the zero-heap
//!   `StackKey` vs the heap `format!` it replaced.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use squeezefs::ipc_host::SeveredPool;
use squeezefs::keys::{active_block_stack, StackKey};
use squeezefs::nt_copy::dma_copy_forced;
use std::hint::black_box;

/// NT vs cached (std) copy across the DMA-destined size ladder: 4 KiB
/// (sub-floor — small-write territory, cached must win), 256 KiB (the
/// policy floor), 1 MiB (the merge-chunk class), 4 MiB (whole block).
/// Both sides write the same destination so the A/B is fair; note the
/// NT side's number INCLUDES the load-bearing trailing `sfence`.
fn bench_nt_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("copy_path_nt");
    for (label, len) in [
        ("4k", 4usize * 1024),
        ("256k", 256 * 1024),
        ("1m", 1024 * 1024),
        ("4m", 4 * 1024 * 1024),
    ] {
        // Transport leases / arena windows land at arbitrary alignment;
        // a 1-byte source skew keeps the unaligned-load body honest.
        let src_backing = vec![0x5Au8; len + 1];
        let src = &src_backing[1..];
        let mut dst = vec![0u8; len];
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("nt", label), &len, |b, _| {
            b.iter(|| black_box(dma_copy_forced(&mut dst, black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("std", label), &len, |b, _| {
            b.iter(|| dst.copy_from_slice(black_box(src)));
        });
    }
    group.finish();
}

/// The §5.5.2 sever cost: ONE arena read into a private destination.
/// `pooled_hit` = the shipped path (pool get → 1 MiB copy → put);
/// `alloc_miss` = the retired pre-pool behavior (fresh slab-sized `Vec`
/// per op → copy → dealloc) — the minor-fault engine the campaign
/// deleted. Geometry mirrors production: `max_op_bytes` = 1 MiB
/// buffers, 64 MiB arena cap (`DEFAULT_ARENA_BYTES`).
fn bench_severed_pool(c: &mut Criterion) {
    const OP: usize = 1024 * 1024; // DEFAULT_MAX_OP_BYTES
    let arena_like = vec![0xA7u8; OP]; // page-warm "client arena" source
    let mut group = c.benchmark_group("copy_path_sever");
    group.throughput(Throughput::Bytes(OP as u64));

    let pool = SeveredPool::new(OP as u32, 64 * 1024 * 1024);
    // Seed the pool so the steady state is hit-cycle (the saturated-
    // ingest shape: in-flight severs recycle through retained buffers).
    pool.put(Vec::with_capacity(OP));
    group.bench_function("pooled_hit_1m", |b| {
        b.iter(|| {
            let mut buf = pool.get();
            buf.extend_from_slice(black_box(&arena_like));
            pool.put(buf);
        });
    });

    group.bench_function("alloc_miss_1m", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(OP);
            buf.extend_from_slice(black_box(&arena_like));
            black_box(&buf);
            drop(buf);
        });
    });
    group.finish();
}

/// The warm serve prelude's key-formatting term: the zero-heap
/// `StackKey` (`active_block:inode_{}:block_{}` — the exact §5.5.1 probe
/// key) vs the heap `format!` it replaced. The allocs/op==0 law lives in
/// `tests/ipc_op_economy_tests.rs`; this is its ns/op face.
fn bench_serve_prelude(c: &mut Criterion) {
    let mut group = c.benchmark_group("copy_path_serve_prelude");
    let mut ino = 0u64;
    group.bench_function("stack_key_active_block", |b| {
        b.iter(|| {
            ino = ino.wrapping_add(1);
            let k = active_block_stack(black_box(ino), black_box(ino % 1024));
            black_box(k.len())
        });
    });
    group.bench_function("heap_key_active_block", |b| {
        b.iter(|| {
            ino = ino.wrapping_add(1);
            let k = format!(
                "active_block:inode_{}:block_{}",
                black_box(ino),
                black_box(ino % 1024)
            );
            black_box(k.len())
        });
    });
    // The generic formatter face (overflow-checked, never truncates).
    group.bench_function("stack_key_format_generic", |b| {
        b.iter(|| {
            ino = ino.wrapping_add(1);
            let k = StackKey::format(format_args!("inode_{ino}")).expect("fits");
            black_box(k.len())
        });
    });
    group.finish();
}

/// **PERF-12 · the single-block key resolve.**
///
/// The kernel read path resolves one block's key twice per tier-hit serve
/// (the serve itself, then the binding recheck) and once per prefetch /
/// read-lane fetch task. Both used `load_striped_block_keys`, whose contract
/// is a SPAN: it allocates a `Vec`, clones the key `String` into it, sorts
/// the one-element result and pops it. The shipped path resolves against the
/// live map by reference (`Cow::Borrowed`) and, for the recheck, compares in
/// place without materializing anything.
///
/// The two arms are the exact algorithms over the exact data structure — a
/// 1,024-entry inline block map (a 4 GiB file at the shipped 4 MiB block).
/// The allocs/op face lives in `tests/kernel_op_economy_tests.rs` (14.05 ->
/// 8.05 per warm READ).
fn bench_single_block_resolve(c: &mut Criterion) {
    use std::collections::HashMap;

    let map: HashMap<u32, String> = (0..1024u32)
        .map(|b| (b, format!("sqz:vol-00aa11bb:blk_{b:012x}_0000")))
        .collect();
    let probe = "sqz:vol-00aa11bb:blk_0000000001ff_0000";
    let mut n = 0u32;

    let mut group = c.benchmark_group("read_single_block_resolve");
    group.bench_function("borrowed_compare", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            let idx = black_box(n % 1024);
            let hit = match map.get(&idx) {
                Some(k) => std::borrow::Cow::Borrowed(k.as_str()),
                None => std::borrow::Cow::Owned(String::new()),
            };
            black_box(hit == probe)
        });
    });
    group.bench_function("vec_clone_sort_pop_compare", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            let idx = black_box(n % 1024);
            let mut keys: Vec<(u32, Option<String>)> = Vec::new();
            keys.push((idx, map.get(&idx).cloned()));
            keys.sort_by_key(|(b, _)| *b);
            let hit = keys.pop().and_then(|(_, k)| k);
            black_box(hit.as_deref() == Some(probe))
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_nt_copy,
    bench_severed_pool,
    bench_serve_prelude,
    bench_single_block_resolve
);
criterion_main!(benches);
