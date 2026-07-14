//! P3-1: metrics counters are lock-free and visible.

use squeezefs::fuse_client::METRICS;
use std::sync::atomic::Ordering;

#[test]
fn test_layout_and_admission_metrics_increment() {
    let before_inline = METRICS.layout_inline_writes.load(Ordering::Relaxed);
    METRICS.layout_inline_writes.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS.layout_inline_writes.load(Ordering::Relaxed),
        before_inline + 1
    );

    let before_adm = METRICS.bg_spawn_admitted.load(Ordering::Relaxed);
    METRICS.bg_spawn_admitted.fetch_add(3, Ordering::Relaxed);
    assert_eq!(
        METRICS.bg_spawn_admitted.load(Ordering::Relaxed),
        before_adm + 3
    );

    let before_full = METRICS.uring_queue_full.load(Ordering::Relaxed);
    METRICS.uring_queue_full.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS.uring_queue_full.load(Ordering::Relaxed),
        before_full + 1
    );

    // PR 2 (zero-copy write-path §5.6): the pooled-buffer alignment-contract
    // violation detector must be a live, lock-free counter.
    let before_fallbacks = METRICS
        .nvme_unaligned_write_fallbacks
        .load(Ordering::Relaxed);
    METRICS
        .nvme_unaligned_write_fallbacks
        .fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS
            .nvme_unaligned_write_fallbacks
            .load(Ordering::Relaxed),
        before_fallbacks + 1
    );
}

#[test]
fn test_lease_metrics_fields_exist() {
    // Smoke: fields are readable (no panics / alignment issues).
    let _ = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let _ = METRICS.lease_acquire_fail.load(Ordering::Relaxed);
    let _ = METRICS.writeback_retry_exhaustions.load(Ordering::Relaxed);
    let _ = METRICS.layout_staged_writes.load(Ordering::Relaxed);
    let _ = METRICS.layout_striped_writes.load(Ordering::Relaxed);
}

/// The `.stats` snapshot-coherence contract (PR M2): one reader's
/// LOOKUP → OPEN → fstat(GETATTR) → read sequence must yield a size that
/// EXACTLY matches the bytes the open fh serves, even when counters churn
/// between the open and the fstat — the kernel copies exactly `i_size`
/// bytes (`cat` uses `copy_file_range`), so a GETATTR that regenerates the
/// payload and republishes a *different* size makes every mid-churn
/// snapshot a torn JSON prefix. Measured live in the M2 acceptance
/// session: with the op-profile rig enabled (a ~40 KB stats payload),
/// 9 of 10 mdstorm phase snapshots were unparseable prefixes clamped at
/// the stale size. GETATTR on the virtual inodes must therefore report
/// the published generation's size, never regenerate-and-republish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_snapshot_getattr_size_matches_served_bytes_under_churn() {
    use fuse3::raw::prelude::Filesystem;
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::{SqueezefsFilesystem, STATS_INODE};
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    async fn open_v3_meta(
        path: &std::path::Path,
    ) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
        squeezefs::meta_backend::kv::builder::format_v3(
            path,
            256 * 1024 * 1024,
            &squeezefs::meta_backend::kv::builder::FormatV3Options {
                node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: None,
            },
        )
        .await
        .expect("format v3 meta volume");
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
            .await
            .expect("open v3 meta volume")
    }

    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "stats_coherence_test")
            .await
            .unwrap(),
    );
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path()).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    let req = fuse3::raw::Request {
        unique: 0,
        uid: 1000,
        gid: 1000,
        pid: 1,
    };

    // Two full "cat" cycles with counter churn in the middle of each —
    // the storm-session reality (phase snapshots bracket 100 k-op storms).
    let mut prev_size: Option<u64> = None;
    for round in 0..2u64 {
        // cat: LOOKUP …
        let _entry = fs
            .lookup(req, 1, std::ffi::OsStr::new(".stats"))
            .await
            .expect("lookup .stats");
        // … OPEN (pins a generation) …
        let opened = fs
            .open(req, STATS_INODE, libc::O_RDONLY as u32)
            .await
            .expect("open .stats");
        // … counters churn (a storm is running; here: force a size-visible
        // digit growth in several fields) …
        METRICS
            .parked_gate_timeouts
            .fetch_add(987_654_321 * (round + 1), Ordering::Relaxed);
        METRICS
            .prefetch_window_hwm
            .fetch_add(123_456_789 * (round + 1), Ordering::Relaxed);
        // … fstat (GETATTR — the kernel's copy bound) …
        let attr = fs
            .getattr(req, STATS_INODE, Some(opened.fh), 0)
            .await
            .expect("getattr .stats");
        // … read to EOF from the pinned fh.
        let data = fs
            .read(req, STATS_INODE, opened.fh, 0, 16 * 1024 * 1024, 0)
            .await
            .expect("read .stats");
        assert_eq!(
            attr.attr.size,
            data.data.len() as u64,
            "round {round}: fstat size must equal the bytes the open fh \
             serves — anything else tears `cat` at the stale bound"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&data.data)
            .expect("a full-size read of .stats must parse as JSON");
        assert!(parsed.get("metrics").is_some(), "snapshot carries metrics");

        // The splice-path reality (measured: `cat` reads via splice, whose
        // read bound is a possibly ONE-GENERATION-STALE `i_size` no matter
        // what the daemon replies — FOPEN_DIRECT_IO exempts only the
        // plain-`read(2)` path, and 4 KiB quantization still tore whenever
        // the storm grew the payload across a quantum): the size must be
        // CONSTANT, not merely quantized. `.stats` pads to a fixed 256 KiB
        // floor (trailing whitespace — legal JSON; 6× headroom over the
        // rig-enabled ~41 KB payload), so `i_size` never moves between
        // generations and every stale bound covers the whole payload.
        assert_eq!(
            attr.attr.size,
            256 * 1024,
            "round {round}: .stats payload size is CONSTANT (256 KiB floor) \
             so a stale-i_size splice bound always covers the full payload"
        );
        if let Some(prev) = prev_size {
            let clamped = &data.data[..std::cmp::min(prev as usize, data.data.len())];
            let clamped_parsed: serde_json::Value = serde_json::from_slice(clamped).expect(
                "a stale-i_size-clamped snapshot must still parse (padding-only truncation)",
            );
            assert!(clamped_parsed.get("metrics").is_some());
        }
        prev_size = Some(attr.attr.size);
        fs.release(req, STATS_INODE, opened.fh, 0, 0, false)
            .await
            .expect("release .stats");
    }
}
