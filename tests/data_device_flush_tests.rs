//! DUR-2 — the data device must have a cache-flush primitive (pre-RC
//! engineering spec §1 DUR-2, P0).
//!
//! Before this work `UringRequest` had exactly two variants (Read, Write)
//! submitted with no `rw_flags`: no `Fsync` op, no FUA, no `O_SYNC`, and
//! no VWC probe anywhere in the tree. `O_DIRECT` bypasses the page cache,
//! **not** the device's volatile write cache, so for striped write-through
//! the sequence DMA → block-map merge → meta journal → meta `fdatasync`
//! ordered nothing at all on the data device: power loss left durable
//! metadata naming a block whose contents were still volatile.
//!
//! What this suite pins:
//!
//! 1. `NvmeBlockDev::flush()` exists and is a real barrier (checked
//!    against the TEST-1 harness in `data_device_power_cut_tests.rs`);
//! 2. concurrent flushes **coalesce** through the existing
//!    `SyncCoalescer` discipline (N callers → far fewer device barriers),
//!    with the counters that prove it (`data_device_syncs` vs
//!    `data_device_sync_requests`);
//! 3. the VWC probe classifies a device and is surfaced as the
//!    `data_volume_write_cache` gauge beside `meta_volume_atomicity_physical`;
//! 4. the buffered-degrade arm: `O_DIRECT` open failure **fails loud**
//!    (decision (a) of the §6.1 design review — the "fix uring or fail
//!    loud" house law). Today's prohibited posture was degrade-silently
//!    plus nothing ever flushing those writes.

use squeezefs::fuse_client::METRICS;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::write_cache::WriteCacheClass;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

fn backing(len: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("backing tempfile");
    f.as_file().set_len(len).expect("size backing tempfile");
    f
}

/// A lone flush issues exactly one device barrier and reports it.
#[tokio::test]
async fn test_flush_issues_one_data_device_barrier() {
    let tmp = backing(8 * 1024 * 1024);
    let dev = NvmeBlockDev::new(tmp.path().to_str().unwrap());

    dev.write_block(0, bytes::Bytes::from(vec![0x31u8; 4096]))
        .await
        .unwrap();

    let syncs0 = METRICS.data_device_syncs.load(Ordering::Relaxed);
    let reqs0 = METRICS.data_device_sync_requests.load(Ordering::Relaxed);
    dev.flush().await.expect("data-device barrier");
    assert_eq!(
        METRICS.data_device_syncs.load(Ordering::Relaxed) - syncs0,
        1,
        "one flush must issue exactly one device barrier"
    );
    assert_eq!(
        METRICS
            .data_device_sync_requests
            .load(Ordering::Relaxed)
            .saturating_sub(reqs0),
        1,
        "the request counter must count the caller"
    );
}

/// Concurrent flushes coalesce through the existing `SyncCoalescer`
/// discipline: 64 callers must not cost 64 device barriers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_flushes_coalesce() {
    let tmp = backing(8 * 1024 * 1024);
    let dev = Arc::new(NvmeBlockDev::new(tmp.path().to_str().unwrap()));

    dev.write_block(0, bytes::Bytes::from(vec![0x32u8; 4096]))
        .await
        .unwrap();

    let syncs0 = METRICS.data_device_syncs.load(Ordering::Relaxed);
    let reqs0 = METRICS.data_device_sync_requests.load(Ordering::Relaxed);

    let n = 64usize;
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let d = dev.clone();
        handles.push(tokio::spawn(async move { d.flush().await }));
    }
    for h in handles {
        h.await.unwrap().expect("coalesced barrier must succeed");
    }

    let syncs = METRICS.data_device_syncs.load(Ordering::Relaxed) - syncs0;
    let reqs = METRICS.data_device_sync_requests.load(Ordering::Relaxed) - reqs0;
    assert_eq!(reqs, n as u64, "every caller counts as a request");
    assert!(
        syncs >= 1 && syncs <= n as u64,
        "barrier count {syncs} out of range"
    );
    assert!(
        syncs < n as u64,
        "{n} concurrent flushes issued {syncs} device barriers — the \
         SyncCoalescer discipline is not engaged"
    );
}

/// Flushing a device with nothing in flight is still a real barrier (it
/// must never silently no-op — a caller cannot know what a sibling wrote).
#[tokio::test]
async fn test_flush_on_idle_device_still_barriers() {
    let tmp = backing(4 * 1024 * 1024);
    let dev = NvmeBlockDev::new(tmp.path().to_str().unwrap());
    let syncs0 = METRICS.data_device_syncs.load(Ordering::Relaxed);
    dev.flush().await.expect("idle barrier");
    assert_eq!(
        METRICS.data_device_syncs.load(Ordering::Relaxed) - syncs0,
        1
    );
}

/// The VWC probe classifies a backing file as `file-backed` and a missing
/// path as `unknown` — never a guess (the `meta_volume_atomicity` probe's
/// discipline).
#[test]
fn test_write_cache_probe_classifies_honestly() {
    let tmp = backing(1024 * 1024);
    assert_eq!(
        squeezefs::write_cache::probe_data_volume(tmp.path()),
        WriteCacheClass::FileBacked
    );
    assert_eq!(
        squeezefs::write_cache::probe_data_volume(std::path::Path::new(
            "/nonexistent/data/volume"
        )),
        WriteCacheClass::Unknown
    );
    // sysfs `queue/write_cache` strings, verbatim.
    assert_eq!(
        squeezefs::write_cache::classify_write_cache(Some("write back")),
        WriteCacheClass::WriteBack
    );
    assert_eq!(
        squeezefs::write_cache::classify_write_cache(Some("write through")),
        WriteCacheClass::WriteThrough
    );
    assert_eq!(
        squeezefs::write_cache::classify_write_cache(None),
        WriteCacheClass::Unknown
    );
    // Stats-surface strings are a dashboard contract.
    assert_eq!(WriteCacheClass::WriteBack.as_str(), "write-back");
    assert_eq!(WriteCacheClass::WriteThrough.as_str(), "write-through");
    assert_eq!(WriteCacheClass::FileBacked.as_str(), "file-backed");
    assert_eq!(WriteCacheClass::Unknown.as_str(), "unknown");
}

/// Every device carries its probed VWC class for the stats gauge.
#[test]
fn test_device_reports_its_write_cache_class() {
    let tmp = backing(1024 * 1024);
    let dev = NvmeBlockDev::new(tmp.path().to_str().unwrap());
    assert_eq!(dev.write_cache(), WriteCacheClass::FileBacked);
}

/// The buffered-degrade arm, decision (a): a device that cannot be opened
/// `O_DIRECT` fails LOUD at open time instead of silently degrading into
/// a mode where nothing ever flushes.
#[test]
fn test_open_checked_refuses_a_device_without_o_direct() {
    // A directory can never be opened read-write, let alone O_DIRECT.
    let dir = tempfile::tempdir().unwrap();
    let msg = match NvmeBlockDev::open_checked(dir.path().to_str().unwrap()) {
        Ok(_) => panic!("a non-openable data volume must fail loud, never degrade"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("O_DIRECT"),
        "the refusal must name the O_DIRECT requirement: {msg}"
    );
}

/// A healthy backing file passes the checked open and serves I/O.
#[tokio::test]
async fn test_open_checked_accepts_a_direct_io_capable_device() {
    let tmp = backing(4 * 1024 * 1024);
    let dev =
        NvmeBlockDev::open_checked(tmp.path().to_str().unwrap()).expect("O_DIRECT-capable backing");
    dev.write_block(0, bytes::Bytes::from(vec![0x44u8; 4096]))
        .await
        .unwrap();
    dev.flush().await.unwrap();
    assert_eq!(
        &dev.read_block(0, 4096).await.unwrap()[..],
        &[0x44u8; 4096][..]
    );
}
