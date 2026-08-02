//! Criterion micro-benches for the squeezefs-ipc protocol hot path
//! (PR L4-2 — joins the bench-smoke gate).
//!
//! These are the IN-PROCESS protocol-cost floors (no second process, no
//! futex): what one op costs in ring/slot state-machine work + the 4 KiB
//! payload moves, excluding scheduling. The two-process G-L4-1 numbers
//! come from the rig (`crates/squeezefs-ipc/src/bin/ipc_hop_rig.rs`,
//! `tests/run_ipc_hop_bench.sh`); if these floors regress, the rig numbers
//! will too — that is the regression-signal split.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use squeezefs_ipc::layout::{IpcSlot, SlotDescriptor, OP_READ};
use squeezefs_ipc::ring_core::{RingConsumer, RingStorage};

fn bench_ring_push_pop(c: &mut Criterion) {
    let storage = RingStorage::with_capacity(1024).expect("valid capacity");
    let ring = storage.view();
    let mut consumer = RingConsumer::new();
    let mut g = c.benchmark_group("ipc_hop");
    g.throughput(Throughput::Elements(1));
    g.bench_function("ring_push_pop", |b| {
        b.iter(|| {
            assert!(ring.push(std::hint::black_box(7)));
            assert_eq!(consumer.pop(&ring), Some(7));
        })
    });
    g.finish();
}

fn bench_slot_cycle(c: &mut Criterion) {
    let slot = IpcSlot::new();
    let mut g = c.benchmark_group("ipc_hop");
    g.throughput(Throughput::Elements(1));
    g.bench_function("slot_full_cycle", |b| {
        b.iter(|| {
            let gen = slot.core.try_claim().expect("free slot");
            slot.publish_descriptor(&SlotDescriptor {
                op: OP_READ,
                flags: 0,
                binding: 1,
                offset: 4096,
                len: 4096,
                arena_off: 0,
            });
            slot.core.publish_submitted();
            assert!(slot.core.try_begin_serve());
            let d = slot.snapshot_descriptor();
            slot.set_result(i64::from(d.len));
            slot.core.complete();
            assert!(slot.core.is_done_for(gen));
            let r = slot.result();
            slot.core.release();
            std::hint::black_box(r)
        })
    });
    g.finish();
}

fn bench_payload_moves(c: &mut Criterion) {
    // The 4 KiB each-way payload movement the echo leg pays per op
    // (user→arena + arena→user), isolated.
    let mut arena = vec![0u8; 4096];
    let mut user = vec![7u8; 4096];
    let mut g = c.benchmark_group("ipc_hop");
    g.throughput(Throughput::Bytes(8192));
    g.bench_function("payload_4k_each_way", |b| {
        b.iter(|| {
            arena.copy_from_slice(std::hint::black_box(&user));
            user.copy_from_slice(std::hint::black_box(&arena));
        })
    });
    g.finish();
}

/// Microbench program (2026-08-04,
/// `.benchmarks/2026-08-04-microbench-program.md`): the completion
/// doorbell (the 2026-07-28 ipc op-economy campaign, lever 2 —
/// `.benchmarks/2026-07-28-ipc-op-economy.md`). Both Dekker
/// `fence(SeqCst)` sides are load-bearing (loom-verified ×3), so their
/// cost IS the protocol's cost:
/// * `complete_unparked` — the saturated-reap steady state (wake
///   ELIDED; the pre-campaign posture paid a `FUTEX_WAKE` here — the
///   collect-and-wake serialization term past ~525 k IOPS);
/// * `complete_parked` — the wake-decision path (`true` = the caller
///   pays the syscall; the syscall itself is the rig's to measure);
/// * `park_begin_end` — the reaper's register→snapshot→deregister
///   ceremony around each sparse-regime wait
///   (`REAP_EVENT_PARK_MAX` = 2 keeps this off deep-qd paths).
fn bench_cqe_doorbell(c: &mut Criterion) {
    use squeezefs_ipc::cqe_core::CqeDoorbell;

    let mut g = c.benchmark_group("cqe_doorbell");
    g.throughput(Throughput::Elements(1));

    let bell = CqeDoorbell::new();
    g.bench_function("complete_unparked", |b| {
        b.iter(|| {
            assert!(!bell.complete(), "no reaper is parked");
        })
    });

    let parked_bell = CqeDoorbell::new();
    let _snapshot = parked_bell.park_begin(); // one parked reaper, held
    g.bench_function("complete_parked", |b| {
        b.iter(|| {
            assert!(parked_bell.complete(), "a parked reaper needs the wake");
        })
    });
    parked_bell.park_end();

    let cycle_bell = CqeDoorbell::new();
    g.bench_function("park_begin_end", |b| {
        b.iter(|| {
            let seq = cycle_bell.park_begin();
            cycle_bell.park_end();
            std::hint::black_box(seq)
        })
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_ring_push_pop,
    bench_slot_cycle,
    bench_payload_moves,
    bench_cqe_doorbell
);
criterion_main!(benches);
