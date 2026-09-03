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
//! * `ReadDest` — FUSE-4e (pre-rc spec §4): the zero-copy READ
//!   destination's window bound, evaluated on EVERY dest-bearing serve
//!   (and, on the multi-block path, once more for the assembly fan-out).
//!   The shapes are the registered-payload window a kernel READ serves
//!   into — the negotiated 1 MiB `max_write` geometry — in its three
//!   arms: the exact-fit serve (window == request, the O_DIRECT/EXA
//!   shape), a partial serve inside a full-size window (the sub-read
//!   shape), and the REFUSAL a cross-ABI breach would take. The refusal
//!   must not be more expensive than the admit: it is the arm a
//!   misbehaving kernel would drive.
//! * `StreamLanes::observe` — the §5.3 classifier (docs/design-read-path)
//!   in its three arms: the exact-contiguity match (steady classified
//!   stream), the CLASSIFIED-MEMBERSHIP ±64×len tolerance (the read-lane
//!   round-3 qd-reorder wedge fix — libaio qd8 completion swaps), and
//!   the foreign/claim arm random traffic rides (the declassify path,
//!   scan-resistance cold-row residual).

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use squeezefs::read_lane::ReadLaneHold;
use squeezefs::routing::{ReadDest, StreamLanes};
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
                let served = hold.serve_with_provenance("bench:block_00000042", SUB_READ);
                assert!(served.is_some(), "hold must serve until coverage retires");
                black_box(served);
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
            // Engagement assert: this bench measures the CLASSIFIED match
            // arm — anything else is measuring the wrong path.
            assert!(
                r.as_ref().is_some_and(|l| l.is_streaming()),
                "steady seq stream must ride the classified match arm"
            );
            black_box(r.is_some())
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
            // in-order request the swap displaced. Both must keep
            // MEMBERSHIP (streaming) — losing it means this bench slid
            // onto the foreign/claim arm (the pre-fix wedge behavior).
            let a = lanes2.observe(black_box(edge + SUB_READ), SUB_READ, true);
            let b2 = lanes2.observe(black_box(edge), SUB_READ, true);
            edge += 2 * SUB_READ;
            assert!(
                a.is_some_and(|l| l.is_streaming()) && b2.is_some_and(|l| l.is_streaming()),
                "reorder pair must stay inside the classified-membership window"
            );
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

fn bench_assembly_join(c: &mut Criterion) {
    // Fan-out/join bookkeeping of the multi-block assembly (MEM-2
    // ownership fix, fix/assembly-task-ownership): spawn N per-block
    // tasks + join ALL of them. Field shapes: N = 2 is the kernel-FUSE
    // multi-block read — a max_read (1 MiB) request straddling a 4 MiB
    // block boundary never spans more than two blocks; N = 16 is the
    // interior large-op fan-out (`squeezefs bench` / il ring reads).
    // Comparator: the RETIRED detached shape (Vec<JoinHandle> +
    // futures::future::try_join_all over a usize-laundered pointer) —
    // the A/B this bench exists for. The owned shape's per-task delta
    // is one `Arc<AssemblyDest>` clone/drop + JoinSet bookkeeping; the
    // copy work itself is identical on both sides, so tasks here are
    // deliberately trivial (the bookkeeping IS the measurement).
    use squeezefs::assembly_tasks::{AssemblyDest, OwnedTaskSet};
    use squeezefs::cache::pool::BufferPool;
    use std::sync::Arc;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("bench runtime");
    let pool = Arc::new(BufferPool::new(1, 4096));
    let mut buf = pool.alloc();
    buf.resize(4096, 0);
    let dest = Arc::new(AssemblyDest::pooled(buf));

    let mut group = c.benchmark_group("assembly_join");
    for n in [2usize, 16] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("owned_join_set_{n}"), |b| {
            let dest = Arc::clone(&dest);
            b.to_async(&rt).iter(move || {
                let dest = Arc::clone(&dest);
                async move {
                    let mut tasks: OwnedTaskSet<()> = OwnedTaskSet::new("bench assembly");
                    for i in 0..n {
                        let dest = Arc::clone(&dest);
                        tasks.spawn(async move {
                            // The ownership the fix added: each task
                            // co-owns the destination.
                            black_box((&dest, i));
                            Ok(())
                        });
                    }
                    let (completed, err) = tasks.join_all().await;
                    assert!(err.is_none());
                    black_box(completed.len())
                }
            });
        });
        group.bench_function(format!("detached_try_join_all_{n}"), |b| {
            b.to_async(&rt).iter(move || async move {
                let mut handles = Vec::with_capacity(n);
                for i in 0..n {
                    // The retired shape: tasks capture a plain word (the
                    // usize-laundered pointer), no owner.
                    let laundered: usize = i;
                    handles.push(tokio::spawn(async move {
                        black_box(laundered);
                        Ok::<(), std::io::Error>(())
                    }));
                }
                let results = futures::future::try_join_all(handles)
                    .await
                    .expect("bench tasks never panic");
                for r in results {
                    r.expect("bench tasks never error");
                }
                black_box(n)
            });
        });
    }
    group.finish();
}

/// POSIX-2 (`docs/pre-rc-engineering-spec.md` §5): the
/// `lseek(SEEK_DATA/SEEK_HOLE)` resolution — `seek_scan_striped`, the
/// pure core of the FUSE `lseek` handler.
///
/// **Field-derived shape.** The sparse consumers are `cp --sparse`,
/// `tar -S`, `rsync -S`, and `qemu-img convert`, which walk a file
/// hole-to-data-to-hole: one lseek pair per RUN, not per block. The
/// inputs are a 4 GiB file at the shipped 4 MiB block size (1,024 block
/// indices — the `block_map` size the layout code spills to an indirect
/// map beyond, see `routing::save_metadata_to_backend`) in three shapes:
/// * `dense` — every index mapped: SEEK_HOLE walks the whole map to the
///   EOF hole. The worst case, and what a `cp --sparse=always` of a
///   fully-allocated image pays once.
/// * `alternating` — every other index mapped (the `qemu-img` shape a
///   discard-heavy guest leaves behind): resolution is O(1) runs but the
///   walk is repeated per run.
/// * `single_run` — one 64-block data island in a mostly-empty file (the
///   `dd seek=` / sparse-log shape).
fn bench_sparse_lseek(c: &mut Criterion) {
    use squeezefs::fuse_client::seek_scan_striped;
    use std::collections::HashMap;

    const BS: u64 = 4 * 1024 * 1024; // shipped block size
    const BLOCKS: u64 = 1024; // 4 GiB
    const SIZE: u64 = BLOCKS * BS;

    let mk = |f: &dyn Fn(u64) -> bool| -> HashMap<u32, String> {
        (0..BLOCKS)
            .filter(|b| f(*b))
            .map(|b| (b as u32, format!("be://{}", b * BS)))
            .collect()
    };
    let dense = mk(&|_| true);
    let alternating = mk(&|b| b % 2 == 0);
    let single_run = mk(&|b| (512..576).contains(&b));

    let mut group = c.benchmark_group("sparse_lseek");
    group.throughput(Throughput::Elements(1));

    // The full-length walks: worst case for each shape.
    group.bench_function("dense_seek_hole_1024_blocks", |b| {
        b.iter(|| {
            black_box(seek_scan_striped(
                black_box(&dense),
                SIZE,
                BS,
                0,
                false,
                |_| false,
            ))
        });
    });
    group.bench_function("alternating_seek_hole", |b| {
        b.iter(|| {
            black_box(seek_scan_striped(
                black_box(&alternating),
                SIZE,
                BS,
                0,
                false,
                |_| false,
            ))
        });
    });
    group.bench_function("single_run_seek_data_scan_512", |b| {
        b.iter(|| {
            black_box(seek_scan_striped(
                black_box(&single_run),
                SIZE,
                BS,
                0,
                true,
                |_| false,
            ))
        });
    });
    // The steady per-run step a sparse copier actually repeats: resolve
    // from just inside the current run.
    group.bench_function("single_run_seek_hole_from_run_start", |b| {
        b.iter(|| {
            black_box(seek_scan_striped(
                black_box(&single_run),
                SIZE,
                BS,
                512 * BS,
                false,
                |_| false,
            ))
        });
    });
    // The parked-custody probe arm: a hole candidate must consult the
    // three-way overlay probe before it may be called a hole (the
    // dirty-custody-is-DATA law). Priced with a probe that always
    // answers "parked", i.e. every candidate pays it.
    group.bench_function("alternating_seek_hole_all_parked", |b| {
        b.iter(|| {
            black_box(seek_scan_striped(
                black_box(&alternating),
                SIZE,
                BS,
                0,
                false,
                |_| true,
            ))
        });
    });

    group.finish();
}

/// FUSE-4e: the per-serve destination-window bound.
fn bench_read_dest_bound(c: &mut Criterion) {
    const WINDOW: usize = 1024 * 1024; // the negotiated max_write geometry
    let mut backing = vec![0u8; WINDOW];
    // SAFETY (bench): `backing` outlives every use below and nothing else
    // touches it — the ReadDest window contract.
    let dest = unsafe { ReadDest::new(backing.as_mut_ptr() as u64, WINDOW) };

    let mut group = c.benchmark_group("read_dest_bound");

    // Exact fit: window == request (the kernel READ / EXA whole-window
    // shape). This is the arm every dest-bearing serve pays.
    group.bench_function("checked_ptr_exact_fit_1m", |b| {
        b.iter(|| black_box(dest.checked_ptr(black_box(WINDOW))))
    });

    // A 4 KiB sub-read inside the full-size window (the rand-4k shape).
    group.bench_function("checked_ptr_sub_read_4k", |b| {
        b.iter(|| black_box(dest.checked_ptr(black_box(4096))))
    });

    // The refusal arm: a serve one byte past the window. Must be as cheap
    // as the admits (a malformed-geometry storm would otherwise pay twice).
    group.bench_function("checked_ptr_refuse_over_window", |b| {
        b.iter(|| black_box(dest.checked_ptr(black_box(WINDOW + 1))))
    });

    // The §5.6 ranged offer: bound + `RangedDest` mint, the whole decision
    // the ranged dispatch makes per eligible request.
    group.throughput(Throughput::Elements(1));
    group.bench_function("ranged_offer_4k", |b| {
        b.iter(|| {
            // SAFETY (bench): as above — `backing` is live and unaliased.
            black_box(unsafe { dest.ranged(black_box(4096)) }.map(|r| r.cap()))
        })
    });

    group.finish();
}

/// **PERF-11 · the tier publish's place on the read serve path.**
///
/// Every cold demand fill above 64 KiB decides on a disk-tier publish, and
/// that publish is a multi-MiB mmap write under the tier shard's write lock
/// — correctly on the blocking pool. What PERF-11 changes is whether the
/// READER waits for it: the publish now carries the single-flight guard, so
/// the anti-churn ordering (publish visible before the flight's registry
/// entry clears) survives while the caller's bytes do not wait.
///
/// The arms price the removed critical-path term at the field's 4 MiB block
/// size — a `spawn_blocking` round trip plus the payload move:
/// * `awaited_hop_4m` — the pre-fix shape (dispatch + await).
/// * `detached_hop_4m` — the shipped shape (dispatch only).
///
/// Note the awaited arm is measured on an IDLE blocking pool; in the field
/// it also inherits the pool's backlog, which is what made the term visible
/// in `read_fill_phase_ns[admission]`.
fn bench_tier_publish_hop(c: &mut Criterion) {
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    const BLOCK: usize = 4 * 1024 * 1024;
    let rt = Runtime::new().expect("bench runtime");
    let payload = Bytes::from(vec![0x5Au8; BLOCK]);
    let sink: Arc<parking_lot::Mutex<Vec<u8>>> =
        Arc::new(parking_lot::Mutex::new(vec![0u8; BLOCK]));

    let mut group = c.benchmark_group("read_tier_publish_hop");
    group.throughput(Throughput::Bytes(BLOCK as u64));

    group.bench_function("awaited_hop_4m", |b| {
        b.iter(|| {
            rt.block_on(async {
                let dl = payload.clone();
                let sink = Arc::clone(&sink);
                let _ = tokio::task::spawn_blocking(move || {
                    sink.lock().copy_from_slice(&dl);
                })
                .await;
            })
        });
    });

    group.bench_function("detached_hop_4m", |b| {
        b.iter(|| {
            rt.block_on(async {
                let dl = payload.clone();
                let sink = Arc::clone(&sink);
                tokio::task::spawn_blocking(move || {
                    sink.lock().copy_from_slice(&dl);
                });
            })
        });
    });

    group.finish();
}

/// R-3 fill-issue economy (read board #3): the two primitives the funnel
/// and the zc bridge pay per fill, in isolation on this box.
///
/// * `enter_per_cqe_N` vs `poll_drain_batch_N` — N ops issued on a real
///   io_uring (NOP SQEs, the kernel's cheapest completion) as N
///   `submit_and_wait(1)` round trips (the retired per-completion park)
///   vs ONE `submit_and_wait(N)` + one CQ drain. The gap is the syscall +
///   ring-sync cost per op the batch amortizes; N = 8 is the field's
///   per-queue in-flight depth (24 jobs × qd8 over 32 queues).
/// * `oneshot_cross_thread_hop` vs `oneshot_same_thread_hop` — a
///   `sqz_channel::oneshot` resolution awaited on a PARKED foreign thread
///   (the bridge's `wake_hop`: futex wake + schedule + resume) vs the
///   same resolution consumed by a poll on the sending thread (the fused
///   venue). The cross-thread row is the mechanism price of one hop at
///   idle; the field pays it three times per zc READ at load.
fn bench_fill_issue_economy(c: &mut Criterion) {
    use io_uring::{opcode, IoUring};
    let mut group = c.benchmark_group("fill_issue_economy");
    for &n in &[1usize, 8, 32] {
        let mut ring = IoUring::new(64).expect("bench ring");
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("enter_per_cqe_{n}"), |b| {
            b.iter(|| {
                for i in 0..n {
                    let e = opcode::Nop::new().build().user_data(i as u64);
                    // SAFETY: a NOP references no user memory.
                    unsafe { ring.submission().push(&e) }.expect("sq room");
                    ring.submit_and_wait(1).expect("enter");
                    let mut cq = ring.completion();
                    cq.sync();
                    for c in cq {
                        black_box(c.user_data());
                    }
                }
            });
        });
        let mut ring = IoUring::new(64).expect("bench ring");
        group.bench_function(format!("poll_drain_batch_{n}"), |b| {
            b.iter(|| {
                for i in 0..n {
                    let e = opcode::Nop::new().build().user_data(i as u64);
                    // SAFETY: a NOP references no user memory.
                    unsafe { ring.submission().push(&e) }.expect("sq room");
                }
                ring.submit_and_wait(n).expect("enter");
                let mut cq = ring.completion();
                cq.sync();
                let mut seen = 0usize;
                for c in cq {
                    black_box(c.user_data());
                    seen += 1;
                }
                assert_eq!(seen, n);
            });
        });
    }
    group.throughput(Throughput::Elements(1));

    // Cross-thread hop: the receiver thread parks on the oneshot (a
    // futex wait through the sqz executor's block_on); the sender
    // resolves it and waits for the receiver's acknowledgement (a second
    // oneshot back) so one iteration = one full wake round trip ÷ 2 is
    // the per-hop price.
    {
        use squeezefs_ipc::sqz_channel::oneshot;
        use std::sync::mpsc;
        let (req_tx, req_rx) = mpsc::channel::<(oneshot::Receiver<u32>, oneshot::Sender<u32>)>();
        let worker = std::thread::spawn(move || {
            while let Ok((rx, ack)) = req_rx.recv() {
                let v = squeezefs_ipc::sqz_blocking::block_on(rx).unwrap_or(0);
                let _ = ack.send(v);
            }
        });
        group.bench_function("oneshot_cross_thread_hop", |b| {
            b.iter(|| {
                let (tx, rx) = oneshot::channel::<u32>();
                let (ack_tx, ack_rx) = oneshot::channel::<u32>();
                req_tx.send((rx, ack_tx)).expect("worker alive");
                let _ = tx.send(7);
                let v = squeezefs_ipc::sqz_blocking::block_on(ack_rx).unwrap_or(0);
                black_box(v);
            });
        });
        drop(req_tx);
        let _ = worker.join();
    }
    group.bench_function("oneshot_same_thread_hop", |b| {
        use squeezefs_ipc::sqz_channel::oneshot;
        b.iter(|| {
            let (tx, rx) = oneshot::channel::<u32>();
            let _ = tx.send(7);
            let v = squeezefs_ipc::sqz_blocking::block_on(rx).unwrap_or(0);
            black_box(v);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_read_dest_bound,
    bench_hold,
    bench_classifier,
    bench_assembly_join,
    bench_sparse_lseek,
    bench_tier_publish_hop,
    bench_fill_issue_economy
);

criterion_main!(benches);
