//! Criterion micro-benches for the zcrx read-lane cores — the microbench
//! program (2026-08-04, `.benchmarks/2026-08-04-microbench-program.md`).
//!
//! Field-shape sources:
//! * `SpanLedger` — the grant/refcount/free-stack ledger under every area
//!   chunk (design-zcrx-read-lane §4.3/§5; loom-modeled core). The Phase-1
//!   field bench ran 200 M+ chunk recycles with zero stalls
//!   (`.benchmarks/2026-08-03-zcrx-lane.md` §3) — grant/release IS the
//!   per-chunk steady-state cost, and add_ref/release is the fill-scatter
//!   / gather clone-drop edge (multiple consumers per chunk).
//! * chunk-stream parser — the push-based PDU state machine over AREA
//!   CHUNKS (design §4.3: headers interleave with data and split across
//!   chunk seams; only header bytes copy — ≤ 128 B/PDU, priced in
//!   `zcrx_hdr_copy_bytes`; payload emits as ref sub-slices). Shapes:
//!   page-grain chunks (`chunk_bytes_default()` — the kernel zcrx net_iov
//!   granule), C2HData hlen 24 / pdo 24 (digests off — the negotiation
//!   law), (a) 32 × 4 KiB-payload PDUs (rand-4k-class completion stream)
//!   and (b) one 128 KiB-payload PDU (the MDTS-face sub-command read —
//!   `.benchmarks/2026-08-04-zcrx-z2.md` seam-split contract).
//! * gather fusion (PR Z3, design §4.4/§10) — the Z2 two-pass shape
//!   (completion gather → pooled bounce, then the upstream serve copy)
//!   vs the Z3 fused shape (ONE gather straight into the registered
//!   dest). Field shapes: page-grain payload spans over a 128 KiB
//!   MDTS-face sub-command (32 spans) and a 4 MiB whole-block read
//!   (1024 spans — the EXA cold raw-dest leg the fusion serves). Both
//!   legs use cached copies (the serve NT floor is 256 KiB and the
//!   bench isolates the PASS-COUNT delta, not NT policy — priced in
//!   copy_path_bench).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use squeezefs::zcrx_lane::area::{chunk_bytes_default, AreaSlice, ZcrxArea};
use squeezefs::zcrx_lane::area_core::SpanLedger;
use squeezefs::zcrx_lane::fill_table::ZcrxFill;
use squeezefs::zcrx_lane::pdu_stream::{ParseEvent, StreamParser};
use std::hint::black_box;
use std::sync::Arc;

fn bench_span_ledger(c: &mut Criterion) {
    let mut group = c.benchmark_group("zcrx_span_ledger");
    group.throughput(Throughput::Elements(1));

    // The per-chunk steady state: grant (driver) → last-ref release
    // (consumer) → free-stack recycle.
    let ledger = SpanLedger::new(4096);
    group.bench_function("grant_release_cycle", |b| {
        b.iter(|| {
            let slot = ledger.try_grant().expect("ledger has free slots");
            black_box(ledger.release(slot));
        });
    });

    // The clone-drop edge: a consumer ref taken and dropped without
    // 0-crossing (fill scatter slice / gather pass lifetime).
    let held = ledger.try_grant().expect("free slot");
    group.bench_function("add_ref_release_nocross", |b| {
        b.iter(|| {
            ledger.add_ref(held);
            black_box(ledger.release(held));
        });
    });

    // Refcount contention: 4 threads clone-dropping refs on ONE hot slot
    // (the multi-consumer chunk shape — fill spans + gather in flight).
    // The main thread's grant keeps the count above zero, so the free
    // stack stays out of the picture: this isolates the shared-cacheline
    // fetch_add/fetch_sub term.
    group.throughput(Throughput::Elements(4));
    group.bench_function("add_ref_release_contended_4t", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            std::thread::scope(|s| {
                for _ in 0..4 {
                    let ledger = &ledger;
                    s.spawn(move || {
                        for _ in 0..iters {
                            ledger.add_ref(held);
                            ledger.release(held);
                        }
                    });
                }
            });
            start.elapsed()
        });
    });
    ledger.release(held);
    group.finish();
}

/// Encode one C2HData PDU (hlen 24, pdo 24 — no pad, digests off) into
/// `out`: CH + PSH + payload.
fn push_c2h(out: &mut Vec<u8>, cid: u16, datao: u32, payload_len: u32, last: bool) {
    let plen = 24 + payload_len;
    out.push(0x07); // PDU_C2H_DATA
    out.push(if last { (1 << 2) | (1 << 3) } else { 0 }); // LAST|SUCCESS on the final PDU
    out.push(24); // hlen
    out.push(24); // pdo
    out.extend_from_slice(&plen.to_le_bytes());
    // PSH: cccid @8..10, resvd @10..12, datao @12..16, datal @16..20, resvd.
    out.extend_from_slice(&cid.to_le_bytes());
    out.extend_from_slice(&[0u8; 2]);
    out.extend_from_slice(&datao.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend(std::iter::repeat_n(0xC3u8, payload_len as usize));
}

/// Lay a raw byte stream into page-grain area chunks (headers land split
/// across seams exactly as NIC delivery would) and return the chunk
/// slices, refs held for the bench's lifetime.
fn stream_into_chunks(area: &Arc<ZcrxArea>, stream: &[u8]) -> Vec<AreaSlice> {
    let chunk = area.chunk_bytes();
    stream
        .chunks(chunk)
        .map(|part| {
            let grant = area.try_grant_chunk().expect("area has free chunks");
            let ptr = grant.chunk_ptr();
            // SAFETY: the grant gives exclusive custody of a chunk-sized
            // region; `part.len() <= chunk` by construction.
            unsafe {
                std::ptr::copy_nonoverlapping(part.as_ptr(), ptr, part.len());
            }
            AreaSlice::new(grant, ptr as *const u8, part.len())
        })
        .collect()
}

fn bench_pdu_stream(c: &mut Criterion) {
    let chunk = chunk_bytes_default();
    let area = ZcrxArea::new(1024 * chunk as u64, chunk, None).expect("area map");

    let mut group = c.benchmark_group("zcrx_pdu_stream");

    // Shape (a): 32 C2HData PDUs × 4 KiB payload — one 128 KiB read
    // completed as a page-per-PDU stream (worst-case header density).
    let mut stream_a = Vec::new();
    for i in 0..32u32 {
        push_c2h(&mut stream_a, 7, i * 4096, 4096, i == 31);
    }
    let chunks_a = stream_into_chunks(&area, &stream_a);
    group.throughput(Throughput::Bytes(stream_a.len() as u64));
    group.bench_function("parse_128k_as_32x4k_pdus", |b| {
        let mut out: Vec<ParseEvent> = Vec::with_capacity(128);
        b.iter(|| {
            let mut parser = StreamParser::new();
            for ch in &chunks_a {
                parser.push(ch, &mut out).expect("well-formed stream");
            }
            black_box(out.len());
            out.clear(); // drops span refs (non-crossing: base slices hold)
        });
    });

    // Shape (b): one 128 KiB-payload C2HData — the MDTS-face sub-command
    // completion (1 header copy + 32 ref spans; header split across the
    // first seam only when preceded — here the pure span-emit floor).
    let mut stream_b = Vec::new();
    push_c2h(&mut stream_b, 7, 0, 128 * 1024, true);
    let chunks_b = stream_into_chunks(&area, &stream_b);
    group.throughput(Throughput::Bytes(stream_b.len() as u64));
    group.bench_function("parse_128k_single_pdu", |b| {
        let mut out: Vec<ParseEvent> = Vec::with_capacity(64);
        b.iter(|| {
            let mut parser = StreamParser::new();
            for ch in &chunks_b {
                parser.push(ch, &mut out).expect("well-formed stream");
            }
            black_box(out.len());
            out.clear();
        });
    });

    group.finish();
}

/// PR Z3 gather fusion vs the Z2 two-pass shape (design §4.4/§10; the
/// pass the Phase-1 bracket priced at −65–68 % RX CPU): identical
/// page-grain span scatter, (a) Z2 = gather into a pooled bounce + the
/// upstream serve copy into the dest, (b) Z3 = ONE fused gather into
/// the dest. Sim venue — no NIC required (the area is the real mapped
/// machinery, spans are real ledger-refcounted `AreaSlice`s).
fn bench_gather_fusion(c: &mut Criterion) {
    let chunk = chunk_bytes_default();
    let area = ZcrxArea::new(2048 * chunk as u64, chunk, None).expect("area map");

    let mut group = c.benchmark_group("zcrx_gather");
    for (label, len) in [
        // The MDTS-face sub-command completion (128 KiB, 32 spans).
        ("128k", 128 * 1024usize),
        // The EXA cold whole-block raw-dest read (4 MiB, 1024 spans).
        ("4m", 4 * 1024 * 1024usize),
    ] {
        let spans: Vec<(u32, AreaSlice)> = (0..len / chunk)
            .map(|i| {
                let grant = area.try_grant_chunk().expect("area has free chunks");
                let ptr = grant.chunk_ptr();
                // SAFETY: exclusive chunk custody via the fresh grant.
                unsafe { std::ptr::write_bytes(ptr, 0xC3, chunk) };
                (
                    (i * chunk) as u32,
                    AreaSlice::new(grant, ptr as *const u8, chunk),
                )
            })
            .collect();
        let fill = ZcrxFill::from_parts(spans, len);
        let mut dest = vec![0u8; len];
        let mut bounce = vec![0u8; len];
        group.throughput(Throughput::Bytes(len as u64));

        group.bench_function(format!("z2_two_pass_{label}"), |b| {
            b.iter(|| {
                // Pass 1: the Z2 completion gather into the pooled bounce.
                // SAFETY: `bounce` is a bench-owned Vec of exactly `len`
                // bytes, uniquely borrowed for this call, and `fill`'s spans
                // total `len` — the gather writes within the allocation and
                // nothing aliases it.
                unsafe {
                    fill.gather_into(bounce.as_mut_ptr());
                }
                // Pass 2: the upstream serve copy (routing R-S) the Z2
                // shape still pays on the dest leg.
                // SAFETY: bench-owned non-overlapping buffers of `len`.
                unsafe {
                    std::ptr::copy_nonoverlapping(bounce.as_ptr(), dest.as_mut_ptr(), len);
                }
                black_box(dest[len - 1]);
            });
        });

        group.bench_function(format!("z3_fused_{label}"), |b| {
            b.iter(|| {
                // The ONE fused gather (Z3): area spans → dest, done.
                // SAFETY: `dest` is a bench-owned Vec of exactly `len` bytes,
                // uniquely borrowed for this call, and `fill`'s spans total
                // `len` — the gather writes within the allocation and nothing
                // aliases it.
                unsafe {
                    fill.gather_into(dest.as_mut_ptr());
                }
                black_box(dest[len - 1]);
            });
        });

        drop(fill); // release the chunk grants for the next shape
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_span_ledger,
    bench_pdu_stream,
    bench_gather_fusion
);
criterion_main!(benches);
