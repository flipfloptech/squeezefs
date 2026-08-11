//! D1.b op-registry sharding contracts (2026-08-11 write-IOPS campaign).
//!
//! The 525 k-IOPS worker profile named `OpRegistry::claim` as the single
//! largest userspace self-cost line (6.65 % — four inlined CAS sites,
//! `fuse_client.rs:1265`): a flat 256-slot slab, 4× UNDER the delivered
//! ring capacity on the field rig (32 possible CPUs × depth 32 = 1024 in
//! flight), claimed through ONE globally-shared round-robin cursor over
//! 24-byte slots packed ~2.7 per cache line — every op bouncing the same
//! 96 lines across 32 CPUs. Evidence:
//! `.benchmarks/2026-08-11-write-iops-campaign-day1.md` (addendum 2).
//!
//! The contract these tests pin: the registry SHADOWS the transport's
//! delivered geometry (shards = possible CPUs, slots/shard = 2 ×
//! `Q_DEPTH_DESIRED`, shipped-256 floor), each slot owns its cache line,
//! claims stay thread-affine (home shard first), and the existing D1.b
//! semantics survive unchanged: exhaustion degrades to
//! unregistered-but-profiled, release is legal from any thread, and the
//! watchdog scan sees every shard.

use std::time::Duration;

use squeezefs::cpu::possible_cpus;
use squeezefs::fuse_client::{
    op_profile_inflight, op_registry_derived_geometry, op_registry_geometry,
    op_registry_slot_layout, op_watchdog_tick, FuseOpKind, OpProf,
};

/// The drift-is-red tie (the `il_sessions_default` pattern): the LIVE
/// registry's geometry must BE the derivation at this box's possible-CPU
/// count. Red against the shipped flat slab (`(1, 256)`).
#[test]
fn registry_geometry_ties_to_the_transport_derivation() {
    assert_eq!(
        op_registry_geometry(),
        op_registry_derived_geometry(possible_cpus()),
        "the live op registry must carry the derived (shards, slots/shard) \
         geometry — a fixed slab is the 6.65 %-of-worker CAS storm"
    );
}

/// The pure derivation on both canonical box shapes (the derivation-sweep
/// convention): the field rig and the floor box, plus the never-regress
/// floor and the ≥ 2× delivered-capacity law.
#[test]
fn derivation_covers_field_and_floor_shapes() {
    let depth = fuse3::raw::Q_DEPTH_DESIRED;
    assert_eq!(depth, 32, "the depth ceiling the derivation cites");

    // Field rig: 32 possible CPUs ⇒ 32 shards × 64 = 2048 slots — 2× the
    // delivered 32 × 32 = 1024 ring capacity (the shipped 256 was 4× UNDER).
    assert_eq!(op_registry_derived_geometry(32), (32, 64));

    // Floor box: 2 CPUs ⇒ the shipped-256 floor holds (4 shards × 64) —
    // no box gets less watchdog surface than every box already ran.
    assert_eq!(op_registry_derived_geometry(2), (4, 64));

    // Degenerate input never yields a zero-shard registry.
    let (shards0, slots0) = op_registry_derived_geometry(0);
    assert!(shards0 >= 1 && shards0 * slots0 >= 256);

    // The ≥ 2× delivered-capacity law across a CPU sweep.
    for cpus in [1usize, 2, 4, 8, 16, 32, 64, 128, 512] {
        let (shards, slots) = op_registry_derived_geometry(cpus);
        assert!(
            shards * slots >= 2 * cpus * depth || shards * slots >= 256,
            "{cpus} cpus: {shards}×{slots} must cover 2× delivered capacity \
             (or the shipped floor)"
        );
        assert!(
            shards * slots >= 256,
            "{cpus} cpus: never regress below the shipped 256-slot surface"
        );
    }
}

/// The false-sharing law: a slot is CAS-claimed from every CPU at op
/// rate, so it must own its cache line outright. Red against the shipped
/// 24-byte packed slot (~2.7 slots/line).
#[test]
fn op_slot_owns_its_cache_line() {
    let (size, align) = op_registry_slot_layout();
    assert_eq!(
        (size, align),
        (64, 64),
        "OpSlot must be exactly one 64-byte cache line (size {size}, align {align})"
    );
}

/// Capacity: the registry must register the transport's ENTIRE delivered
/// ring capacity (possible CPUs × depth ceiling) concurrently — the field
/// shape the 256-slab dropped 768 of. Red on every box with > 8 possible
/// CPUs. (`begin`, not `begin_forced`: the always-on watchdog path.)
#[test]
fn registry_registers_the_full_delivered_ring_capacity() {
    let delivered = possible_cpus() * fuse3::raw::Q_DEPTH_DESIRED;
    let inflight0 = op_profile_inflight();
    let herd: Vec<OpProf> = (0..delivered)
        .map(|i| OpProf::begin(FuseOpKind::Write, i as u64))
        .collect();
    assert_eq!(
        op_profile_inflight(),
        inflight0 + delivered as u64,
        "every op of the delivered ring capacity ({delivered}) must hold a \
         registry slot — unregistered ops are invisible to the D1.b watchdog"
    );
    drop(herd);
    assert_eq!(op_profile_inflight(), inflight0, "all slots recycle on drop");
}

/// Exhaustion contract carried forward verbatim: past TOTAL capacity the
/// claim degrades to unregistered (never blocks, never panics), and every
/// claimed slot still recycles.
#[test]
fn exhaustion_degrades_to_unregistered_and_recycles() {
    let (shards, slots) = op_registry_geometry();
    let total = shards * slots;
    let inflight0 = op_profile_inflight();
    let herd: Vec<OpProf> = (0..total + 44)
        .map(|i| OpProf::begin(FuseOpKind::Getattr, i as u64))
        .collect();
    assert!(
        op_profile_inflight() <= inflight0 + total as u64,
        "claims past total capacity must degrade to None"
    );
    drop(herd);
    assert_eq!(op_profile_inflight(), inflight0, "exhaustion herd fully recycles");
}

/// Foreign-thread release stays legal (the sharded claim is thread-AFFINE,
/// never thread-OWNED): an op claimed here may drop on any thread — FUSE
/// futures migrate across handler lanes and tokio workers.
#[test]
fn release_from_a_foreign_thread_recycles_the_slot() {
    let inflight0 = op_profile_inflight();
    let op = OpProf::begin(FuseOpKind::Write, 7);
    assert_eq!(op_profile_inflight(), inflight0 + 1);
    std::thread::spawn(move || drop(op)).join().expect("release thread");
    assert_eq!(
        op_profile_inflight(),
        inflight0,
        "a foreign-thread drop must release the slot"
    );
}

/// The watchdog scan walks EVERY shard: ops registered from many threads
/// (distinct home shards) all appear in one tick's report.
#[test]
fn watchdog_scan_covers_all_shards() {
    let threads = 8;
    let per_thread = 4;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<OpProf>>();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads + 1));
    let mut joins = Vec::new();
    for t in 0..threads {
        let tx = tx.clone();
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            let ops: Vec<OpProf> = (0..per_thread)
                .map(|i| OpProf::begin(FuseOpKind::Fsync, (0xdead0000 + t * 100 + i) as u64))
                .collect();
            tx.send(ops).expect("send ops");
            barrier.wait(); // hold the thread alive until the scan ran
        }));
    }
    let held: Vec<Vec<OpProf>> = (0..threads).map(|_| rx.recv().expect("ops")).collect();
    let report = op_watchdog_tick(Duration::ZERO);
    let seen = report
        .iter()
        .filter(|o| o.op == "fsync" && (0xdead0000..0xdeadffff).contains(&o.ino))
        .count();
    assert_eq!(
        seen,
        threads * per_thread,
        "one tick must report every registered op regardless of home shard"
    );
    barrier.wait();
    drop(held);
    for j in joins {
        j.join().expect("worker");
    }
}
