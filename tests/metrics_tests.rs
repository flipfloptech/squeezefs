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
    let _ = METRICS.writeback_superseded_noops.load(Ordering::Relaxed);
    let _ = METRICS
        .writeback_stale_token_retries
        .load(Ordering::Relaxed);
    let _ = METRICS.writeback_orphan_discards.load(Ordering::Relaxed);
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

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("stats_coherence_test").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
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
        ..Default::default()
    };

    // Two full "cat" cycles with counter churn in the middle of each —
    // the storm-session reality (phase snapshots bracket 100 k-op storms).
    for round in 0..2u64 {
        // cat: LOOKUP …
        let _entry = fs
            .lookup(req, 1, std::ffi::OsStr::new(".stats"))
            .await
            .expect("lookup .stats");
        // … OPEN (pins a generation) …
        let opened = fs
            .open(req, STATS_INODE, libc::O_RDONLY as u32, 0)
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

        // Exact-size contract (2026-07-22, padding retirement): the
        // constant-size floor padding that used to blunt the splice-path
        // stale-`i_size` bound was deleted — `cat .config`/`.stats`
        // printed its whitespace tail as garbage. Coherence now rides the
        // snapshot protocol alone: OPEN pins generation + size, GETATTR
        // never regenerates once published, and BOTH virtual inodes reply
        // zero attr/entry TTLs so every fstat reaches the daemon and the
        // kernel's copy bound is always the pinned generation's exact
        // size. The payload itself must carry no tail padding beyond one
        // final newline.
        let raw = std::str::from_utf8(&data.data).expect(".stats is UTF-8");
        assert_eq!(
            raw,
            format!("{}\n", raw.trim_end()),
            "round {round}: .stats must carry NO tail padding beyond one \
             final newline"
        );
        fs.release(req, STATS_INODE, opened.fh, 0, 0, false)
            .await
            .expect("release .stats");
    }
}

/// The 2026-08-04 field tear (`dd` reads the full fresh payload while
/// `cat` clamps at a stale size and tears mid-string on a busy mount):
/// the live session serves each over-uring queue from its own
/// `SqueezefsFilesystem` CLONE, and the Clone impl SPLIT the virtual-
/// inode snapshot state per clone — `open_virtual_files` (DashMap deep
/// copy: an fh pinned by queue A's OPEN misses on queue B's READ, which
/// then regenerates PER READ CALL), `latest_stats_json` (split ArcSwap
/// cells while `latest_stats_size` is genuinely shared — GETATTR's size
/// and READ's bytes come from different generations: the exact
/// torn-prefix face), and `next_virtual_fh` (split counter: two queues
/// mint the SAME fh). The M2 pin above never caught it because it
/// drives ONE instance.
///
/// Contract: the LOOKUP → OPEN → churn → GETATTR → READ → RELEASE
/// snapshot protocol holds ACROSS handler clones — any queue may serve
/// any step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_snapshot_protocol_holds_across_handler_clones() {
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

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("stats_clone_coherence").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
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
    fs.meta_backend = Some(routed);

    // The session shape: one clone per queue (the dispatch loops clone
    // the filesystem exactly like this).
    let q_a = fs.clone();
    let q_b = fs.clone();
    let q_c = fs.clone();

    let req = fuse3::raw::Request {
        unique: 0,
        uid: 1000,
        gid: 1000,
        pid: 1,
        ..Default::default()
    };

    // cat's syscalls land on arbitrary queues: LOOKUP on A, OPEN on A,
    // churn, fstat on B, READ on C, RELEASE on B.
    let _entry = q_a
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .expect("lookup .stats");
    let opened = q_a
        .open(req, STATS_INODE, libc::O_RDONLY as u32, 0)
        .await
        .expect("open .stats");
    // Counter churn between the open and the reader's fstat (the busy-
    // mount reality; digit growth changes the payload length).
    METRICS
        .parked_gate_timeouts
        .fetch_add(987_654_321, Ordering::Relaxed);
    METRICS
        .prefetch_window_hwm
        .fetch_add(123_456_789, Ordering::Relaxed);
    let attr = q_b
        .getattr(req, STATS_INODE, Some(opened.fh), 0)
        .await
        .expect("getattr .stats on another queue clone");
    let first = q_c
        .read(req, STATS_INODE, opened.fh, 0, 16 * 1024 * 1024, 0)
        .await
        .expect("read .stats on a third queue clone");
    assert_eq!(
        attr.attr.size,
        first.data.len() as u64,
        "fstat size (queue B) must equal the bytes the pinned fh serves \
         (queue C) — a mismatch is the field's cat-clamp torn-JSON face"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&first.data)
        .expect("cross-clone full read of .stats must parse as JSON");
    assert!(parsed.get("metrics").is_some(), "snapshot carries metrics");

    // Multi-call read stability (dd's read loop, bs smaller than the
    // payload): two reads on DIFFERENT clones against the same fh must
    // serve ONE generation — a regenerate-per-call serve splices two
    // generations mid-payload.
    METRICS
        .parked_gate_timeouts
        .fetch_add(111_111_111, Ordering::Relaxed);
    let head = q_b
        .read(req, STATS_INODE, opened.fh, 0, 4096, 0)
        .await
        .expect("head read");
    let tail = q_c
        .read(req, STATS_INODE, opened.fh, 4096, 16 * 1024 * 1024, 0)
        .await
        .expect("tail read");
    let mut joined = head.data.to_vec();
    joined.extend_from_slice(&tail.data);
    assert_eq!(
        joined.len() as u64,
        attr.attr.size,
        "split reads across clones must still total the pinned size"
    );
    let _: serde_json::Value = serde_json::from_slice(&joined).expect(
        "split reads across clones must join into ONE parseable \
         generation — a per-call regenerate splices two generations",
    );
    q_b.release(req, STATS_INODE, opened.fh, 0, 0, false)
        .await
        .expect("release .stats");

    // Distinct fh minting across clones: split counters mint the SAME
    // fh on two queues, cross-wiring two readers' pinned generations.
    let o1 = q_a
        .open(req, STATS_INODE, libc::O_RDONLY as u32, 0)
        .await
        .expect("open on A");
    let o2 = q_b
        .open(req, STATS_INODE, libc::O_RDONLY as u32, 0)
        .await
        .expect("open on B");
    assert_ne!(
        o1.fh, o2.fh,
        "two clones minted the SAME virtual fh — split next_virtual_fh \
         counters cross-wire concurrent readers' pinned generations"
    );
    let _ = q_a.release(req, STATS_INODE, o1.fh, 0, 0, false).await;
    let _ = q_b.release(req, STATS_INODE, o2.fh, 0, 0, false).await;
}

/// D3.a (PR M3, design-metadata-throughput §9): the `transport_commit_batch`
/// histogram — COMMIT_AND_FETCH SQEs per queue-worker ring flush — is wired
/// to the `.stats` JSON with the labeled-bucket convention
/// (`meta_commit_group_size` precedent: exact 1–8, then power-of-two), plus
/// the flush/commit totals whose ratio is the mean batch size. The buckets
/// must be PRESENT (zero-valued) even before any over-uring session exists —
/// operators key on the field, and "≈ 1 under load ⇒ batching regressed" is
/// only checkable when the surface always exports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_commit_batch_stats_surface() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new("commit_batch_stats_test")
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
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let json: serde_json::Value =
        serde_json::from_str(&fs.generate_stats_json().await).expect("stats JSON parses");
    let metrics = json
        .get("metrics")
        .and_then(|m| m.as_object())
        .expect("stats carries a metrics object");

    let hist = metrics
        .get("transport_commit_batch")
        .and_then(|h| h.as_object())
        .expect("metrics.transport_commit_batch histogram object (design §9, D3.a)");
    for label in [
        "1", "2", "3", "4", "5", "6", "7", "8", "<=16", "<=32", ">32",
    ] {
        assert!(
            hist.get(label).is_some_and(|v| v.is_u64()),
            "transport_commit_batch bucket '{label}' must always export \
             (zero-valued before any session)"
        );
    }
    assert!(
        metrics
            .get("transport_commit_batch_flushes")
            .is_some_and(|v| v.is_u64()),
        "flush total exports (mean batch size = commits / flushes)"
    );
    assert!(
        metrics
            .get("transport_commit_batch_commits")
            .is_some_and(|v| v.is_u64()),
        "commit total exports (mean batch size = commits / flushes)"
    );
}

/// L3 transport-economy lever B: the queue-worker wake-coalescing pair —
/// `transport_wake_writes` (eventfd writes actually performed) and
/// `transport_wakes_elided` (writes skipped because a wake was already
/// armed) — must ALWAYS export as u64s on the `.stats` metrics surface,
/// zero-valued before any over-uring session exists. The regression
/// signal is the ratio: writes/(writes+elided) ≈ 1 under saturated load
/// means the coalescer stopped eliding (the pre-L3 1.67 eventfd
/// writes/op posture).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_wake_stats_surface() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("wake_stats_test").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let json: serde_json::Value =
        serde_json::from_str(&fs.generate_stats_json().await).expect("stats JSON parses");
    let metrics = json
        .get("metrics")
        .and_then(|m| m.as_object())
        .expect("stats carries a metrics object");

    for key in ["transport_wake_writes", "transport_wakes_elided"] {
        assert!(
            metrics.get(key).is_some_and(|v| v.is_u64()),
            "{key} must always export (zero-valued before any session) — \
             operators key on the elision ratio for the L3 lever-B \
             regression signal"
        );
    }
}

/// PR 6 / N6 (design-nvmeof-target-management §6.9): the daemon
/// `fabric_*` family — `fabric_controllers`, `fabric_ctrl_not_live`
/// (both gauges) and `fabric_ctrl_reconnects` (the sampled-transition
/// counter, undercount caveat pinned in
/// `test_fabric_reconnects_is_sampled_transition_counter_undercounts_bursts`)
/// — must ALWAYS export as u64s, zero-valued on boxes with no fabric
/// controllers (the missing-sysfs zero-case): operators and the
/// fidelity tier's G2 leg key on the field names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fabric_family_stats_surface_exports_zero_valued_without_fabric() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new("fabric_family_stats_test")
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
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let json: serde_json::Value =
        serde_json::from_str(&fs.generate_stats_json().await).expect("stats JSON parses");
    let metrics = json
        .get("metrics")
        .and_then(|m| m.as_object())
        .expect("stats carries a metrics object");

    for field in [
        "fabric_controllers",
        "fabric_ctrl_not_live",
        "fabric_ctrl_reconnects",
    ] {
        assert!(
            metrics.get(field).is_some_and(|v| v.is_u64()),
            "metrics.{field} must always export as u64 (design §6.9 fabric_* family)"
        );
    }
}
