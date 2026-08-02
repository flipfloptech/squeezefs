//! Criterion micro-benches for the read-path hot machinery — the
//! microbench program (2026-08-04, `.benchmarks/2026-08-04-microbench-program.md`).
//!
//! Field-shape sources:
//! * `ReadLaneHold` — the read-lane campaign
//!   (`.benchmarks/2026-08-01-read-lane.md`): the shipped default win on
//!   the fio-gap read plateau (+12.8 % qd8 / +14.5 % qd32). The hot
//!   shapes are the EXA validation read (4 MiB blocks consumed as four
//!   1 MiB sub-reads — insert → 4 credited serves → coverage
//!   retirement), the credit-0 anti-refetch probe, and the miss probe
//!   every armed read pays.
//! * `StreamLanes::observe` — the §5.3 classifier (docs/design-read-path)
//!   in its three arms: the exact-contiguity match (steady classified
//!   stream), the CLASSIFIED-MEMBERSHIP ±64×len tolerance (the read-lane
//!   round-3 qd-reorder wedge fix — libaio qd8 completion swaps), and
//!   the foreign/claim arm random traffic rides (the declassify path,
//!   scan-resistance cold-row residual).

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use squeezefs::read_lane::ReadLaneHold;
use squeezefs::routing::StreamLanes;
use std::hint::black_box;

const BLOCK: usize = 4 * 1024 * 1024; // production block size
const SUB_READ: u64 = 1024 * 1024; // libaio bs=1M — the EXA client shape

fn bench_hold(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_lane_hold");

    // Full lifecycle: deposit a 4 MiB completed fill, serve it as four
    // 1 MiB credited sub-reads; the 4th crosses coverage and retires the
    // entry (exactly-once accounting). `Bytes` payload is refcounted —
    // the insert/serve costs are map + FIFO + credit arithmetic, which
    // is what the lane adds per block.
    let payload = Bytes::from(vec![0x42u8; BLOCK]);
    let hold = ReadLaneHold::new();
    let budget = 1u64 << 30;
    group.throughput(Throughput::Bytes(BLOCK as u64));
    group.bench_function("insert_serve4_retire_4m", |b| {
        b.iter(|| {
            hold.insert_demand("bench:block_00000042", payload.clone(), budget);
            for _ in 0..4 {
                black_box(hold.serve_with_provenance("bench:block_00000042", SUB_READ));
            }
        });
    });

    // Steady serve (credit 0): the single-flight loop probe — a `Bytes`
    // clone + scc read, no retirement.
    let steady = ReadLaneHold::new();
    steady.insert("bench:block_steady", payload.clone(), budget);
    group.throughput(Throughput::Elements(1));
    group.bench_function("serve_credit0_probe", |b| {
        b.iter(|| black_box(steady.serve_with_provenance(black_box("bench:block_steady"), 0)));
    });

    // The miss probe: what every armed read pays when the key is not
    // held (contains → scc probe).
    group.bench_function("miss_probe", |b| {
        b.iter(|| black_box(steady.contains(black_box("bench:block_absent"))));
    });

    group.finish();
}

fn bench_classifier(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_classifier");
    group.throughput(Throughput::Elements(1));

    // Arm 1: the exact-contiguity match — a classified sequential stream
    // observed once per request (the steady hot arm).
    let lanes = StreamLanes::new();
    let mut off = 0u64;
    // Classify: 4 contiguous requests.
    for _ in 0..4 {
        let _ = lanes.observe(off, SUB_READ, true);
        off += SUB_READ;
    }
    group.bench_function("match_arm_seq_1m", |b| {
        b.iter(|| {
            let r = lanes.observe(black_box(off), SUB_READ, true);
            off += SUB_READ;
            black_box(r.map(|l| l.is_streaming()))
        });
    });

    // Arm 2: classified-membership tolerance — the qd-reorder sibling
    // shape: pairs (edge+len, edge) miss exact contiguity but stay
    // inside the ±64×len window (the round-3 wedge fix path).
    let lanes2 = StreamLanes::new();
    let mut edge = 0u64;
    for _ in 0..4 {
        let _ = lanes2.observe(edge, SUB_READ, true);
        edge += SUB_READ;
    }
    group.bench_function("membership_reorder_pair", |b| {
        b.iter(|| {
            // Out-of-order sibling first (advances the edge), then the
            // in-order request the swap displaced.
            let a = lanes2.observe(black_box(edge + SUB_READ), SUB_READ, true);
            let b2 = lanes2.observe(black_box(edge), SUB_READ, true);
            edge += 2 * SUB_READ;
            black_box((a.is_some(), b2.is_some()))
        });
    });

    // Arm 3: the foreign/claim arm — random 4 KiB traffic (no lane ever
    // matches; every observe runs the foreign count + stalest-lane
    // claim; declassify fires each 16-run — the 2026-07-26 mid-row
    // shape).
    let lanes3 = StreamLanes::new();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    group.bench_function("foreign_random_4k_claim", |b| {
        b.iter(|| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let offset = (x % (1u64 << 40)) & !4095;
            black_box(lanes3.observe(black_box(offset), 4096, true).is_some())
        });
    });

    group.finish();
}

criterion_group!(benches, bench_hold, bench_classifier);
criterion_main!(benches);
