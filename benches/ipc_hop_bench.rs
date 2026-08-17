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

    use squeezefs_ipc::cqe_core::CompleteOutcome;

    let bell = CqeDoorbell::new();
    g.bench_function("complete_unparked", |b| {
        b.iter(|| {
            assert_eq!(
                bell.complete(true),
                CompleteOutcome::Elided,
                "no reaper is parked"
            );
        })
    });

    let parked_bell = CqeDoorbell::new();
    let _snapshot = parked_bell.park_begin(); // one parked reaper, held
    g.bench_function("complete_parked", |b| {
        // Latch arm live (the shipped default): the first iteration pays
        // (`Wake`), every later one collapses — the steady-state cost
        // priced here IS the collapse arm (one CAS-fail), the campaign's
        // hot path at fan-in.
        b.iter(|| {
            assert_ne!(
                parked_bell.complete(true),
                CompleteOutcome::Elided,
                "a parked mark-passed completion never elides"
            );
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

/// The **cluster-wire frame floor** — the other protocol surface in this
/// tree, and the only one whose input is UNTRUSTED: every write mount
/// opens the listener on `0.0.0.0` (execution-plan ruling D2), so the
/// reader is attacker-reachable before any authentication. VAL-6 rebuilt
/// its bounds (chunk-bounded body commit, per-class caps and deadlines);
/// **DLM S3 moved them onto `cluster_wire` and replaced the codec**, and
/// this group is where that change is a measured delta rather than a
/// hope.
///
/// The A/B is in the group: `*_json_control` rows run the RETIRED
/// `serde_json` codec over the identical frames, so the ratio is
/// reproducible on any box from one `cargo bench` invocation instead of
/// resting on a remembered number. The row that motivated the change is
/// `decode_result_submit_64`: VAL-6 measured **32.6 µs** for it under
/// `serde_json` against §6.5's **10 µs** custody budget — a lock-acquire
/// path cannot spend its whole latency budget parsing text.
///
/// `mac_roundtrip_result_submit_64` prices what S3 ADDED: the per-frame
/// session MAC (HMAC-SHA256 over direction ‖ sequence ‖ length ‖ body,
/// both directions) that makes authentication survive past enrollment.
/// A regression there is a regression on every custody frame.
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

    // VEHICLE NOTE (rip-tokio sweep, partition F): the framing helpers
    // became SYNC (`std::io::Read`/`Write` on OS threads), so these rows
    // now measure the sync calls directly — no async runtime in the
    // loop. Group and row names are unchanged so history lines up; the
    // committed baseline reference is refreshed as part of this landing.

    let enroll = WireFrame::Enroll {
        wire_schema: WIRE_SCHEMA,
        worker_id: "sqz-worker-a3f1c2".to_string(),
        server_nonce: "0f1c2d3e-4a5b-6c7d-8e9f-a0b1c2d3e4f5".to_string(),
        endpoint_nonce: "9e8d7c6b-5a49-3827-1605-f4e3d2c1b0a9".to_string(),
        hmac: "5f".repeat(32),
        pr_key: Some(0xA0),
        caps: 1, // CAP_FLEET_READ — the KD-MW-16 member-worker shape
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
        write_frame(&mut buf, f).expect("encode");
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

    // --- the S3 codec, encode side --------------------------------------
    g.bench_function("encode_result_submit_64", |b| {
        b.iter(|| {
            let mut buf: Vec<u8> = Vec::new();
            write_frame(&mut buf, &submit).expect("encode");
            std::hint::black_box(buf)
        })
    });

    // --- the retired codec, same frames (the A/B control) ---------------
    g.bench_function("encode_result_submit_64_json_control", |b| {
        b.iter(|| {
            let body = serde_json::to_vec(&submit).expect("json encode");
            std::hint::black_box(body)
        })
    });
    let submit_json = serde_json::to_vec(&submit).expect("json encode");
    g.bench_function("decode_result_submit_64_json_control", |b| {
        b.iter(|| {
            let f: WireFrame = serde_json::from_slice(&submit_json).expect("json decode");
            std::hint::black_box(f)
        })
    });
    let enroll_json = serde_json::to_vec(&enroll).expect("json encode");
    g.bench_function("decode_enroll_json_control", |b| {
        b.iter(|| {
            let f: WireFrame = serde_json::from_slice(&enroll_json).expect("json decode");
            std::hint::black_box(f)
        })
    });

    // --- what S3 added: the per-frame session MAC, both directions ------
    let key = squeezefs::cluster_wire::session_key(
        b"storage-trust-enrollment-secret",
        "sqz-worker-a3f1c2",
        "0f1c2d3e-4a5b-6c7d-8e9f-a0b1c2d3e4f5",
        "9e8d7c6b-5a49-3827-1605-f4e3d2c1b0a9",
        None,
    );
    g.bench_function("mac_roundtrip_result_submit_64", |b| {
        b.iter(|| {
            let (mut tx, _) =
                squeezefs::cluster_wire::session_framers(&key, squeezefs::cluster_wire::Role::Peer);
            let (_, mut rx) = squeezefs::cluster_wire::session_framers(
                &key,
                squeezefs::cluster_wire::Role::Coordinator,
            );
            let mut wire: Vec<u8> = Vec::new();
            tx.send(
                &mut wire,
                squeezefs::cluster_wire::FrameClass::Bulk,
                &submit,
            )
            .expect("authenticated send");
            let mut cur = std::io::Cursor::new(wire);
            let f: WireFrame = rx
                .recv(
                    &mut cur,
                    squeezefs::cluster_wire::FrameClass::Bulk.cap(),
                    None,
                )
                .expect("authenticated recv")
                .expect("one frame");
            std::hint::black_box(f)
        })
    });

    g.bench_function("decode_enroll", |b| {
        b.iter(|| {
            let mut cur = std::io::Cursor::new(enroll_wire.as_slice());
            let f = read_frame_limited(&mut cur, MAX_HELLO_FRAME_BYTES, None)
                .expect("valid hello")
                .expect("one frame");
            std::hint::black_box(f)
        })
    });

    g.bench_function("decode_result_submit_64", |b| {
        b.iter(|| {
            let mut cur = std::io::Cursor::new(submit_wire.as_slice());
            let f = read_frame(&mut cur)
                .expect("valid submit")
                .expect("one frame");
            std::hint::black_box(f)
        })
    });

    g.bench_function("refuse_oversize_prefix", |b| {
        b.iter(|| {
            let mut cur = std::io::Cursor::new(oversize_prefix.as_slice());
            let e = read_frame(&mut cur).expect_err("past the cap");
            std::hint::black_box(e)
        })
    });

    g.bench_function("refuse_lying_prefix_16mib", |b| {
        b.iter(|| {
            let mut cur = std::io::Cursor::new(lying_prefix.as_slice());
            let e = read_frame(&mut cur).expect_err("truncated body");
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
/// * **4 sessions per service thread** — multi-process fleets keep
///   sessions ahead of threads (`sizing::il_sessions_default` =
///   clamp(cpus/4, 2, 16) sessions per PROCESS against the drain-lane
///   ceiling `il_drain_lanes_default` = clamp(3×cpus/8, 2, 64) — a
///   32-process fleet runs ~2.7 sessions/owner on the 32-CPU shape, and
///   fd-sharded single-process workloads push the ratio up); 4 is the
///   shape the §5.5.1 pinning invariant makes ordinary.
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

/// **The S8 price, measured** (spec §6.10 risk **R1**, accepted as ruling
/// **D10**): one authenticated request/response round trip on the S3 wire
/// — the irreducible cost of a function-shipped metadata operation before
/// the owner does any work at all.
///
/// §6.5 item 1 is the arithmetic this row feeds: an uncontended acquire
/// sits inside a 64 µs under-lock span, and one 250 µs fabric RTT takes
/// creates from 9,090/s to 2,778/s. Whether S8 is viable is therefore a
/// question about a measured number, and this is the instrument that
/// produces it.
///
/// **Label discipline (the standing instrument rule):** this venue is
/// **loopback** — TCP over `127.0.0.1`, no NIC, no fabric, no softirq
/// path worth the name. It measures framing + MAC + wake + scheduler and
/// is a **FLOOR**, never a fabric row. The fabric row's requirements
/// (venue, substrate, A-B-B-A, sustained ≥ 60 s) are stated in
/// `.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`.
fn bench_cluster_wire_rtt(c: &mut Criterion) {
    use squeezefs::cluster_wire::{
        PingService, RpcClient, RpcListener, RpcListenerConfig, VERB_PING,
    };
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    let rt = Runtime::new().expect("bench runtime");
    let host = RpcListener::start(
        RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
            service_threads: 1,
            ..RpcListenerConfig::default()
        },
        b"storage-trust-enrollment-secret".to_vec(),
        Arc::new(PingService),
    )
    .expect("listener starts");
    let endpoint = host.endpoint().to_string();
    let client = Arc::new(tokio::sync::Mutex::new(
        rt.block_on(RpcClient::connect(
            &endpoint,
            b"storage-trust-enrollment-secret",
            "bench-peer",
            None,
        ))
        .expect("enrollment"),
    ));

    let mut g = c.benchmark_group("cluster_wire_rtt");
    g.throughput(Throughput::Elements(1));
    // qd1, 0-byte payload: the pure round-trip term an S8 metadata verb
    // pays on top of the owner's own work.
    g.bench_function("authenticated_ping_qd1", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let mut c = client.lock().await;
                std::hint::black_box(c.call(VERB_PING, Vec::new()).await.expect("ping"))
            }
        })
    });
    // 4 KiB: the shape a batched grant/reclaim frame is closer to.
    g.bench_function("authenticated_ping_4k", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let mut c = client.lock().await;
                std::hint::black_box(c.call(VERB_PING, vec![0xa5; 4096]).await.expect("ping"))
            }
        })
    });
    g.finish();
    drop(client);
    host.shutdown();
}

/// **DLM S8 — the function-shipped metadata verb's own cost**
/// (`src/meta_ship/`, spec §6.7 decision 1, §6.9 S8, risk **R1** accepted
/// as ruling **D10**).
///
/// The group above prices the round trip; this one prices everything S8
/// adds *around* it, because R1's published `tar -x` A/B has to be
/// decomposable into terms rather than being one mystery number. Three
/// questions, each a row:
///
/// 1. **Is the codec a visible term next to the RTT?** The loopback RTT
///    floor is 9.33 µs at qd1 (`.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`)
///    and a fabric RTT is 50–150 µs (§6.10 R1). An encode+decode pair
///    costing tens of nanoseconds is invisible; one costing microseconds
///    would have to enter R1's arithmetic.
/// 2. **Does batching amortize the frame, or the ops?** The batch is S8's
///    pipelining unit, so per-verb encode cost at batch 64 versus batch 1
///    is what says whether a concurrent stream's win is real.
/// 3. **Is the fencing read still free?** §6.5 item 1 requires ≥ 99.5 % of
///    lock operations served locally, and the census is ~24 fencing reads
///    per write. The token cache replaces a local `scc` read with a local
///    `scc` read, so the answer should be "unchanged" — and a row is how
///    that stops being a hope.
///
/// **Field-derived input shapes** (`docs/pre-rc-engineering-spec.md` §6.5
/// item 1 for the create wall; the M7 metadata-throughput program for the
/// storm shape):
///
/// * `encode/decode_batch_64_create` — the **create-storm** shape: 64
///   `CreateWithRdev` ops (the derived `SQUEEZEFS_META_SHIP_BATCH_MAX`
///   default on an 8-core box, and the M7 conveyor's own batch floor),
///   16-byte names, mode/uid/gid as a real mdstorm create carries. 64 is
///   also what one owner-side conveyor pass drains, which is why the caps
///   derive from each other.
/// * `encode/decode_batch_1_getattr` — the **serial `tar -x`** shape: one
///   verb per frame, which is exactly what R1 says a serial stream
///   degenerates to. This row is the codec's contribution to that
///   regression.
/// * `refuse_lying_frame` — the hostile shape: a body whose in-frame
///   length claims far more than it carries. Every write mount's listener
///   is reachable before authentication (ruling D2), so a decode's cost
///   must not scale with a *claim*.
/// * `token_cache_hit` / `token_cache_record` — the S4-contract read and
///   the grant absorption that feeds it.
/// * `ownership_probe_unarmed` — requirement 1's price: what a SOLO mount
///   (every shipped mount today) pays for S8 existing.
///
/// **Predictions, and what falsifies them** (written under ruling D11,
/// which defers running this):
///
/// | Row | Prediction | Falsified if |
/// |---|---|---|
/// | `ownership_probe_unarmed` | ≤ 2 ns (one relaxed load) | > 20 ns — then "locality is free" is false and solo mounts pay for an unused feature |
/// | `token_cache_hit` | ≤ 40 ns, i.e. within noise of the S1 local read | > 200 ns — 24 sites × that ≈ a whole metadata op, breaking §6.5 item 1's economics |
/// | `encode_batch_64_create` | ≤ 150 ns/verb (≈ 10 µs/frame at 64) | > 1 µs/verb — the codec becomes a visible term beside the 9.33 µs RTT floor |
/// | `decode_batch_64_create` | same order as encode | > 10 µs/frame — §6.5's custody budget is blown and the batch cap must shrink |
/// | `*_batch_1_getattr` | ≤ 300 ns | > 2 µs — the codec, not the fabric, would dominate R1's serial row at 50 µs RTT, and R1's arithmetic needs a codec term |
/// | `refuse_lying_frame` | flat in the CLAIMED length | any scaling with the claim — the decode bound is not being enforced |
///
/// Not run here (D11: no measured benches until the DLM serves N readers
/// and N writers). The armed ownership probe's extra terms (one
/// `ArcSwapOption::load` + a `Vec` index) are priced by proxy today —
/// `high_concurrency_bench`'s `s4_is_local_slot_solo` and
/// `route_ino_width` rows — and get their own row when the S9 mount
/// wiring makes an owner map cheap to build outside a mount.
fn bench_meta_ship_verbs(c: &mut Criterion) {
    use squeezefs::meta_ship::{
        decode_request, encode_request, foreign_fencing_token, ownership_armed, record_grant,
        MetaCall, MetaOp, MetaRequestFrame, META_SHIP_SCHEMA,
    };

    let batch = |ops: Vec<MetaOp>| MetaRequestFrame {
        schema: META_SHIP_SCHEMA,
        client_epoch: 0x5eed_1234_dead_beef,
        client_id: "node_cafe0123456789ab.m00000001".to_string(),
        owner_term: 7,
        ops,
    };
    let creates = batch(
        (0..64u64)
            .map(|i| MetaOp {
                id: 1_000_000 + i,
                call: MetaCall::CreateWithRdev {
                    parent: 2 + i % 8,
                    // 16-byte names: an mdstorm/`tar -x` file name.
                    name: format!("file-{i:010}"),
                    mode: libc::S_IFREG | 0o644,
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                },
            })
            .collect(),
    );
    let single = batch(vec![MetaOp {
        id: 42,
        call: MetaCall::Getattr { ino: 2 },
    }]);
    let creates_bytes = encode_request(&creates).expect("encode");
    let single_bytes = encode_request(&single).expect("encode");

    let mut g = c.benchmark_group("meta_ship_verbs");

    g.bench_function("ownership_probe_unarmed", |b| {
        b.iter(|| std::hint::black_box(ownership_armed()))
    });

    // The S4-contract read: one `scc` read plus two relaxed atomics, i.e.
    // the same shape the local read it replaces has.
    let grant = squeezefs::meta_ship::TokenGrant {
        ino: 8_675_309,
        token: 0x0000_0100_0000_002a,
        term: 1,
    };
    record_grant(&grant);
    g.bench_function("token_cache_hit", |b| {
        b.iter(|| std::hint::black_box(foreign_fencing_token(grant.ino)))
    });
    g.bench_function("token_cache_record", |b| {
        b.iter(|| record_grant(std::hint::black_box(&grant)))
    });

    g.throughput(Throughput::Elements(64));
    g.bench_function("encode_batch_64_create", |b| {
        b.iter(|| std::hint::black_box(encode_request(&creates).expect("encode")))
    });
    g.bench_function("decode_batch_64_create", |b| {
        b.iter(|| std::hint::black_box(decode_request(&creates_bytes).expect("decode")))
    });

    g.throughput(Throughput::Elements(1));
    g.bench_function("encode_batch_1_getattr", |b| {
        b.iter(|| std::hint::black_box(encode_request(&single).expect("encode")))
    });
    g.bench_function("decode_batch_1_getattr", |b| {
        b.iter(|| std::hint::black_box(decode_request(&single_bytes).expect("decode")))
    });

    // Hostile: a truncated body behind a full-size claim. Must refuse
    // without allocating on the claim.
    let lying = &creates_bytes[..creates_bytes.len() / 8];
    g.bench_function("refuse_lying_frame", |b| {
        b.iter(|| std::hint::black_box(decode_request(lying).is_err()))
    });

    g.finish();
}

/// r5 internal-time program (`.benchmarks/2026-08-08-iops-internal-time-r5.md`):
/// the per-op hotspot instruments behind the direct-drive probe/phase
/// path. FIELD-derived shapes in-file (program convention): ino
/// 1_048_612 (a 7-digit steady-state ino), block 173, block key
/// `"8388608"` — the shipped bare whole-block form (`routing::split_key`
/// docs). Pairs measured:
/// * `clock/*` — the phase-anchor read: `Instant::now`+`elapsed` (the
///   pre-r5 form, 2 reads) vs ONE `mono_core::monotonic_ns_u64` span
///   pair — the single-read law's per-read price (the profile's 8.1 %
///   / 7.1 % svc/dd `__vdso_clock_gettime` term at 5–6 reads/op).
/// * `probe_keys/*` — the probe's 3 key builds: heap (inode_path +
///   two `FsKey::to_string`s + `String` key clone — the pre-r5 form,
///   ~8 % of svc cycles as fmt/push_str/alloc) vs the r5 zero-heap
///   stack forms + `CompactString` inline clone.
fn bench_r5_internal_time(c: &mut Criterion) {
    let mut g = c.benchmark_group("r5_internal_time");
    let ino: u64 = 1_048_612;
    let block: u32 = 173;
    let key = String::from("8388608");

    g.bench_function("clock/instant_pair", |b| {
        b.iter(|| {
            let t0 = std::time::Instant::now();
            std::hint::black_box(t0.elapsed())
        })
    });
    g.bench_function("clock/mono_ns_pair", |b| {
        b.iter(|| {
            let t0 = squeezefs::mono_core::monotonic_ns_u64();
            std::hint::black_box(squeezefs::mono_core::monotonic_ns_u64().saturating_sub(t0))
        })
    });

    g.bench_function("probe_keys/heap_build", |b| {
        b.iter(|| {
            let path = squeezefs::keys::inode_path(std::hint::black_box(ino));
            let ck = squeezefs::keys::active_block_for_path(&path, block).to_string();
            let ek = squeezefs::keys::active_block_ext_for_path(&path, block).to_string();
            let k = std::hint::black_box(&key).clone();
            std::hint::black_box((ck.len(), ek.len(), k.len()))
        })
    });
    g.bench_function("probe_keys/stack_build", |b| {
        b.iter(|| {
            let ck =
                squeezefs::keys::active_block_stack(std::hint::black_box(ino), u64::from(block));
            let ek = squeezefs::keys::active_block_ext_stack(ino, u64::from(block));
            let k = compact_str::CompactString::from(std::hint::black_box(key.as_str()));
            std::hint::black_box((ck.as_str().len(), ek.as_str().len(), k.len()))
        })
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
    bench_cluster_wire_rtt,
    bench_meta_ship_verbs,
    bench_drain_pass,
    bench_r5_internal_time
);
criterion_main!(benches);
