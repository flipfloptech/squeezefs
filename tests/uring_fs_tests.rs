//! `crate::uring_fs` worker contracts.
//!
//! The worker pool must (a) expose a batched write API so one logical
//! commit (WAL record + sector images) is one queue message instead of a
//! round-trip per sector, and (b) stay byte-exact under heavy concurrent
//! mixed I/O — the pipelined worker keeps many operations in flight on one
//! ring, and any state-machine slip (continuation resubmit, slot reuse,
//! fd-cache eviction of an in-flight fd) shows up here as corruption.

use squeezefs::uring_fs;
use tempfile::tempdir;

/// Contract 1: `write_at_batch` lands every (offset, bytes) pair durably in
/// one call; entries may be unordered and non-contiguous; the empty batch
/// is a no-op Ok.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_write_at_batch_lands_all_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("batch.bin");

    // Pre-size with a known pattern so untouched ranges are provable.
    uring_fs::write_all(&path, vec![0xEEu8; 64 * 1024])
        .await
        .expect("seed write");

    // Unordered, non-contiguous, varied sizes (sub-sector and multi-sector).
    let ops: Vec<(u64, bytes::Bytes)> = vec![
        (40960, bytes::Bytes::from(vec![0xC3u8; 12288])),
        (0, bytes::Bytes::from(vec![0xA1u8; 512])),
        (8192, bytes::Bytes::from(vec![0xB2u8; 4096])),
        (61440, bytes::Bytes::from(vec![0xD4u8; 100])),
    ];
    uring_fs::write_at_batch(&path, ops.clone())
        .await
        .expect("batch write");

    for (off, data) in &ops {
        let got = uring_fs::read_at(&path, *off, data.len())
            .await
            .expect("read back");
        assert_eq!(&got[..], &data[..], "batch entry at offset {off} corrupt");
    }
    // Untouched gap keeps the seed pattern.
    let gap = uring_fs::read_at(&path, 512, 1024).await.expect("gap read");
    assert!(
        gap.iter().all(|&b| b == 0xEE),
        "batch write touched bytes outside its entries"
    );

    uring_fs::write_at_batch(&path, Vec::new())
        .await
        .expect("empty batch is a no-op Ok");
}

/// Contract 2: batch errors are loud — a non-openable path fails the whole
/// batch instead of silently dropping entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_write_at_batch_propagates_errors() {
    let res = uring_fs::write_at_batch(
        "/proc/definitely/not/a/writable/path.bin",
        vec![(0, bytes::Bytes::from_static(b"x"))],
    )
    .await;
    assert!(res.is_err(), "unwritable path must fail the batch");
}

/// Contract 3: byte-exactness under heavy concurrent mixed I/O. 64 tasks
/// hammer 8 files with interleaved write_at / read_at / fdatasync /
/// write_at_batch; every read must observe exactly the bytes its own task
/// last wrote to its own region (regions are disjoint per task).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_mixed_io_byte_exact() {
    let dir = tempdir().unwrap();
    let files: Vec<_> = (0..8)
        .map(|i| dir.path().join(format!("stress_{i}.bin")))
        .collect();
    for f in &files {
        uring_fs::write_all(f, vec![0u8; 256 * 1024])
            .await
            .expect("seed");
    }

    let mut tasks = Vec::new();
    for t in 0..64u64 {
        let path = files[(t % 8) as usize].clone();
        tasks.push(tokio::spawn(async move {
            // Disjoint 4 KiB region per task within its file.
            let base = (t / 8) * 32 * 1024;
            for round in 0..20u64 {
                let fill = (t as u8) ^ (round as u8) | 1;
                let data = vec![fill; 4096];
                if round % 3 == 0 {
                    uring_fs::write_at_batch(
                        &path,
                        vec![
                            (base, bytes::Bytes::from(data[..2048].to_vec())),
                            (base + 2048, bytes::Bytes::from(data[2048..].to_vec())),
                        ],
                    )
                    .await
                    .expect("batch");
                } else {
                    uring_fs::write_at(&path, base, bytes::Bytes::from(data.clone()))
                        .await
                        .expect("write");
                }
                if round % 5 == 0 {
                    uring_fs::fdatasync(&path).await.expect("fdatasync");
                }
                let got = uring_fs::read_at(&path, base, 4096).await.expect("read");
                assert_eq!(
                    got.len(),
                    4096,
                    "task {t} round {round}: short read under load"
                );
                assert!(
                    got.iter().all(|&b| b == fill),
                    "task {t} round {round}: read bytes from another op's buffer"
                );
            }
        }));
    }
    for task in tasks {
        tokio::time::timeout(std::time::Duration::from_secs(60), task)
            .await
            .expect("stress task hung (pipeline stalled)")
            .expect("stress task panicked");
    }
}

/// Contract 4: many files churning through the worker's fd cache while I/O
/// is in flight (eviction must never close an fd with an outstanding op).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_fd_cache_churn_under_load() {
    let dir = tempdir().unwrap();
    let mut tasks = Vec::new();
    for t in 0..16u32 {
        let dir_path = dir.path().to_path_buf();
        tasks.push(tokio::spawn(async move {
            for i in 0..200u32 {
                let p = dir_path.join(format!("churn_{t}_{i}.bin"));
                let fill = ((t * 7 + i) % 251) as u8;
                uring_fs::write_at(&p, 0, bytes::Bytes::from(vec![fill; 1024]))
                    .await
                    .expect("write");
                let got = uring_fs::read_at(&p, 0, 1024).await.expect("read");
                assert!(
                    got.iter().all(|&b| b == fill),
                    "task {t} file {i}: fd-cache churn corrupted I/O"
                );
            }
        }));
    }
    for task in tasks {
        tokio::time::timeout(std::time::Duration::from_secs(60), task)
            .await
            .expect("churn task hung")
            .expect("churn task panicked");
    }
}

/// A single logical batch LARGER than the worker's submission-queue ring
/// (512) must complete: SQ-full during batch admission is normal
/// backpressure — the reactor flushes the SQ to the kernel and keeps
/// pushing — never an error surfaced to the caller. Regression: the
/// quick-format ghost sweep sent 512-entry batches (== ring size); with
/// any co-resident op on the same worker the push overflowed and format
/// failed loudly ("uring-fs submission queue overflow").
#[tokio::test]
async fn test_write_at_batch_larger_than_ring_completes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big_batch.bin");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(700 * 4096)
        .unwrap();

    // 600 sector writes in ONE message: > the 512-deep SQ ring.
    let ops: Vec<(u64, bytes::Bytes)> = (0..600u64)
        .map(|i| {
            let mut sector = vec![0u8; 4096];
            sector[..8].copy_from_slice(&i.to_le_bytes());
            (i * 4096, bytes::Bytes::from(sector))
        })
        .collect();
    squeezefs::uring_fs::write_at_batch(&path, ops)
        .await
        .expect("a batch larger than the SQ ring must complete via submit-on-full");

    // Byte-exactness spot checks across the range.
    for i in [0u64, 255, 511, 512, 599] {
        let got = squeezefs::uring_fs::read_at(&path, i * 4096, 8)
            .await
            .unwrap();
        assert_eq!(
            u64::from_le_bytes(got[..].try_into().unwrap()),
            i,
            "entry {i} must have landed"
        );
    }
}
