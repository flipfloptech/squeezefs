//! Flag-ON (sector-sharded commit) concurrency + correctness tests.
//!
//! These run with `SQUEEZEFS_META_SECTOR_LOCKS=1` so `run_transaction` and the
//! inode/dentry helpers take the sector-sharded commit path (design PR 4). The
//! flag is read once per process and cached (`meta_sector_locks_enabled`), so
//! this file is a dedicated flag-ON test binary: every test calls `enable()`
//! before touching any storage, and there is no flag-OFF test in this binary.
//!
//! The invariant under test (design R1/§3.8): **flag-on ⇒ no metadata mutator
//! writes a 4 KiB sector outside its sector write lock** — so concurrent
//! mutations of different slots in one physical sector never clobber a sibling
//! (the `352d776` ESTALE class), and allocation never double-hands an inode.

use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata, RoutedMetaBackend};
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Turn on the sector-sharded commit path for this (whole) test process. Must be
/// called before the first `meta_sector_locks_enabled()` read; the assert proves
/// the flag actually took effect (guards against a stale cached read).
fn enable() {
    std::env::set_var("SQUEEZEFS_META_SECTOR_LOCKS", "1");
    assert!(
        squeezefs::meta_backend::storage::meta_sector_locks_enabled(),
        "SQUEEZEFS_META_SECTOR_LOCKS must be enabled for this binary"
    );
}

async fn open_backend() -> (NamedTempFile, Arc<MetaLvBackend>) {
    let tmp = NamedTempFile::new().unwrap();
    let s = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    // `format` runs before any concurrent op (single-threaded), so its direct
    // sector writes are safe; the sector-lock invariant is about the serving path.
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

/// N tasks × M creates across **distinct** parents. Every returned ino must be
/// unique (invariant #2, no double-alloc) and every inode getattr-able with
/// valid magic (invariant #1). Sequential allocation clusters adjacent inos in
/// the same 4 KiB inode sector, so concurrent tasks exercise same-sector RMW.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_create_distinct_inodes_no_collision() {
    enable();
    let (_t, backend) = open_routed(1).await;

    let n_tasks = 8usize;
    let m = 60usize;
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
                    .unwrap_or_else(|e| panic!("create f{j} under {pino}: {e:?}"));
                all.lock().unwrap().push(f.ino);
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let inos = all.lock().unwrap().clone();
    let uniq: HashSet<u64> = inos.iter().copied().collect();
    assert_eq!(inos.len(), uniq.len(), "an inode was allocated twice");
    assert_eq!(inos.len(), n_tasks * m);
    for &ino in &inos {
        let g = backend.getattr(ino).await.unwrap();
        assert_eq!(g.ino, ino);
        assert!(g.mode & libc::S_IFREG != 0, "ino {ino} lost its magic/type");
    }
    // get_allocated_inode_count must reflect the in-RAM allocator (flag-on):
    // n_tasks parent dirs + n_tasks*m files (root ino 1 is not counted).
    let count = backend.volumes[0].get_allocated_inode_count().await;
    assert_eq!(count, n_tasks + n_tasks * m, "allocated count mismatch");
}

/// Force 14 inodes into ONE 4 KiB sector (inos 2..=15 share sector 8192 with the
/// root, 16 slots/sector) and `setattr` them all concurrently with distinct
/// sizes. Every slot must keep valid magic and its own independent size — the
/// `352d776` sibling-slot clobber, at sector granularity, flag-on.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_same_sector_rmw_no_lost_slot() {
    enable();
    let (_t, backend) = open_backend().await;

    let mut inos = Vec::new();
    for i in 0..14 {
        let f = backend
            .create(1, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }
    // Confirm they all live in the same physical sector (else the test is moot).
    let sector_of = |ino: u64| (8192u64 + ino * 256) & !(4096u64 - 1);
    let s0 = sector_of(inos[0]);
    assert!(
        inos.iter().all(|&i| sector_of(i) == s0),
        "test setup: inodes not co-located in one sector: {inos:?}"
    );

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
        assert_eq!(g.size, (idx as u64 + 1) * 1000, "lost setattr on ino {ino}");
        assert!(g.mode & libc::S_IFREG != 0, "ino {ino} magic clobbered");
    }
    let root = backend.getattr(1).await.unwrap();
    assert!(root.mode & libc::S_IFDIR != 0, "root clobbered");
}

/// The fast-path's exact hazard (review Issue 19): concurrently create a regular
/// file **and** setattr/mkdir on already-allocated adjacent inodes in the same
/// 4 KiB sector. Flag-on routes the regular create through the sector-locked
/// commit, so it must not clobber a sibling slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_create_vs_setattr_adjacent_inodes() {
    enable();
    let (_t, backend) = open_routed(1).await;

    // Pre-create 12 files (inos 2..=13, sector 8192). inos 14/15 remain free in
    // the same sector for the concurrent create/mkdir to land in.
    let mut inos = Vec::new();
    for i in 0..12 {
        let f = backend
            .create(1, &format!("pre{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }

    let mut handles = Vec::new();
    for (idx, &ino) in inos.iter().enumerate() {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            let sz = (idx as u64 + 1) * 777;
            b.setattr(ino, None, None, None, Some(sz), None, None, None)
                .await
                .unwrap();
        }));
    }
    {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            b.create(1, "newreg", libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap();
        }));
    }
    {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            b.create(1, "newdir", libc::S_IFDIR | 0o755, 0, 0)
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    for (idx, &ino) in inos.iter().enumerate() {
        let g = backend.getattr(ino).await.unwrap();
        assert_eq!(g.size, (idx as u64 + 1) * 777, "lost setattr on ino {ino}");
        assert!(g.mode & libc::S_IFREG != 0, "ino {ino} magic clobbered");
    }
    let nr = backend.lookup(1, "newreg").await.unwrap();
    assert!(nr.mode & libc::S_IFREG != 0, "newreg wrong type/clobbered");
    let nd = backend.lookup(1, "newdir").await.unwrap();
    assert!(nd.mode & libc::S_IFDIR != 0, "newdir wrong type/clobbered");
}

/// N regular creates in ONE parent (review Issue 1 / R8). The parent's nlink must
/// be **unchanged** (regular files never bump it), every child ino unique, and no
/// child slot clobbered.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_regular_create_same_parent() {
    enable();
    let (_t, backend) = open_routed(1).await;

    let parent = backend
        .create(1, "dir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let pino = parent.ino;
    let nlink_before = backend.getattr(pino).await.unwrap().nlink;

    let n = 8usize;
    let m = 40usize;
    let all = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for t in 0..n {
        let b = backend.clone();
        let all = all.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..m {
                let f = b
                    .create(pino, &format!("f_{t}_{j}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                all.lock().unwrap().push(f.ino);
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let g = backend.getattr(pino).await.unwrap();
    assert_eq!(
        g.nlink, nlink_before,
        "regular-file creates must NOT change parent nlink"
    );

    let inos = all.lock().unwrap().clone();
    let uniq: HashSet<u64> = inos.iter().copied().collect();
    assert_eq!(inos.len(), uniq.len(), "duplicate child ino");
    assert_eq!(inos.len(), n * m);
    for &ino in &inos {
        let gi = backend.getattr(ino).await.unwrap();
        assert!(gi.mode & libc::S_IFREG != 0, "child ino {ino} clobbered");
    }
    let list = backend.readdir(pino, 0, 1_000_000).await.unwrap();
    assert_eq!(list.len(), n * m, "some dentry lost/duplicated");
}

/// PR 5: concurrent same-parent regular creates hold only a SHARED parent lock
/// and update the parent via a 16-byte mtime/ctime field patch. Assert the patch
/// took effect (parent mtime advanced) while every value field the patch must NOT
/// touch (nlink/mode/uid/gid) is preserved — i.e. concurrent field patches under
/// the parent sector lock never clobber the parent's value fields (design §3.8).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_regular_create_advances_parent_mtime_preserves_fields() {
    enable();
    let (_t, backend) = open_routed(1).await;

    let parent = backend
        .create(1, "pd", libc::S_IFDIR | 0o750, 4321, 8765)
        .await
        .unwrap();
    let pino = parent.ino;
    let before = backend.getattr(pino).await.unwrap();

    let n = 8usize;
    let m = 30usize;
    let mut handles = Vec::new();
    for t in 0..n {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..m {
                b.create(pino, &format!("g_{t}_{j}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let after = backend.getattr(pino).await.unwrap();
    assert!(
        after.mtime > before.mtime,
        "parent mtime must advance via the field patch ({} !> {})",
        after.mtime,
        before.mtime
    );
    assert_eq!(
        after.nlink, before.nlink,
        "field patch must not touch parent nlink"
    );
    assert_eq!(
        after.mode, before.mode,
        "field patch must not touch parent mode"
    );
    assert_eq!(
        after.uid, before.uid,
        "field patch must not touch parent uid"
    );
    assert_eq!(
        after.gid, before.gid,
        "field patch must not touch parent gid"
    );
    // All children present and correct.
    let list = backend.readdir(pino, 0, 1_000_000).await.unwrap();
    assert_eq!(list.len(), n * m, "some dentry lost/duplicated");
}

/// Many inserts/removes colliding on ONE dentry bucket (force overflow-chain
/// growth). After the churn every live name is found exactly once, no name is
/// duplicated, and no overflow slot is leaked (the occupied set returns to
/// baseline). Exercises the per-bucket lock + in-RAM chain integrity (design
/// §3.6, review Issue 3/16).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_same_bucket_dentry_ops() {
    enable();
    let (_t, backend) = open_routed(1).await;

    let parent = backend
        .create(1, "bkt", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let pino = parent.ino;
    let baseline = backend.volumes[0]
        .storage
        .dentry_occupied_offsets
        .lock()
        .unwrap()
        .len();

    // Collect 24 names that all hash to ONE bucket (so a single overflow chain
    // of length 24 is built, exercising insert/remove chain surgery).
    let target = squeezefs::meta_backend::dentry::dentry_bucket(pino, "seed");
    let mut names = Vec::new();
    let mut i = 0u64;
    while names.len() < 24 && i < 5_000_000 {
        let n = format!("x{i}");
        if squeezefs::meta_backend::dentry::dentry_bucket(pino, &n) == target {
            names.push(n);
        }
        i += 1;
    }
    assert_eq!(names.len(), 24, "could not find 24 colliding names");
    let names = Arc::new(names);

    // Concurrent create of all colliding names.
    let mut handles = Vec::new();
    for t in 0..8usize {
        let b = backend.clone();
        let names = names.clone();
        handles.push(tokio::spawn(async move {
            let mut k = t;
            while k < names.len() {
                b.create(pino, &names[k], libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                k += 8;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Every name present, unique ino.
    let mut seen = HashSet::new();
    for n in names.iter() {
        let f = backend
            .lookup(pino, n)
            .await
            .unwrap_or_else(|e| panic!("lookup {n} after create: {e:?}"));
        assert!(seen.insert(f.ino), "duplicate ino for {n}");
    }
    assert_eq!(
        backend.readdir(pino, 0, 1_000_000).await.unwrap().len(),
        24,
        "chain lost/duplicated a dentry"
    );

    // Concurrent unlink of all names.
    let mut handles = Vec::new();
    for t in 0..8usize {
        let b = backend.clone();
        let names = names.clone();
        handles.push(tokio::spawn(async move {
            let mut k = t;
            while k < names.len() {
                b.unlink(pino, &names[k]).await.unwrap();
                k += 8;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    for n in names.iter() {
        assert!(backend.lookup(pino, n).await.is_err(), "still present: {n}");
    }
    assert_eq!(
        backend.readdir(pino, 0, 1_000_000).await.unwrap().len(),
        0,
        "dentries remain after unlink-all"
    );
    let after = backend.volumes[0]
        .storage
        .dentry_occupied_offsets
        .lock()
        .unwrap()
        .len();
    assert_eq!(after, baseline, "leaked overflow slot(s) after churn");
}

/// Transactions each touching 2+ sectors (cross-directory renames in opposing
/// natural order + concurrent creates). Must complete under a timeout — a
/// lock-order inversion would deadlock and blow the deadline (design R2).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_no_deadlock_multi_sector_tx() {
    enable();
    let (_t, backend) = open_routed(1).await;

    let a = backend
        .create(1, "A", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let b = backend
        .create(1, "B", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..20 {
        backend
            .create(a, &format!("a{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        backend
            .create(b, &format!("b{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }

    let fut = async {
        let mut handles = Vec::new();
        for i in 0..20 {
            let bk = backend.clone();
            handles.push(tokio::spawn(async move {
                bk.rename(a, &format!("a{i}"), b, &format!("ma{i}"), 0)
                    .await
                    .unwrap();
            }));
            let bk = backend.clone();
            handles.push(tokio::spawn(async move {
                bk.rename(b, &format!("b{i}"), a, &format!("mb{i}"), 0)
                    .await
                    .unwrap();
            }));
            let bk = backend.clone();
            handles.push(tokio::spawn(async move {
                bk.create(1, &format!("c{i}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), fut)
        .await
        .expect("multi-sector transactions deadlocked (timed out)");

    // Sanity: the renamed entries all landed.
    for i in 0..20 {
        assert!(backend.lookup(b, &format!("ma{i}")).await.is_ok());
        assert!(backend.lookup(a, &format!("mb{i}")).await.is_ok());
    }
}

/// Interleave `destroy_inode(X)` with a `create` that may re-allocate X. The
/// recreated inode must always survive with valid magic — proving `free(X)` is
/// ordered strictly after the destroy's durable slot-zero, never in-closure
/// (design Key Decision 11 / R10).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_destroy_realloc_no_clobber() {
    enable();
    let (_t, backend) = open_backend().await;

    for iter in 0..120 {
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

        let g = backend
            .getattr(newf.ino)
            .await
            .unwrap_or_else(|e| panic!("recreated ino {} gone at iter {iter}: {e:?}", newf.ino));
        assert!(
            g.mode & libc::S_IFREG != 0,
            "recreated inode {} clobbered at iter {iter}",
            newf.ino
        );

        backend.unlink(1, "reused").await.unwrap();
        backend.destroy_inode(newf.ino).await.unwrap();
    }
}

/// The production dual-volume shape under concurrent mkdir, flag-ON. Mirrors the
/// flag-OFF `test_routed_concurrent_mkdir_no_lost_inodes` (which stays in
/// `meta_lv_tests.rs`); this proves the sector-locked commit keeps the same
/// no-lost-inode / parent-nlink guarantees.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_routed_concurrent_mkdir_no_lost_inodes_flag_on() {
    enable();
    let (_t, backend) = open_routed(2).await;

    let threads = 10usize;
    let count = 60usize;

    let mut parents = Vec::new();
    for i in 0..threads {
        let parent = backend
            .create(1, &format!("bench_dir_{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("create parent");
        let g = backend.getattr(parent.ino).await.expect("getattr parent");
        assert_eq!(g.ino, parent.ino);
        assert!(g.mode & libc::S_IFDIR != 0);
        assert!(g.nlink >= 2);
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
                    .unwrap_or_else(|e| panic!("create dir_{i} under {parent_ino}: {e:?}"));
                let g = backend
                    .getattr(child.ino)
                    .await
                    .unwrap_or_else(|e| panic!("getattr child {} : {e:?}", child.ino));
                assert_eq!(g.ino, child.ino);
                assert!(g.mode & libc::S_IFDIR != 0);
                assert!(g.nlink >= 2);
            }
            let parent_g = backend.getattr(parent_ino).await.expect("parent after");
            assert!(parent_g.mode & libc::S_IFDIR != 0);
            assert!(
                parent_g.nlink >= 2 + count as u32,
                "parent nlink {} expected >= {}",
                parent_g.nlink,
                2 + count
            );
            for i in 0..count {
                let found = backend
                    .lookup(parent_ino, &format!("dir_{i}"))
                    .await
                    .unwrap_or_else(|e| panic!("lookup dir_{i}: {e:?}"));
                let g = backend.getattr(found.ino).await.expect("getattr found");
                assert_eq!(g.ino, found.ino);
            }
        }));
    }
    for h in handles {
        h.await.expect("join task");
    }
}
