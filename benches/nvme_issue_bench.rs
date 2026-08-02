//! PERF-4 (pre-rc engineering spec §9): the NvmeBlockDev read-fill ISSUE
//! path — request build → channel → SQE submit → CQE reap → oneshot
//! dispatch.
//!
//! FIELD-derived input shape (microbench program law): the cold-fill
//! client-side issue+wake term is the PERF-4 anchor — 3.0 ms of the
//! 7.77 ms fill RTT measured client-side (pre-rc spec §9, from the
//! serve-decomposition instrument `.benchmarks/2026-08-01-serve-decomposition.md`:
//! `read_fill_phase_ns.{dev_queue,dev_service}` vs `fetch_dma`). Ranged
//! cold fills are the 4–64 KiB `RANGED_BUF_POOL` class (pool.rs, the
//! 2026-07-25 ipc-miss-path fix); whole-block fills are 4 MiB. Shapes
//! here: 4 KiB and 64 KiB at qd1 (per-op issue floor) and 4 KiB at qd16
//! (the concurrent-fill batch face — elbencho/fio deep-qd cold reads).
//!
//! VENUE: a tmpfs-resident backing file (`/dev/shm` when present). tmpfs
//! refuses O_DIRECT, so the worker's buffered fallback serves from the
//! page cache and the device-service term is ~0 — the measured ns/op IS
//! the issue-path bookkeeping PERF-4 targets, before/after-comparable on
//! the same box (the SAME-BOX RELATIVE baseline law).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use squeezefs::nvme_dev::NvmeBlockDev;
use std::io::Write;

const FILE_LEN: usize = 16 * 1024 * 1024;

/// Backing file on tmpfs when available (deterministic buffered venue).
fn backing_file() -> tempfile::NamedTempFile {
    let mut f = if std::path::Path::new("/dev/shm").is_dir() {
        tempfile::Builder::new()
            .prefix("sqz-nvme-issue-bench-")
            .tempfile_in("/dev/shm")
            .expect("tmpfs backing file")
    } else {
        tempfile::NamedTempFile::new().expect("backing file")
    };
    // Deterministic non-zero content, written once (page-cache resident).
    let block: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let mut written = 0;
    while written < FILE_LEN {
        f.write_all(&block).expect("fill backing");
        written += block.len();
    }
    f.flush().expect("flush backing");
    f
}

fn bench_issue_path(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("bench runtime");

    let file = backing_file();
    let dev = std::sync::Arc::new(NvmeBlockDev::new(file.path().to_str().unwrap()));

    let mut g = c.benchmark_group("nvme_issue");

    // Per-op issue floor: one awaited round trip, ranged-fill sizes.
    for &(label, size) in &[("read_4k_qd1", 4096usize), ("read_64k_qd1", 65536)] {
        g.throughput(Throughput::Bytes(size as u64));
        let dev = std::sync::Arc::clone(&dev);
        g.bench_function(label, |b| {
            let dev = std::sync::Arc::clone(&dev);
            b.to_async(&rt).iter(move || {
                let dev = std::sync::Arc::clone(&dev);
                async move {
                    let out = dev.read_block(0, size).await.expect("bench read");
                    assert_eq!(out.len(), size);
                }
            });
        });
    }

    // Concurrent-fill batch face: 16 in-flight ranged reads joined — the
    // shape where submit batching (one enter per pass) pays.
    g.throughput(Throughput::Bytes(16 * 4096));
    {
        let dev = std::sync::Arc::clone(&dev);
        g.bench_function("read_4k_qd16", |b| {
            let dev = std::sync::Arc::clone(&dev);
            b.to_async(&rt).iter(move || {
                let dev = std::sync::Arc::clone(&dev);
                async move {
                    let futs = (0..16u64).map(|i| {
                        let dev = std::sync::Arc::clone(&dev);
                        async move {
                            let out = dev
                                .read_block(i * 4096 % FILE_LEN as u64, 4096)
                                .await
                                .expect("bench read");
                            assert_eq!(out.len(), 4096);
                        }
                    });
                    futures::future::join_all(futs).await;
                }
            });
        });
    }

    g.finish();
}

criterion_group!(benches, bench_issue_path);
criterion_main!(benches);
