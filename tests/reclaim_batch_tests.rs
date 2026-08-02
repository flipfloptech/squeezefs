//! Reclaim group-commit contracts (design-wal-crash-consistency §4.5, PR 5,
//! Key Decision 8 — re-pinned against the v3 CoW KV backend when v2 support
//! was removed).
//!
//! FORGET-driven reclaim used to issue one `destroy_inode` transaction per
//! ino — under delete storms, thousands of tiny commits. Batching folds a
//! whole batch into ONE metadata transaction (v3: one journal entry
//! carrying every inode `Delete` plus each corpse's xattr `Delete`s).
//!
//! Contracts:
//! - `destroy_inodes(batch)`: DLM exclusive locks on all inos via the
//!   canonical `lock_many` order; per-ino nlink revalidation under the
//!   locks (P0-8 preserved); ONE transaction (= one journal entry) for
//!   the whole batch, xattr reap riding the SAME entry (§4.8).
//! - No deadlock against foreground create/unlink storms (R5).
//! - The consumer records its group-commit fill
//!   (`meta_reclaim_batch_size`).
//! - BATCH_FORGET behaves exactly like N FORGETs (caches invalidated,
//!   every ino queued for reclaim).
//!
//! (The v2 poisoned-sector bisect case — one bad table sector wedging only
//! its victim ino — was deleted with v2 support: v3 has no per-ino sector
//! to poison, and a journal write failure fail-stops the volume instead,
//! pinned by `crash_kill_tests::test_repeated_journal_failures_escalate_to_disabled_volume`.)

use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
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

fn hist_count(buckets: &[std::sync::atomic::AtomicU64]) -> u64 {
    buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
}

async fn backend_with_orphans(n: u64) -> (NamedTempFile, Arc<KvMetaBackend>, Vec<u64>) {
    let tmp = NamedTempFile::new().unwrap();
    let backend = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;
    let mut inos = Vec::new();
    for i in 0..n {
        let f = backend
            .create(1, &format!("orphan{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }
    for i in 0..n {
        backend.unlink(1, &format!("orphan{i}")).await.unwrap();
    }
    (tmp, backend, inos)
}

/// A 40-ino batch commits as ONE transaction (observable: exactly one
/// journal entry) and removes every inode record.
#[tokio::test]
async fn test_destroy_inodes_batch_single_transaction_removes_records() {
    let (_t, backend, inos) = backend_with_orphans(40).await;

    let entries_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    backend
        .destroy_inodes(&inos)
        .await
        .expect("batch destroy of 40 orphans");
    let entries_after = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    assert_eq!(
        entries_after - entries_before,
        1,
        "the whole batch must commit as ONE transaction (one journal entry)"
    );

    for &ino in &inos {
        assert!(
            backend.getattr(ino).await.is_err(),
            "ino {ino} record must be removed"
        );
    }
    backend.shutdown().await.unwrap();
}

/// P0-8 across the batch: nlink is revalidated per ino UNDER the exclusive
/// locks — live files in the batch survive; missing inos are tolerated;
/// doomed ones are destroyed.
#[tokio::test]
async fn test_destroy_inodes_revalidates_nlink_under_lock() {
    let tmp = NamedTempFile::new().unwrap();
    let backend = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;

    let live = backend
        .create(1, "live", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let doomed = backend
        .create(1, "doomed", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    backend.unlink(1, "doomed").await.unwrap();

    backend
        .destroy_inodes(&[live.ino, doomed.ino, 999_999])
        .await
        .expect("batch with live + doomed + missing must succeed");

    let survivor = backend
        .getattr(live.ino)
        .await
        .expect("live (nlink > 0) ino must survive the batch");
    assert_eq!(survivor.ino, live.ino);
    assert!(
        backend.getattr(doomed.ino).await.is_err(),
        "doomed ino must be destroyed"
    );
    backend.shutdown().await.unwrap();
}

/// R5: batched reclaim holding many I-stripes via `lock_many` must not
/// deadlock against a foreground create/unlink storm on the same stripes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_no_deadlock_reclaim_batch_vs_create_storm() {
    let tmp = NamedTempFile::new().unwrap();
    let backend = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;

    let reclaimer = {
        let backend = backend.clone();
        tokio::spawn(async move {
            for round in 0..30 {
                let mut inos = Vec::new();
                for i in 0..16 {
                    let f = backend
                        .create(1, &format!("r{round}_{i}"), libc::S_IFREG | 0o644, 0, 0)
                        .await
                        .unwrap();
                    inos.push(f.ino);
                }
                for i in 0..16 {
                    backend.unlink(1, &format!("r{round}_{i}")).await.unwrap();
                }
                backend.destroy_inodes(&inos).await.unwrap();
            }
        })
    };
    let stormer = {
        let backend = backend.clone();
        tokio::spawn(async move {
            for i in 0..200 {
                let name = format!("s{i}");
                let f = backend
                    .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                backend.unlink(1, &name).await.unwrap();
                backend.destroy_inode(f.ino).await.unwrap();
            }
        })
    };

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        reclaimer.await.expect("reclaimer task");
        stormer.await.expect("stormer task");
    })
    .await
    .expect("reclaim batching deadlocked against the create storm");
    backend.shutdown().await.unwrap();
}

/// The reclaim consumer records its group-commit fill.
#[tokio::test]
async fn test_metrics_reclaim_batch_size_recorded() {
    let (_t, backend, inos) = backend_with_orphans(8).await;

    let before = hist_count(&METRICS.meta_reclaim_batch_size.buckets);
    backend
        .destroy_inodes(&inos)
        .await
        .expect("batch destroy records its fill");
    let after = hist_count(&METRICS.meta_reclaim_batch_size.buckets);
    assert!(
        after > before,
        "meta_reclaim_batch_size must record the batch fill (delta {})",
        after - before
    );
    backend.shutdown().await.unwrap();
}

/// The batched destroy also reaps each corpse's xattrs (layout entry)
/// INSIDE the same transaction — reclaim must not issue a per-corpse
/// removexattr commit (that doubled meta-commit traffic under delete
/// storms), and no destroyed ino's layout xattr may survive.
#[tokio::test]
async fn test_destroy_inodes_kills_xattrs_in_the_same_transaction() {
    let tmp = NamedTempFile::new().unwrap();
    let backend = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;

    let mut inos = Vec::new();
    for i in 0..8 {
        let f = backend
            .create(1, &format!("x{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        backend
            .setxattr(f.ino, "layout", b"striped-blockmap")
            .await
            .unwrap();
        inos.push(f.ino);
    }
    for i in 0..8 {
        backend.unlink(1, &format!("x{i}")).await.unwrap();
    }

    let entries_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    backend
        .destroy_inodes(&inos)
        .await
        .expect("batch destroy with xattr-carrying corpses");
    let entries_after = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    assert_eq!(
        entries_after - entries_before,
        1,
        "xattr reap must ride the SAME transaction — no per-corpse removexattr commits"
    );

    for &ino in &inos {
        assert_eq!(
            backend
                .getxattr(ino, "layout")
                .await
                .expect("xattr read on destroyed ino"),
            None,
            "destroyed ino {ino} must not keep its layout xattr"
        );
        assert!(
            backend.listxattr(ino).await.unwrap().is_empty(),
            "destroyed ino {ino} must list no xattrs"
        );
    }
    backend.shutdown().await.unwrap();
}

/// BATCH_FORGET must behave exactly like N FORGETs: caches invalidated and
/// every ino queued for reclaim. fuse3's default impl is a NO-OP — before
/// this contract existed, batch-evicted inos (memory pressure,
/// drop_caches, umount-time mass eviction) were simply never reclaimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_batch_forget_queues_reclaim_like_forget() {
    use fuse3::raw::prelude::Filesystem;
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("batch_forget_test").await.unwrap());
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
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    // Three orphans (created + unlinked), attrs cached.
    let mut inos = Vec::new();
    for i in 0..3 {
        let f = routed
            .create(1, &format!("bf{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }
    for i in 0..3 {
        routed.unlink(1, &format!("bf{i}")).await.unwrap();
    }
    let now = std::time::Instant::now();
    for &ino in &inos {
        fs.attr_cache.insert(
            ino,
            (
                fuse3::raw::prelude::FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: fuse3::Timestamp::new(0, 0),
                    mtime: fuse3::Timestamp::new(0, 0),
                    ctime: fuse3::Timestamp::new(0, 0),
                    kind: fuse3::FileType::RegularFile,
                    perm: 0o644,
                    nlink: 0,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    blksize: 4096,
                },
                now,
            ),
        );
    }

    let req = fuse3::raw::Request {
        unique: 0,
        uid: 1000,
        gid: 1000,
        pid: 1,
        ..Default::default()
    };
    fs.batch_forget(req, &inos).await;

    for &ino in &inos {
        assert!(
            fs.attr_cache.get(&ino).is_none(),
            "batch_forget must invalidate attrs for ino {ino}"
        );
    }
    // The reclaim queue received all three: drain it through the batch
    // consumer entry and observe the destroys.
    fs.reclaim_orphaned_batch(inos.clone()).await;
    for &ino in &inos {
        assert!(
            routed.getattr(ino).await.is_err(),
            "batch-forgotten orphan {ino} must be reclaimable (record removed)"
        );
    }
}
