//! Reclaim group-commit contracts (design-wal-crash-consistency §4.5, PR 5,
//! Key Decision 8).
//!
//! FORGET-driven reclaim used to issue one `destroy_inode` transaction per
//! ino — under delete storms, thousands of tiny commits zeroing one 256 B
//! slot each. With 16 slots per 4 KiB sector, batching is nearly free:
//! sequential allocation clusters doomed inos in the same sectors, so a
//! batch's zeroes merge into shared sector images and ONE apply write.
//!
//! Contracts:
//! - `destroy_inodes(batch)`: DLM exclusive locks on all inos via the
//!   canonical `lock_many` order; per-ino nlink revalidation under the
//!   locks (P0-8 preserved); ONE transaction for the whole batch;
//!   `free()` strictly after the durable commit (Key Decision 11).
//! - Consumer bisects on batch failure down to size-1 (byte-for-byte
//!   today's per-ino path); a failed singleton still receives the per-ino
//!   lease/POSIX-lock/cache teardown — only `free()` is withheld.
//! - No deadlock against foreground create/unlink storms (R5).

use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::inode::read_inode;
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata};
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;

fn hist_count(buckets: &[std::sync::atomic::AtomicU64]) -> u64 {
    buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
}

async fn backend_with_orphans(n: u64) -> (NamedTempFile, MetaLvBackend, Vec<u64>) {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let backend = MetaLvBackend::new(storage);
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
/// `meta_commit_sectors` record), zeroes every slot, and frees every
/// allocator bit only after the commit.
#[tokio::test]
async fn test_destroy_inodes_batch_single_transaction_zeroes_and_frees() {
    let (_t, backend, inos) = backend_with_orphans(40).await;

    let commits_before = hist_count(&METRICS.meta_commit_sectors.buckets);
    backend
        .destroy_inodes(&inos)
        .await
        .expect("batch destroy of 40 orphans");
    let commits_after = hist_count(&METRICS.meta_commit_sectors.buckets);
    assert_eq!(
        commits_after - commits_before,
        1,
        "the whole batch must commit as ONE transaction (same-sector zeroes merged)"
    );

    for &ino in &inos {
        assert!(
            read_inode(&backend.storage, ino).await.is_err(),
            "ino {ino} slot must be zeroed"
        );
        assert!(
            !backend.storage.inode_alloc.is_set(ino),
            "ino {ino} must be freed after the durable commit"
        );
    }

    // Freed slots are reusable.
    let reused = backend
        .create(1, "reuse", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert!(
        inos.contains(&reused.ino),
        "a freed ino must be reusable (got {})",
        reused.ino
    );
}

/// P0-8 across the batch: nlink is revalidated per ino UNDER the exclusive
/// locks — live files in the batch survive; missing/zeroed inos are
/// tolerated; doomed ones are destroyed.
#[tokio::test]
async fn test_destroy_inodes_revalidates_nlink_under_lock() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let backend = MetaLvBackend::new(storage);

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
        .destroy_inodes(&[live.ino, doomed.ino, 9999])
        .await
        .expect("batch with live + doomed + missing must succeed");

    let survivor = read_inode(&backend.storage, live.ino)
        .await
        .expect("live (nlink > 0) ino must survive the batch");
    assert_eq!(survivor.ino, live.ino);
    assert!(backend.storage.inode_alloc.is_set(live.ino));
    assert!(
        read_inode(&backend.storage, doomed.ino).await.is_err(),
        "doomed ino must be destroyed"
    );
    assert!(!backend.storage.inode_alloc.is_set(doomed.ino));
}

/// Batch failure semantics (§4.5): one persistently failing sector must not
/// wedge the innocent inos — the consumer bisects down to singletons, the
/// other inos commit, and the failed singleton still receives the per-ino
/// cache teardown while its `free()` is withheld.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reclaim_batch_bisect_poisoned_sector_wedges_only_victim() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;

    // Full fs harness (reclaim is a fuse_client concern).
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "reclaim_batch_test")
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
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(m.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&ms, true, true, None)
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(ms)),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    // Doomed inos 2..=15 share the first table sector (8192); the victim
    // (ino 18) sits in sector 12288 with only LIVE siblings, so the armed
    // sector error wedges exactly one admitted ino.
    let backend = &routed.volumes[0];
    let mut doomed = Vec::new();
    for i in 0..14 {
        let f = backend
            .create(1, &format!("d{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        doomed.push(f.ino);
    }
    for i in 0..6 {
        backend
            .create(1, &format!("live{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let victim = backend
        .create(1, "victim", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    assert!(
        victim >= 16,
        "victim must live outside the first table sector"
    );
    for i in 0..14 {
        backend.unlink(1, &format!("d{i}")).await.unwrap();
    }
    backend.unlink(1, "victim").await.unwrap();

    // Pre-seed caches so the teardown edge is observable for BOTH outcomes.
    let now = std::time::Instant::now();
    let dummy_attr = |ino: u64| fuse3::raw::prelude::FileAttr {
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
    };
    fs.attr_cache.insert(victim, (dummy_attr(victim), now));
    fs.attr_cache
        .insert(doomed[0], (dummy_attr(doomed[0]), now));

    // The victim's slot sector persistently errors (no poison, no tear).
    let victim_sector = 8192 + (victim / 16) * 4096;
    squeezefs::uring_fs::arm_sector_write_error(victim_sector);

    let mut batch = doomed.clone();
    batch.push(victim);
    fs.reclaim_orphaned_batch(batch).await;
    squeezefs::uring_fs::clear_faults();

    // The 14 innocents committed: slots zeroed, bits freed.
    for &ino in &doomed {
        assert!(
            read_inode(&backend.storage, ino).await.is_err(),
            "innocent ino {ino} must commit despite the victim's bad sector"
        );
        assert!(!backend.storage.inode_alloc.is_set(ino));
    }
    // The victim did not: slot intact, free() withheld.
    let v = read_inode(&backend.storage, victim)
        .await
        .expect("victim slot must be untouched after its write failed");
    assert_eq!(v.ino, victim);
    assert!(
        backend.storage.inode_alloc.is_set(victim),
        "free() must be withheld for the failed singleton"
    );
    // Teardown ran on BOTH edges: caches invalidated for victim and innocent.
    assert!(
        fs.attr_cache.get(&victim).is_none(),
        "failed singleton must still receive cache teardown"
    );
    assert!(
        fs.attr_cache.get(&doomed[0]).is_none(),
        "committed ino must receive cache teardown"
    );
}

/// R5: batched reclaim holding many I-stripes via `lock_many` must not
/// deadlock against a foreground create/unlink storm on the same stripes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_no_deadlock_reclaim_batch_vs_create_storm() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let backend = std::sync::Arc::new(MetaLvBackend::new(storage));

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
}

/// The batched destroy also kills each corpse's xattr block (layout entry)
/// INSIDE the same transaction — reclaim must not issue a per-corpse
/// removexattr commit (that doubled meta-commit traffic under delete
/// storms and its sector guards collided with foreground unlinks), and an
/// ino reused after reclaim must not resurrect the corpse's layout xattr.
#[tokio::test]
async fn test_destroy_inodes_kills_xattrs_in_the_same_transaction() {
    use squeezefs::meta_backend::xattr;

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let backend = MetaLvBackend::new(storage);

    let mut inos = Vec::new();
    for i in 0..8 {
        let f = backend
            .create(1, &format!("x{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        xattr::set_xattr(&backend.storage, f.ino, "layout", b"striped-blockmap")
            .await
            .unwrap();
        inos.push(f.ino);
    }
    for i in 0..8 {
        backend.unlink(1, &format!("x{i}")).await.unwrap();
    }

    let commits_before = hist_count(&METRICS.meta_commit_sectors.buckets);
    backend
        .destroy_inodes(&inos)
        .await
        .expect("batch destroy with xattr-carrying corpses");
    let commits_after = hist_count(&METRICS.meta_commit_sectors.buckets);
    assert_eq!(
        commits_after - commits_before,
        1,
        "xattr kill must ride the SAME transaction — no per-corpse removexattr commits"
    );

    // Reuse the inos: fresh files must see no layout ghost.
    for (i, &old_ino) in inos.iter().enumerate() {
        let f = backend
            .create(1, &format!("fresh{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        assert_eq!(f.ino, old_ino, "freed ino must be reused for this probe");
        assert_eq!(
            xattr::get_xattr(&backend.storage, f.ino, "layout")
                .await
                .expect("xattr read on reused ino"),
            None,
            "reused ino {} must not resurrect the corpse's layout xattr",
            f.ino
        );
    }
}

/// BATCH_FORGET must behave exactly like N FORGETs: caches invalidated and
/// every ino queued for reclaim. fuse3's default impl is a NO-OP — before
/// this contract existed, batch-evicted inos (memory pressure,
/// drop_caches, umount-time mass eviction) were simply never reclaimed:
/// their slots leaked until the next mount's reconciliation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_batch_forget_queues_reclaim_like_forget() {
    use fuse3::raw::prelude::Filesystem;
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;

    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "batch_forget_test")
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
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(m.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&ms, true, true, None)
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(ms)),
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
            read_inode(&routed.volumes[0].storage, ino).await.is_err(),
            "batch-forgotten orphan {ino} must be reclaimable (slot zeroed)"
        );
    }
}
