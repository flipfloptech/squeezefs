//! The NvmeBlockDev funnel's issue economy (e2e audit R-3 — read board #3
//! "fill-issue economy", `docs/design-e2e-perf-audit.md` §3.3 / Appendix
//! B; the stock-kernel row's `read_fill_phase_ns.dev_queue` = 500 µs
//! against `dev_service` = 285 µs in `.benchmarks/2026-09-02-r1-device-
//! read-executor.md` §4.3).
//!
//! The shape it kills: the lane worker pumped its request channel, then
//! parked in `submit_and_wait(1)` — a request that arrived while ANY
//! fill was in flight sat in the channel until an UNRELATED completion
//! woke the worker, so its issue paid another op's device service before
//! its own began (`dev_queue` ≈ one device RTT under load). The worker
//! now parks on its ring with a request-arrival wake in the ring too
//! (an eventfd read SQE the enqueue side writes through a coalescer —
//! the fuse3 queue worker's loom-verified drain → disarm → scan
//! protocol), so ONE `io_uring_enter` both flushes the pass's SQEs and
//! returns on the first of {a completion, a new request}.
//!
//! Contracts (red-first against the old worker):
//! 1. **No fill's issue waits on an unrelated fill's completion**: with
//!    one deliberately slow read in flight (the `set_test_read_slow` seam
//!    — a linked `TIMEOUT` ahead of the read's SQE, so the DEVICE
//!    completion is late while the worker is free), a second read issued
//!    afterwards completes at device speed, long before the slow one.
//! 2. **Enters are batched**: a concurrent burst of N fills rides fewer
//!    than N ring enters (`dev_enters` Δ < `dev_fills` Δ = N), and every
//!    completion drain that resolved ≥ 1 fill is one `dev_wake_batches`.
//! 3. **The idle lane wakes on its first request** through the ring
//!    (`dev_wake_writes` moves): no request ever waits for a device event
//!    to be noticed.

use squeezefs::fuse_client::METRICS;
use squeezefs::nvme_dev::{self, NvmeBlockDev};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const BLOCK: usize = 65536;

fn pattern_file(len: usize) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("temp backing file");
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    f.write_all(&data).expect("write pattern");
    f.flush().expect("flush");
    f
}

/// The funnel counters are process-global: read them as one snapshot.
fn funnel() -> (u64, u64, u64, u64) {
    (
        METRICS.dev_enters.load(Ordering::Relaxed),
        METRICS.dev_fills.load(Ordering::Relaxed),
        METRICS.dev_wake_batches.load(Ordering::Relaxed),
        METRICS.dev_wake_writes.load(Ordering::Relaxed),
    )
}

/// Tests share the process-global seams + counters; serialize.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_fill_completes_while_a_slow_fill_holds_the_lane() {
    let _g = SERIAL.lock().await;
    nvme_dev::set_test_read_slow(0, 0);
    let file = pattern_file(4 * BLOCK);
    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());

    // The next read's DEVICE completion is 400 ms late (linked TIMEOUT).
    nvme_dev::set_test_read_slow(1, 400);
    let slow = {
        let dev = dev.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            let b = dev.read_block(0, BLOCK).await.expect("slow read");
            (t0.elapsed(), b)
        })
    };
    // Let the slow read reach the ring and park the worker on it.
    tokio::time::sleep(Duration::from_millis(30)).await;

    let t0 = Instant::now();
    let fast = dev
        .read_block(BLOCK as u64, BLOCK)
        .await
        .expect("fast read");
    let fast_took = t0.elapsed();
    assert_eq!(fast.len(), BLOCK);
    assert_eq!(fast[0], ((BLOCK) % 251) as u8, "the fast read's bytes");
    assert!(
        fast_took < Duration::from_millis(150),
        "a fill issued while an unrelated slow fill is in flight must not wait for that \
         fill's completion to be issued: fast read took {fast_took:?} (slow fill = 400 ms)"
    );

    let (slow_took, slow_bytes) = slow.await.expect("slow task");
    assert!(
        slow_took >= Duration::from_millis(380),
        "the seam held the slow read's completion ({slow_took:?})"
    );
    assert_eq!(slow_bytes.len(), BLOCK);
    assert_eq!(slow_bytes[1], 1);
    nvme_dev::set_test_read_slow(0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_burst_rides_fewer_enters_than_fills() {
    let _g = SERIAL.lock().await;
    nvme_dev::set_test_read_slow(0, 0);
    const N: usize = 64;
    let file = pattern_file(N * BLOCK);
    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    // Prime the lane so the burst below is the measured population.
    dev.read_block(0, BLOCK).await.expect("prime");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (e0, f0, w0, _) = funnel();
    let mut hs = Vec::with_capacity(N);
    for i in 0..N {
        let dev = dev.clone();
        hs.push(tokio::spawn(async move {
            dev.read_block((i * BLOCK) as u64, BLOCK)
                .await
                .expect("burst read")
        }));
    }
    for (i, h) in hs.into_iter().enumerate() {
        let b = h.await.expect("burst task");
        assert_eq!(b.len(), BLOCK);
        assert_eq!(b[0], ((i * BLOCK) % 251) as u8);
    }
    let (e1, f1, w1, _) = funnel();
    let (enters, fills, batches) = (e1 - e0, f1 - f0, w1 - w0);
    assert_eq!(fills, N as u64, "every burst read is one fill");
    assert!(
        enters < fills,
        "a concurrent burst must share ring enters: {enters} enters for {fills} fills"
    );
    assert!(
        (1..=fills).contains(&batches),
        "completion drains resolving ≥ 1 fill: {batches} for {fills} fills"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_lane_wakes_through_the_ring_on_its_first_request() {
    let _g = SERIAL.lock().await;
    nvme_dev::set_test_read_slow(0, 0);
    let file = pattern_file(2 * BLOCK);
    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    // The worker is parked with nothing in flight.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_, f0, _, k0) = funnel();
    let t0 = Instant::now();
    let b = dev
        .read_block(BLOCK as u64, BLOCK)
        .await
        .expect("first read");
    let took = t0.elapsed();
    assert_eq!(b.len(), BLOCK);
    let (_, f1, _, k1) = funnel();
    assert_eq!(f1 - f0, 1);
    assert!(
        k1 > k0,
        "the first request into an idle lane wakes the parked worker through the ring eventfd"
    );
    assert!(
        took < Duration::from_secs(1),
        "idle wake is prompt ({took:?})"
    );
    // Teardown: dropping the device must wake the parked worker (the
    // disconnect rides the same eventfd) and join it — no hang.
    let t0 = Instant::now();
    drop(dev);
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "worker teardown from the parked state is prompt"
    );
}
