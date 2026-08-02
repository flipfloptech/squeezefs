//! Criterion micro-benches for the fuse3 transport hot path — the
//! microbench program (2026-08-04, root
//! `.benchmarks/2026-08-04-microbench-program.md`).
//!
//! Field-shape sources:
//! * `ent_codec` — the per-op header codec the session pays on EVERY
//!   request/reply: `fuse_in_header` bincode decode (ingress, 40 B) and
//!   the `fuse_out_header` + body serialize (`serialize_into` a
//!   capacity-exact `Vec` — the exact session.rs shape). The bodies are
//!   the metadata-storm hot replies (design-metadata-throughput D2/D3:
//!   lookup/getattr dominated the 4.4× FUSE-layer multiple):
//!   `fuse_attr_out` (GETATTR) and `fuse_entry_out` (LOOKUP).
//! * `commit_batch` — the D3.a drain accounting every queue-worker
//!   flush pays (`.benchmarks/2026-07-18-l3-transport-economy.md`;
//!   `transport_commit_batch*` on the stats inode): one histogram
//!   record per flush, batch sizes cycled 1..=8 (the exact buckets the
//!   "≈ 1 under load ⇒ batching regressed" verdict reads).
//! * `kmbuf` — the attachment law (docs in
//!   `src/raw/connection/kmbuf.rs`; `.benchmarks/2026-08-03-sqz-kernel.md`):
//!   a flagged delivery CQE re-points the ent's buffer, an unflagged one
//!   keeps it (reuse). SIM venue (`KmbufQueue::sim_anon`) — anon-mapped
//!   regions, the state machine only; the kernel surface needs the sqz
//!   kernel. Geometry: depth 32 (the Q_DEPTH clamp), 1 MiB payloads
//!   (the L1 full-size payload-buffer law).
//! * `reply_addressing` — FUSE-2 ⊕ PERF-16: answering "where does this
//!   reply go?". The `pending_map_*` arms reproduce the DELETED sharded
//!   `unique → (qid, ent_idx, commit_id)` map (64 shards, `unique >> 1`
//!   selector, depth-32 queues, kernel-shaped uniques stepping by 2) so
//!   the win stays measurable once the map is out of the tree; the
//!   `carried_slot_*` arms are the shipped path, where the address rides
//!   the request.
//! * `conn_prelude` — the session pool-slot probes the request path pays
//!   4× per READ (pre-rc spec PERF-2: dispatch venue, reply venue,
//!   payload-buffer resolve, ready gate — ~4 M slot reads/s at 1 M
//!   IOPS): `over_uring_ready()` on the empty (pre-arm) and armed
//!   shapes, and the `get_payload_buffer` prelude (slot probe + sharded
//!   pending miss — the READ-reply shape where the reply body is not an
//!   arena slice). SIM venue (`FuseOverUring::sim_inert`) — the shipped
//!   slot + liveness machinery, no kernel session; before/after
//!   comparable across the PERF-2 mutex→lock-free swap.

use async_notify::Notify;
use bincode::Options;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fuse3::get_bincode_config;
use fuse3::raw::abi::{fuse_attr, fuse_attr_out, fuse_entry_out, fuse_in_header, fuse_out_header};
use fuse3::raw::connection::fuse_over_uring::{CommitBatchHistogram, FuseOverUring};
use fuse3::raw::connection::kmbuf::{KmbufQueue, IORING_CQE_BUFFER_SHIFT, IORING_CQE_F_BUFFER};
use fuse3::raw::connection::FuseConnection;
use fuse3::raw::reply::FileAttr;
use fuse3::raw::{ReplySlot, Request};
use fuse3::{FileType, Timestamp};
use std::hint::black_box;
use std::sync::Arc;

fn sample_attr() -> fuse_attr {
    FileAttr {
        ino: 42,
        size: 4 << 20,
        blocks: 8192,
        atime: Timestamp::new(1_700_000_000, 0),
        mtime: Timestamp::new(1_700_000_001, 0),
        ctime: Timestamp::new(1_700_000_002, 0),
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: 1000,
        gid: 1000,
        rdev: 0,
        blksize: 4096,
    }
    .into()
}

fn bench_ent_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse3_ent_codec");
    group.throughput(Throughput::Elements(1));

    // Ingress: fuse_in_header decode from the ent header bytes (fixint
    // LE — 40 bytes: len, opcode, unique, nodeid, uid, gid, pid, pad).
    let mut in_bytes = Vec::new();
    in_bytes.extend_from_slice(&(40u32 + 4096).to_le_bytes()); // len
    in_bytes.extend_from_slice(&16u32.to_le_bytes()); // opcode (WRITE)
    in_bytes.extend_from_slice(&0xDEAD_BEEFu64.to_le_bytes()); // unique
    in_bytes.extend_from_slice(&42u64.to_le_bytes()); // nodeid
    in_bytes.extend_from_slice(&1000u32.to_le_bytes()); // uid
    in_bytes.extend_from_slice(&1000u32.to_le_bytes()); // gid
    in_bytes.extend_from_slice(&4242u32.to_le_bytes()); // pid
    in_bytes.extend_from_slice(&0u32.to_le_bytes()); // padding
    group.bench_function("in_header_decode", |b| {
        b.iter(|| {
            let hdr: fuse_in_header = get_bincode_config()
                .deserialize(black_box(&in_bytes))
                .expect("valid header");
            black_box(hdr.unique)
        });
    });

    // GETATTR reply: out_header + fuse_attr_out into a capacity-exact Vec
    // (the session.rs serialize_into shape, byte for byte).
    let out_hdr_size = std::mem::size_of::<fuse_out_header>();
    let attr_out_size = std::mem::size_of::<fuse_attr_out>();
    group.bench_function("attr_out_encode", |b| {
        b.iter(|| {
            let attr_out = fuse_attr_out {
                attr_valid: 1,
                attr_valid_nsec: 0,
                dummy: 0,
                attr: sample_attr(),
            };
            let out_header = fuse_out_header {
                len: (out_hdr_size + attr_out_size) as u32,
                error: 0,
                unique: 0xDEAD_BEEF,
            };
            let mut data = Vec::with_capacity(out_hdr_size + attr_out_size);
            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("encode");
            get_bincode_config()
                .serialize_into(&mut data, &attr_out)
                .expect("encode");
            black_box(data.len())
        });
    });

    // LOOKUP reply: out_header + fuse_entry_out.
    let entry_out_size = std::mem::size_of::<fuse_entry_out>();
    group.bench_function("entry_out_encode", |b| {
        b.iter(|| {
            let entry_out = fuse_entry_out {
                nodeid: 42,
                generation: 0,
                entry_valid: 1,
                attr_valid: 1,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr: sample_attr(),
            };
            let out_header = fuse_out_header {
                len: (out_hdr_size + entry_out_size) as u32,
                error: 0,
                unique: 0xDEAD_BEEF,
            };
            let mut data = Vec::with_capacity(out_hdr_size + entry_out_size);
            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("encode");
            get_bincode_config()
                .serialize_into(&mut data, &entry_out)
                .expect("encode");
            black_box(data.len())
        });
    });

    group.finish();
}

fn bench_commit_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse3_commit_batch");
    group.throughput(Throughput::Elements(1));
    let hist = CommitBatchHistogram::new();
    let mut n = 0usize;
    group.bench_function("histogram_record_1to8", |b| {
        b.iter(|| {
            n = (n % 8) + 1;
            hist.record(black_box(n));
        });
    });
    black_box(hist.snapshot());
    group.finish();
}

fn bench_kmbuf_attach(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse3_kmbuf");
    group.throughput(Throughput::Elements(1));

    let q = KmbufQueue::sim_anon(32, 1024 * 1024).expect("sim regions map");

    // Fresh selection: every delivery re-points the attachment (bid
    // cycles the ring) — worst-case attachment-table traffic.
    let mut bid = 0u32;
    group.bench_function("note_delivery_flagged", |b| {
        b.iter(|| {
            bid = (bid + 1) % q.ring_entries();
            let flags = IORING_CQE_F_BUFFER | (bid << IORING_CQE_BUFFER_SHIFT);
            black_box(q.note_delivery(black_box(7), flags)).expect("in-range bid")
        });
    });

    // Reuse: an unflagged CQE keeps the attachment (consecutive
    // payload-carrying requests on one ent — the steady write stream).
    group.bench_function("note_delivery_reuse", |b| {
        b.iter(|| black_box(q.note_delivery(black_box(7), 0)).expect("attached above"));
    });

    // The read side `get_payload_buffer` serves through.
    group.bench_function("attached_ptr_read", |b| {
        b.iter(|| black_box(q.attached_ptr(black_box(7))).expect("attached above"));
    });

    group.finish();
}

fn bench_conn_prelude(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse3_conn_prelude");
    group.throughput(Throughput::Elements(1));

    // AsyncFd registration needs a live reactor; the probes themselves
    // are synchronous.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let _guard = rt.enter();
    let conn =
        FuseConnection::new(Arc::new(Notify::new())).expect("/dev/fuse (0666) + io_uring on box");

    // Pre-arm shape: the INIT/REGISTER window's venue probe (empty slot).
    group.bench_function("reply_venue_probe_empty", |b| {
        b.iter(|| black_box(conn.over_uring_ready()));
    });

    let pool = FuseOverUring::sim_inert(2);
    assert!(
        conn.install_over_uring(Arc::clone(&pool)).is_ok(),
        "install into fresh slot"
    );
    pool.mark_ready();

    // Armed steady state: the per-reply venue probe (session.rs reply
    // task + the in-place reply gate) — the exact call the request path
    // pays.
    group.bench_function("reply_venue_probe_armed", |b| {
        b.iter(|| black_box(conn.over_uring_ready()));
    });

    // Armed READ-reply prelude: the slot probe + a direct
    // (qid, ent_idx) index — the shape the reply path pays per READ.
    let slot = ReplySlot::Ring {
        qid: 0,
        ent_idx: 7,
        commit_id: 0xDEAD_BEEF,
    };
    group.bench_function("payload_buffer_probe_armed", |b| {
        b.iter(|| black_box(conn.get_payload_buffer(black_box(slot))));
    });

    group.finish();
}

/// FUSE-2 ⊕ PERF-16 — reply ADDRESSING: what it costs to answer "where
/// does this reply go?" on every request.
///
/// Before: a sharded `unique → (qid, ent_idx, commit_id)` map, paid
/// three times per request (insert at delivery, get in the handler's
/// payload-buffer resolve, remove at reply) — a mutex acquisition plus a
/// hash probe each time, across 32 queue workers plus the handler and
/// reply tasks. After: the address rides the request, so the reply is a
/// move of three integers.
///
/// The `pending_map_*` arms reproduce the DELETED map exactly (64
/// shards, `unique >> 1` selector — kernel uniques step by 2 because bit
/// 0 is `FUSE_INT_REQ_BIT`, so the low bit carried no entropy) so the
/// delta stays measurable after the map is gone from the tree. Shape:
/// depth-32 queues (the `Q_DEPTH_DESIRED` clamp), uniques stepping by 2
/// like the kernel's.
fn bench_reply_addressing(c: &mut Criterion) {
    use std::collections::HashMap;
    use std::sync::Mutex;

    const SHARDS: usize = 64;
    const DEPTH: usize = 32;

    /// The deleted map's value: `(qid, ent_idx, commit_id)`.
    type PendingEnt = (u16, u16, u64);

    struct PendingMap {
        shards: Vec<Mutex<HashMap<u64, PendingEnt>>>,
    }

    impl PendingMap {
        fn new() -> Self {
            Self {
                shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            }
        }
        #[inline]
        fn shard(&self, unique: u64) -> &Mutex<HashMap<u64, PendingEnt>> {
            &self.shards[((unique >> 1) as usize) & (SHARDS - 1)]
        }
        fn insert(&self, unique: u64, v: PendingEnt) {
            self.shard(unique).lock().unwrap().insert(unique, v);
        }
        fn get(&self, unique: u64) -> Option<PendingEnt> {
            self.shard(unique).lock().unwrap().get(&unique).copied()
        }
        fn remove(&self, unique: u64) -> Option<PendingEnt> {
            self.shard(unique).lock().unwrap().remove(&unique)
        }
    }

    let mut group = c.benchmark_group("fuse3_reply_addressing");
    group.throughput(Throughput::Elements(1));

    let map = PendingMap::new();
    let mut unique = 2u64;

    // The FULL per-request address round trip: what the map cost from
    // delivery to reply (insert → handler probe → reply remove).
    group.bench_function("pending_map_request_roundtrip", |b| {
        b.iter(|| {
            let u = unique;
            unique = unique.wrapping_add(2);
            let ent = (u as usize / 2) % DEPTH;
            map.insert(u, (0, ent as u16, u));
            black_box(map.get(u));
            black_box(map.remove(u))
        });
    });

    // The same round trip on the shipped path: the address is BORN with
    // the request and travels with it — nothing to look up, nothing to
    // remove.
    group.bench_function("carried_slot_request_roundtrip", |b| {
        b.iter(|| {
            let u = unique;
            unique = unique.wrapping_add(2);
            let ent = ((u as usize / 2) % DEPTH) as u16;
            let slot = ReplySlot::Ring {
                qid: 0,
                ent_idx: ent,
                commit_id: u,
            };
            let mut req = Request {
                unique: u,
                uid: 0,
                gid: 0,
                pid: 0,
                slot,
            };
            req.slot = black_box(req.slot);
            black_box(resolve_ring_slot(black_box(req.slot)))
        });
    });

    // The single reply-time lookup in isolation (the `submit_reply`
    // prelude: one hash + one mutex, vs a three-word destructure).
    let hot: Vec<u64> = (0..DEPTH as u64).map(|i| 2 + i * 2).collect();
    for u in &hot {
        map.insert(*u, (0, ((*u / 2) % DEPTH as u64) as u16, *u));
    }
    group.bench_function("pending_map_reply_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let u = hot[i % hot.len()];
            i += 1;
            black_box(map.get(black_box(u)))
        });
    });
    group.bench_function("carried_slot_reply_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let u = hot[i % hot.len()];
            i += 1;
            let slot = ReplySlot::Ring {
                qid: 0,
                ent_idx: ((u / 2) % DEPTH as u64) as u16,
                commit_id: u,
            };
            black_box(resolve_ring_slot(black_box(slot)))
        });
    });

    group.finish();
}

/// The shipped reply-address resolve: destructure the slot the request
/// carried (`submit_reply`'s prelude).
#[inline]
fn resolve_ring_slot(slot: ReplySlot) -> Option<(u16, u16, u64)> {
    match slot {
        ReplySlot::Ring {
            qid,
            ent_idx,
            commit_id,
        } => Some((qid, ent_idx, commit_id)),
        ReplySlot::Classical => None,
    }
}

criterion_group!(
    benches,
    bench_ent_codec,
    bench_commit_batch,
    bench_kmbuf_attach,
    bench_conn_prelude,
    bench_reply_addressing
);
criterion_main!(benches);
