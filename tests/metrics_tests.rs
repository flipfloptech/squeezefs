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

    // Per-lookup generations across clones (the wb-cache face's fix rides
    // the SAME one-cell law): LOOKUP on clone A mints a generation ino;
    // OPEN on clone B and READ on clone C must serve exactly the payload
    // clone A minted — a per-clone registry would miss on B (ESTALE or a
    // regenerated different-size payload, the torn-JSON face again).
    let entry = q_a
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .expect("lookup .stats on clone A");
    METRICS
        .parked_gate_timeouts
        .fetch_add(222_222_222, Ordering::Relaxed);
    let og = q_b
        .open(req, entry.attr.ino, libc::O_RDONLY as u32, 0)
        .await
        .expect("open the clone-A-minted generation ino on clone B");
    let data = q_c
        .read(req, entry.attr.ino, og.fh, 0, 16 * 1024 * 1024, 0)
        .await
        .expect("read the generation ino on clone C");
    assert_eq!(
        data.data.len() as u64,
        entry.attr.size,
        "clone C must serve exactly the generation clone A minted — a \
         split registry regenerates a different-size payload mid-churn"
    );
    let _: serde_json::Value =
        serde_json::from_slice(&data.data).expect("cross-clone generation read must parse as JSON");
    let _ = q_b.release(req, entry.attr.ino, og.fh, 0, 0, false).await;
}

/// The 2026-08-04 WRITEBACK-CACHE face — the THIRD mechanism of the
/// torn-`.stats` bug (after the cross-clone split and the retained kernel
/// pages, both fixed on this branch): on default mounts the kernel
/// negotiates FUSE_WRITEBACK_CACHE, under which the kernel OWNS `i_size`
/// for regular files and DISCARDS the size in every attr reply after
/// inode instantiation. Measured live: daemon GETATTR replies sized
/// 71352 → 71350 → 71349 while the kernel kept serving a frozen 71352 —
/// `cat` (the splice path) clamps at the frozen size and tears the JSON
/// mid-string (39/40 torn under counter churn). No attr-reply protocol
/// can fix a FIXED ino under that ownership; the fix is a FRESH kernel
/// inode per LOOKUP, whose wb-cache size authority initializes from the
/// LOOKUP entry's attr size and never needs to change — that generation's
/// payload is IMMUTABLE.
///
/// Contract pinned here (the in-process wb-cache-shape pin):
/// 1. every `.stats`/`.config` LOOKUP mints a DIFFERENT ino, from the
///    reserved generation range `0xffff_ffff_0000_0000 ..=
///    0xffff_ffff_ffff_fff0` — disjoint from real inos (v3 minting is
///    monotonic-from-1, no reuse, ino cap ≥ 100 M ≪ 2^32) and from the
///    canonical STATS_INODE / CONFIG_INODE / ino 1 (asserted with
///    LITERALS so the reserved values themselves are the pin);
/// 2. GETATTR / OPEN / READ (offset-sliced) on a generation ino serve
///    exactly THAT generation's bytes — size frozen per ino, parsing as
///    whole JSON — even under counter churn after the mint;
/// 3. FORGET (and BATCH_FORGET) retire the generation: subsequent
///    GETATTR / OPEN answer an error (ESTALE — the honest observable for
///    registry removal; no test-only registry accessor needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_lookup_mints_fresh_generation_inos_wb_cache_face() {
    use fuse3::raw::prelude::Filesystem;
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::{SqueezefsFilesystem, CONFIG_INODE, STATS_INODE};
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
    let ba = Arc::new(BlockAllocator::new("stats_gen_ino_test").await.unwrap());
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

    let req = fuse3::raw::Request {
        unique: 0,
        uid: 1000,
        gid: 1000,
        pid: 1,
        ..Default::default()
    };

    // 1. Two lookups with counter churn between — the wb-cache face: a
    //    FIXED ino would hand the kernel a frozen size authority.
    let e1 = fs
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .expect("first lookup .stats");
    METRICS
        .parked_gate_timeouts
        .fetch_add(987_654_321, Ordering::Relaxed);
    METRICS
        .prefetch_window_hwm
        .fetch_add(123_456_789, Ordering::Relaxed);
    let e2 = fs
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .expect("second lookup .stats");
    assert_ne!(
        e1.attr.ino, e2.attr.ino,
        "every .stats LOOKUP must mint a FRESH generation ino — a fixed \
         ino hands the wb-cache kernel a frozen i_size authority that \
         tears cat/splice reads mid-string under counter churn"
    );

    // Reserved-range disjointness (LITERALS on purpose — the reserved
    // values are the contract): real inos are monotonic-from-1 with no
    // reuse (v3), so the range can never collide; the canonical virtual
    // inos and ino 1 sit outside it.
    const GEN_FIRST: u64 = 0xffff_ffff_0000_0000;
    const GEN_LAST: u64 = 0xffff_ffff_ffff_fff0;
    for (label, e) in [("first", &e1), ("second", &e2)] {
        let ino = e.attr.ino;
        assert!(
            (GEN_FIRST..=GEN_LAST).contains(&ino),
            "{label} lookup's generation ino {ino:#x} must come from the \
             reserved range {GEN_FIRST:#x}..={GEN_LAST:#x}"
        );
        assert!(
            ino < STATS_INODE,
            "{label}: the range must end BELOW the canonical virtual inos \
             (STATS_INODE is the lower of the two)"
        );
        assert_ne!(ino, STATS_INODE, "{label}: gen ino ≠ canonical stats");
        assert_ne!(ino, CONFIG_INODE, "{label}: gen ino ≠ canonical config");
        assert!(ino > 1, "{label}: gen ino ≠ root");
    }

    // 2. Each generation ino serves EXACTLY its entry's bytes — size
    //    frozen per ino, offset-sliced reads joining into one JSON —
    //    under further churn (the payload is immutable by construction).
    METRICS
        .parked_gate_timeouts
        .fetch_add(111_111_111, Ordering::Relaxed);
    for (label, e) in [("first", &e1), ("second", &e2)] {
        let ino = e.attr.ino;
        let ga = fs
            .getattr(req, ino, None, 0)
            .await
            .unwrap_or_else(|err| panic!("{label}: getattr gen ino: {err}"));
        assert_eq!(
            ga.attr.size, e.attr.size,
            "{label}: a generation's size is FROZEN at its entry size — \
             the whole point of per-lookup inos under wb-cache"
        );
        assert_eq!(ga.attr.ino, ino, "{label}: attr names the gen ino");
        let opened = fs
            .open(req, ino, libc::O_RDONLY as u32, 0)
            .await
            .unwrap_or_else(|err| panic!("{label}: open gen ino: {err}"));
        let head = fs
            .read(req, ino, opened.fh, 0, 4096, 0)
            .await
            .unwrap_or_else(|err| panic!("{label}: head read: {err}"));
        let tail = fs
            .read(req, ino, opened.fh, 4096, 16 * 1024 * 1024, 0)
            .await
            .unwrap_or_else(|err| panic!("{label}: tail read: {err}"));
        let mut joined = head.data.to_vec();
        joined.extend_from_slice(&tail.data);
        assert_eq!(
            joined.len() as u64,
            e.attr.size,
            "{label}: offset-sliced reads must total exactly the entry size"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&joined).unwrap_or_else(|err| {
            panic!("{label}: a generation's reads must join into whole JSON: {err}")
        });
        assert!(parsed.get("metrics").is_some(), "{label}: carries metrics");
        fs.release(req, ino, opened.fh, 0, 0, false)
            .await
            .unwrap_or_else(|err| panic!("{label}: release: {err}"));
    }

    // 3. FORGET retires a generation (kernel dentry death is the
    //    lifecycle; ESTALE afterward is the honest registry observable).
    fs.forget(req, e1.attr.ino, 1).await;
    assert!(
        fs.getattr(req, e1.attr.ino, None, 0).await.is_err(),
        "GETATTR on a FORGETted generation must fail (ESTALE) — the \
         registry entry is retired with the kernel inode"
    );
    assert!(
        fs.open(req, e1.attr.ino, libc::O_RDONLY as u32, 0)
            .await
            .is_err(),
        "OPEN on a FORGETted generation must fail (ESTALE)"
    );
    // …and BATCH_FORGET must sweep exactly like N FORGETs.
    fs.batch_forget(req, &[(e2.attr.ino, 1)]).await;
    assert!(
        fs.getattr(req, e2.attr.ino, None, 0).await.is_err(),
        "GETATTR on a BATCH_FORGETted generation must fail (ESTALE)"
    );

    // `.config` mints from the same range, same protocol.
    let c1 = fs
        .lookup(req, 1, std::ffi::OsStr::new(".config"))
        .await
        .expect("first lookup .config");
    let c2 = fs
        .lookup(req, 1, std::ffi::OsStr::new(".config"))
        .await
        .expect("second lookup .config");
    assert_ne!(
        c1.attr.ino, c2.attr.ino,
        ".config lookups must mint fresh generation inos too"
    );
    assert!(
        (GEN_FIRST..=GEN_LAST).contains(&c1.attr.ino),
        ".config generation ino comes from the reserved range"
    );
    assert_ne!(
        c1.attr.ino, e2.attr.ino,
        ".config and .stats generations never collide"
    );
    let opened = fs
        .open(req, c1.attr.ino, libc::O_RDONLY as u32, 0)
        .await
        .expect("open .config gen ino");
    let data = fs
        .read(req, c1.attr.ino, opened.fh, 0, 16 * 1024 * 1024, 0)
        .await
        .expect("read .config gen ino");
    assert_eq!(
        data.data.len() as u64,
        c1.attr.size,
        ".config generation serves exactly its entry size"
    );
    let _: serde_json::Value =
        serde_json::from_slice(&data.data).expect(".config generation parses as JSON");
    fs.release(req, c1.attr.ino, opened.fh, 0, 0, false)
        .await
        .expect("release .config gen ino");
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

/// The FUSE-zc engagement LEDGER (K1 kill + write-side campaign,
/// 2026-08-06): the full family must ALWAYS export as u64s on the
/// `.stats` **`metrics`** object — armed or not, on every mount. The
/// nesting is the contract: every zc key lives UNDER `metrics` (like
/// every other counter family), never at the JSON top level — the
/// 2026-08-06 write-side campaign found a top-level scan reading {} on
/// a live armed mount and this pin is what makes that a tooling error
/// rather than an export regression. A bracket row whose deltas cannot
/// be read from these keys is INVALID by repo law.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_zc_ledger_always_exports_under_metrics() {
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
    let ba = Arc::new(BlockAllocator::new("zc_ledger_stats_test").await.unwrap());
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

    let zc_family = [
        "fuse3_kmbuf_negotiated",
        "fuse3_zc_negotiated",
        "fuse3_zc_replies",
        "fuse3_zc_fallbacks",
        "fuse3_zc_slot_payload_skips",
        "fuse3_zc_write_extractions",
        "fuse3_zc_write_extract_bytes",
        "fuse3_zc_write_directs",
        "fuse3_zc_write_direct_bytes",
        "read_zc_serve_bytes",
    ];

    let metrics = json
        .get("metrics")
        .and_then(|m| m.as_object())
        .expect("stats carries a metrics object");
    for key in zc_family {
        assert!(
            metrics.get(key).is_some_and(|v| v.is_u64()),
            "metrics.{key} must always export as u64 (the zc engagement \
             ledger — an armed bracket row is INVALID without it)"
        );
        assert!(
            json.get(key).is_none(),
            "{key} must NOT appear at the JSON top level — the metrics \
             nesting is the contract tooling keys on"
        );
    }
}
