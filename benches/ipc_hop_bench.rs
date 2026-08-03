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

/// The **job-wire frame decode floor** — the other protocol surface in
/// this tree, and the only one whose input is UNTRUSTED: every write
/// mount opens the §5.1.6 listener on `0.0.0.0` (execution-plan ruling
/// D2), so `read_frame` is attacker-reachable before any
/// authentication. VAL-6 rebuilt it (chunk-bounded body commit,
/// per-class caps and deadlines); this group prices what it costs on
/// the honest shapes AND what a hostile prefix costs the coordinator.
///
/// Field-derived input shapes (`docs/design-volume-lifecycle.md`
/// §5.1.6; anchors in `src/job_wire.rs`):
///
/// * `decode_enroll` — the ONLY pre-enrollment shape: worker id + two
///   UUID nonces + a 64-hex HMAC ≈ 300 B, well inside the 8 KiB
///   `MAX_HELLO_FRAME_BYTES` class.
/// * `decode_result_submit_64` — a mover shard's result proposal: 64
///   `BlockChecksum` entries (one per destination block, the VL4 shard
///   granularity — blocks move over shared storage, only their
///   checksums ride the wire).
/// * `refuse_oversize_prefix` — the cheapest hostile shape: four bytes
///   claiming more than the cap. Must be a comparison, never an
///   allocation.
/// * `refuse_lying_prefix_16mib` — the expensive hostile shape: a
///   `MAX_FRAME_BYTES` claim with a 4 KiB body. This is the DoS unit
///   cost per connection, and the reason body memory is committed
///   `FRAME_CHUNK_BYTES` at a time instead of up front.
fn bench_job_wire_frames(c: &mut Criterion) {
    use squeezefs::job_wire::{
        read_frame, read_frame_limited, write_frame, BlockChecksum, DestTuple, WireFrame,
        MAX_FRAME_BYTES, MAX_HELLO_FRAME_BYTES, WIRE_SCHEMA,
    };
    use tokio::runtime::Runtime;

    let rt = Runtime::new().expect("bench runtime");

    let enroll = WireFrame::Enroll {
        wire_schema: WIRE_SCHEMA,
        worker_id: "sqz-worker-a3f1c2".to_string(),
        server_nonce: "0f1c2d3e-4a5b-6c7d-8e9f-a0b1c2d3e4f5".to_string(),
        endpoint_nonce: "9e8d7c6b-5a49-3827-1605-f4e3d2c1b0a9".to_string(),
        hmac: "5f".repeat(32),
        pr_key: Some(0xA0),
    };
    let submit = WireFrame::ResultSubmit {
        job_id: "job-0f1c2d3e".to_string(),
        shard: 0,
        shard_fencing: 3,
        checksums: (0..64u64)
            .map(|i| BlockChecksum {
                dest: DestTuple {
                    backend_id: (i % 4) as u32,
                    offset: i * 4 * 1024 * 1024,
                },
                len: 4 * 1024 * 1024,
                xxh3: 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(i + 1),
            })
            .collect(),
    };

    let encode = |f: &WireFrame| -> Vec<u8> {
        let mut buf = Vec::new();
        rt.block_on(write_frame(&mut buf, f)).expect("encode");
        buf
    };
    let enroll_wire = encode(&enroll);
    let submit_wire = encode(&submit);
    let oversize_prefix = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
    let lying_prefix = {
        let mut v = MAX_FRAME_BYTES.to_be_bytes().to_vec();
        v.extend(std::iter::repeat_n(b'x', 4096));
        v
    };

    let mut g = c.benchmark_group("job_wire_frame");
    g.throughput(Throughput::Elements(1));

    g.bench_function("decode_enroll", |b| {
        b.to_async(&rt).iter(|| async {
            let mut cur = std::io::Cursor::new(enroll_wire.as_slice());
            let f = read_frame_limited(&mut cur, MAX_HELLO_FRAME_BYTES, None)
                .await
                .expect("valid hello")
                .expect("one frame");
            std::hint::black_box(f)
        })
    });

    g.bench_function("decode_result_submit_64", |b| {
        b.to_async(&rt).iter(|| async {
            let mut cur = std::io::Cursor::new(submit_wire.as_slice());
            let f = read_frame(&mut cur)
                .await
                .expect("valid submit")
                .expect("one frame");
            std::hint::black_box(f)
        })
    });

    g.bench_function("refuse_oversize_prefix", |b| {
        b.to_async(&rt).iter(|| async {
            let mut cur = std::io::Cursor::new(oversize_prefix.as_slice());
            let e = read_frame(&mut cur).await.expect_err("past the cap");
            std::hint::black_box(e)
        })
    });

    g.bench_function("refuse_lying_prefix_16mib", |b| {
        b.to_async(&rt).iter(|| async {
            let mut cur = std::io::Cursor::new(lying_prefix.as_slice());
            let e = read_frame(&mut cur).await.expect_err("truncated body");
            std::hint::black_box(e)
        })
    });
}

/// VAL-5c/VAL-5e (pre-RC control-plane bounds, 2026-08-02): the per-pass
/// drain bookkeeping the fairness bound added to the HOT serve loop.
/// Every op now pays a budget compare and an in-flight ledger pair, and
/// every pass pays a rotation index — this group is where that price is
/// visible (or shown to be noise).
///
/// FIELD-derived shapes (the program's toy-input ban):
/// * ring/slot geometry = the shipped `Geometry::default_v1` — 1024 ring
///   entries, 1024 slots (`layout::DEFAULT_RING_ENTRIES`), which is also
///   the drain budget (VAL-5e's quantum = one honest in-flight window);
/// * a drain pass of **32 published ops** — the ingest-economy field
///   shape (a 32-CPU client's fleet keeps tens of ops in flight per
///   session, `.benchmarks/2026-07-28-ingest-economy.md`);
/// * **4 sessions per service thread** — sessions outnumber threads on
///   the derived defaults (`sizing::il_sessions_default` = clamp(cpus/4,
///   2, 16) sessions against the same ceiling of threads, with fd-sharded
///   multi-process fleets pushing the ratio up); 4 is the shape the
///   §5.5.1 pinning invariant makes ordinary.
fn bench_drain_pass(c: &mut Criterion) {
    use squeezefs_ipc::layout::DEFAULT_RING_ENTRIES;
    use std::sync::atomic::{AtomicU32, Ordering};

    const PUBLISHED: u32 = 32;
    const SESSIONS: usize = 4;
    let budget = DEFAULT_RING_ENTRIES;

    let mut g = c.benchmark_group("ipc_drain");
    g.throughput(Throughput::Elements(u64::from(PUBLISHED)));

    // The pre-VAL-5e loop: pop until the ring reports empty.
    let storage = RingStorage::with_capacity(DEFAULT_RING_ENTRIES).expect("capacity");
    let ring = storage.view();
    let mut consumer = RingConsumer::new();
    g.bench_function("pop_loop_unbudgeted", |b| {
        b.iter(|| {
            for i in 0..PUBLISHED {
                assert!(ring.push(i));
            }
            let mut served = 0u32;
            while let Some(idx) = consumer.pop(&ring) {
                std::hint::black_box(idx);
                served += 1;
            }
            std::hint::black_box(served)
        })
    });

    // The shipped loop: the same pops plus the per-op budget compare.
    let storage_b = RingStorage::with_capacity(DEFAULT_RING_ENTRIES).expect("capacity");
    let ring_b = storage_b.view();
    let mut consumer_b = RingConsumer::new();
    g.bench_function("pop_loop_budgeted", |b| {
        b.iter(|| {
            for i in 0..PUBLISHED {
                assert!(ring_b.push(i));
            }
            let mut served = 0u32;
            while let Some(idx) = consumer_b.pop(&ring_b) {
                std::hint::black_box(idx);
                served += 1;
                if served >= budget {
                    break;
                }
            }
            std::hint::black_box(served)
        })
    });

    // A whole round-robin sweep over 4 owned sessions: the rotation
    // index + modulo per session, the budget compare per op.
    let rings: Vec<RingStorage> = (0..SESSIONS)
        .map(|_| RingStorage::with_capacity(DEFAULT_RING_ENTRIES).expect("capacity"))
        .collect();
    let mut consumers: Vec<RingConsumer> = (0..SESSIONS).map(|_| RingConsumer::new()).collect();
    let mut rr_start = 0usize;
    g.throughput(Throughput::Elements(u64::from(PUBLISHED) * SESSIONS as u64));
    g.bench_function("round_robin_pass_4_sessions", |b| {
        b.iter(|| {
            for r in &rings {
                let view = r.view();
                for i in 0..PUBLISHED {
                    assert!(view.push(i));
                }
            }
            let mut served = 0u32;
            for k in 0..SESSIONS {
                let s = (rr_start.wrapping_add(k)) % SESSIONS;
                let view = rings[s].view();
                let mut per_session = 0u32;
                while let Some(idx) = consumers[s].pop(&view) {
                    std::hint::black_box(idx);
                    per_session += 1;
                    if per_session >= budget {
                        break;
                    }
                }
                served += per_session;
            }
            rr_start = rr_start.wrapping_add(1);
            std::hint::black_box(served)
        })
    });

    // The VAL-5c ledger pair, per op (mirrors `ipc_host::SessionInflight`
    // exactly: `fetch_add` on admit, saturating `fetch_update` on
    // release — both AcqRel, both uncontended in the single-consumer
    // drain, contended only against completions landing off-thread).
    let live = AtomicU32::new(0);
    g.throughput(Throughput::Elements(1));
    g.bench_function("inflight_ledger_pair", |b| {
        b.iter(|| {
            let n = live.fetch_add(1, Ordering::AcqRel) + 1;
            std::hint::black_box(n);
            let _ = live.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(1))
            });
        })
    });
    g.finish();
}

/// **PERF-18 · the header's opposite-side wake pairs.**
///
/// Two pairs in the session header were opposite-side producer/consumer
/// words sharing ONE cache line: `doorbell` (client RMW per submit) with
/// `daemon_parked` (daemon store per park), and `CqeDoorbell.seq` (daemon
/// RMW per completion) with `.parked` (client RMW per park). Each side's
/// write invalidated the line the other side was about to read — on every
/// submit and every completion.
///
/// The arms are the exact traffic shape of one such pair, run
/// concurrently: writer A RMWs its word and reads B's; writer B RMWs its
/// word and reads A's. `same_line` is the pre-PERF-18 placement,
/// `split_lines` the shipped one (the words are now 64 B apart — the
/// layout pins in `squeezefs_ipc::layout` assert exactly that).
fn bench_wake_pair_lines(c: &mut Criterion) {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    const OPS: usize = 20_000;

    #[repr(C, align(64))]
    struct SameLine {
        a: AtomicU32,
        b: AtomicU32,
        _pad: [u8; 56],
    }
    #[repr(C, align(64))]
    struct SplitLines {
        a: AtomicU32,
        _pad_a: [u8; 60],
        b: AtomicU32,
        _pad_b: [u8; 60],
    }

    let mut g = c.benchmark_group("ipc_wake_pair_lines");
    g.throughput(Throughput::Elements((2 * OPS) as u64));

    g.bench_function("same_line", |bch| {
        let w = Arc::new(SameLine {
            a: AtomicU32::new(0),
            b: AtomicU32::new(0),
            _pad: [0; 56],
        });
        bch.iter(|| {
            let (x, y) = (Arc::clone(&w), Arc::clone(&w));
            let ta = std::thread::spawn(move || {
                for _ in 0..OPS {
                    x.a.fetch_add(1, Ordering::Release);
                    std::hint::black_box(x.b.load(Ordering::Acquire));
                }
            });
            let tb = std::thread::spawn(move || {
                for _ in 0..OPS {
                    y.b.fetch_add(1, Ordering::Release);
                    std::hint::black_box(y.a.load(Ordering::Acquire));
                }
            });
            ta.join().unwrap();
            tb.join().unwrap();
        });
    });

    g.bench_function("split_lines", |bch| {
        let w = Arc::new(SplitLines {
            a: AtomicU32::new(0),
            _pad_a: [0; 60],
            b: AtomicU32::new(0),
            _pad_b: [0; 60],
        });
        bch.iter(|| {
            let (x, y) = (Arc::clone(&w), Arc::clone(&w));
            let ta = std::thread::spawn(move || {
                for _ in 0..OPS {
                    x.a.fetch_add(1, Ordering::Release);
                    std::hint::black_box(x.b.load(Ordering::Acquire));
                }
            });
            let tb = std::thread::spawn(move || {
                for _ in 0..OPS {
                    y.b.fetch_add(1, Ordering::Release);
                    std::hint::black_box(y.a.load(Ordering::Acquire));
                }
            });
            ta.join().unwrap();
            tb.join().unwrap();
        });
    });

    // The FIELD shape, which the symmetric arms above are not: one side is
    // HOT (the client bumps `doorbell` on every submit / the daemon bumps
    // `cqe.seq` on every completion) and the other is COLD (the daemon
    // stores `daemon_parked` only when it actually parks; a saturated
    // reaper does not park at all — `REAP_EVENT_PARK_MAX` = 2). Both sides
    // still READ the other's word on their own cadence.
    const COLD_EVERY: usize = 1_000;
    g.bench_function("same_line_hot_cold", |bch| {
        let w = Arc::new(SameLine {
            a: AtomicU32::new(0),
            b: AtomicU32::new(0),
            _pad: [0; 56],
        });
        bch.iter(|| {
            let (x, y) = (Arc::clone(&w), Arc::clone(&w));
            let ta = std::thread::spawn(move || {
                for _ in 0..OPS {
                    x.a.fetch_add(1, Ordering::Release);
                    std::hint::black_box(x.b.load(Ordering::Acquire));
                }
            });
            let tb = std::thread::spawn(move || {
                for i in 0..OPS {
                    if i % COLD_EVERY == 0 {
                        y.b.fetch_add(1, Ordering::Release);
                        std::hint::black_box(y.a.load(Ordering::Acquire));
                    }
                }
            });
            ta.join().unwrap();
            tb.join().unwrap();
        });
    });
    g.bench_function("split_lines_hot_cold", |bch| {
        let w = Arc::new(SplitLines {
            a: AtomicU32::new(0),
            _pad_a: [0; 60],
            b: AtomicU32::new(0),
            _pad_b: [0; 60],
        });
        bch.iter(|| {
            let (x, y) = (Arc::clone(&w), Arc::clone(&w));
            let ta = std::thread::spawn(move || {
                for _ in 0..OPS {
                    x.a.fetch_add(1, Ordering::Release);
                    std::hint::black_box(x.b.load(Ordering::Acquire));
                }
            });
            let tb = std::thread::spawn(move || {
                for i in 0..OPS {
                    if i % COLD_EVERY == 0 {
                        y.b.fetch_add(1, Ordering::Release);
                        std::hint::black_box(y.a.load(Ordering::Acquire));
                    }
                }
            });
            ta.join().unwrap();
            tb.join().unwrap();
        });
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_ring_push_pop,
    bench_slot_cycle,
    bench_payload_moves,
    bench_cqe_doorbell,
    bench_wake_pair_lines,
    bench_job_wire_frames,
    bench_drain_pass
);
criterion_main!(benches);
