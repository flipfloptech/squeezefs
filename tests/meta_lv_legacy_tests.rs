//! Flag-OFF (legacy `transaction_lock`) rollback-path tests.
//!
//! PR 4 flips the `SQUEEZEFS_META_SECTOR_LOCKS` default to **on**, but the whole
//! legacy path (`transaction_lock` + global `inode_lock`/`dentry_lock` +
//! `alloc_inode_bit_locked` + the regular-file fast path) is retained as the
//! rollback lever until PR 8. This binary sets the flag **off** before any
//! storage use and re-runs the critical concurrency/CRUD tests to prove the
//! legacy writer model still behaves correctly (the two models never coexist in
//! one process — the flag is read once and cached).

use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata, RoutedMetaBackend};
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Force the legacy path off for this whole test process.
fn disable() {
    std::env::set_var("SQUEEZEFS_META_SECTOR_LOCKS", "0");
    assert!(
        !squeezefs::meta_backend::storage::meta_sector_locks_enabled(),
        "SQUEEZEFS_META_SECTOR_LOCKS must be disabled for this binary"
    );
}

async fn open_backend() -> (NamedTempFile, Arc<MetaLvBackend>) {
    let tmp = NamedTempFile::new().unwrap();
    let s = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&s).await.unwrap();
    (tmp, Arc::new(MetaLvBackend::new(s)))
}

async fn open_routed(n: usize) -> (Vec<NamedTempFile>, Arc<RoutedMetaBackend>) {
    let mut tmps = Vec::new();
    let mut vols = Vec::new();
    for _ in 0..n {
        let tmp = NamedTempFile::new().unwrap();
        let s = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
        MetaLvBackend::format(&s).await.unwrap();
        vols.push(Arc::new(MetaLvBackend::new(s)));
        tmps.push(tmp);
    }
    (tmps, Arc::new(RoutedMetaBackend::new(vols)))
}

#[test]
fn test_flag_off_when_explicitly_disabled() {
    disable();
    assert!(!squeezefs::meta_backend::storage::meta_sector_locks_enabled());
}

/// Legacy path: distinct-parent concurrent creates never double-allocate.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_legacy_concurrent_create_distinct_inodes() {
    disable();
    let (_t, backend) = open_routed(1).await;

    let n_tasks = 8usize;
    let m = 50usize;
    let mut parents = Vec::new();
    for i in 0..n_tasks {
        let p = backend
            .create(1, &format!("p{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
        parents.push(p.ino);
    }
    let all = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for &pino in &parents {
        let b = backend.clone();
        let all = all.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..m {
                let f = b
                    .create(pino, &format!("f{j}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                all.lock().unwrap().push(f.ino);
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let inos = all.lock().unwrap().clone();
    let uniq: HashSet<u64> = inos.iter().copied().collect();
    assert_eq!(inos.len(), uniq.len(), "legacy path double-allocated");
    assert_eq!(inos.len(), n_tasks * m);
    for &ino in &inos {
        let g = backend.getattr(ino).await.unwrap();
        assert!(g.mode & libc::S_IFREG != 0, "ino {ino} clobbered (legacy)");
    }
}

/// Legacy path: concurrent same-sector setattr keeps every slot independent.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_legacy_concurrent_same_sector_rmw() {
    disable();
    let (_t, backend) = open_backend().await;

    let mut inos = Vec::new();
    for i in 0..14 {
        let f = backend
            .create(1, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }
    let mut handles = Vec::new();
    for (idx, &ino) in inos.iter().enumerate() {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            let sz = (idx as u64 + 1) * 1000;
            b.setattr(ino, None, None, None, Some(sz), None, None, None)
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    for (idx, &ino) in inos.iter().enumerate() {
        let g = backend.getattr(ino).await.unwrap();
        assert_eq!(g.size, (idx as u64 + 1) * 1000, "legacy lost setattr {ino}");
        assert!(g.mode & libc::S_IFREG != 0);
    }
}

/// Legacy path: destroy + reallocation must not clobber the recreated inode.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_legacy_destroy_realloc_no_clobber() {
    disable();
    let (_t, backend) = open_backend().await;

    for iter in 0..80 {
        let f = backend
            .create(1, "victim", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        let ino = f.ino;
        backend.unlink(1, "victim").await.unwrap();
        let b1 = backend.clone();
        let b2 = backend.clone();
        let h1 = tokio::spawn(async move { b1.destroy_inode(ino).await.unwrap() });
        let h2 = tokio::spawn(async move {
            b2.create(1, "reused", libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
        });
        h1.await.unwrap();
        let newf = h2.await.unwrap();
        let g = backend.getattr(newf.ino).await.unwrap();
        assert!(
            g.mode & libc::S_IFREG != 0,
            "legacy recreated inode {} clobbered at iter {iter}",
            newf.ino
        );
        backend.unlink(1, "reused").await.unwrap();
        backend.destroy_inode(newf.ino).await.unwrap();
    }
}

/// Legacy dual-volume concurrent mkdir (the original `352d776` regression shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_legacy_routed_concurrent_mkdir_no_lost_inodes() {
    disable();
    let (_t, backend) = open_routed(2).await;

    let threads = 10usize;
    let count = 60usize;
    let mut parents = Vec::new();
    for i in 0..threads {
        let parent = backend
            .create(1, &format!("bench_dir_{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("create parent");
        parents.push(parent);
    }
    let mut handles = Vec::new();
    for parent in &parents {
        let backend = backend.clone();
        let parent_ino = parent.ino;
        handles.push(tokio::spawn(async move {
            for i in 0..count {
                let child = backend
                    .create(parent_ino, &format!("dir_{i}"), libc::S_IFDIR | 0o755, 0, 0)
                    .await
                    .unwrap();
                let g = backend.getattr(child.ino).await.unwrap();
                assert!(g.mode & libc::S_IFDIR != 0);
                assert!(g.nlink >= 2);
            }
            let parent_g = backend.getattr(parent_ino).await.unwrap();
            assert!(parent_g.nlink >= 2 + count as u32);
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
}
