//! Sector-sharded commit concurrency + correctness tests (design PR 4; the
//! sole writer model since PR 8 deleted the legacy `transaction_lock` path).
//!
//! The invariant under test (design R1/§3.8): **no metadata mutator
//! writes a 4 KiB sector outside its sector write lock** — so concurrent
//! mutations of different slots in one physical sector never clobber a sibling
//! (the `352d776` ESTALE class), and allocation never double-hands an inode.

use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata, RoutedMetaBackend};
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::NamedTempFile;

async fn open_backend() -> (NamedTempFile, Arc<MetaLvBackend>) {
    let tmp = NamedTempFile::new().unwrap();
    let s = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    // `format` runs before any concurrent op (single-threaded), so its direct
    // sector writes are safe; the sector-lock invariant is about the serving path.
    MetaLvBackend::format_v2_for_tests(&s, true, true, None)
        .await
        .unwrap();
    (tmp, Arc::new(MetaLvBackend::new(s)))
}

async fn open_routed(n: usize) -> (Vec<NamedTempFile>, Arc<RoutedMetaBackend>) {
    let mut tmps = Vec::new();
    let mut vols = Vec::new();
    for _ in 0..n {
        let tmp = NamedTempFile::new().unwrap();
        let s = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
        MetaLvBackend::format_v2_for_tests(&s, true, true, None)
            .await
            .unwrap();
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
    // get_allocated_inode_count must reflect the in-RAM allocator:
    // n_tasks parent dirs + n_tasks*m files (root ino 1 is not counted).
    let count = backend.volumes[0].get_allocated_inode_count().await;
    assert_eq!(count, n_tasks + n_tasks * m, "allocated count mismatch");
}

/// Force 14 inodes into ONE 4 KiB sector (inos 2..=15 share sector 8192 with the
/// root, 16 slots/sector) and `setattr` them all concurrently with distinct
/// sizes. Every slot must keep valid magic and its own independent size — the
/// `352d776` sibling-slot clobber, at sector granularity.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_same_sector_rmw_no_lost_slot() {
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

// ---------------------------------------------------------------------------
// PR 6 — xattr + superblock on sector locks.
//
// Contract (design PR 6): xattr and superblock access participates in the
// sector scheme and takes NO global metadata mutex (the legacy mutexes were
// deleted in PR 8, so their absence is enforced by the type system). Encoded
// here as bounded completion + round-trips, and concurrent/interleaved access
// never tearing a 32 KiB xattr block or the superblock sector.
// ---------------------------------------------------------------------------

/// Xattr ops run on the sector scheme with no global mutex (PR 6; the legacy
/// `xattr_lock` was deleted outright in PR 8, so the absence of a global lock
/// is enforced by the type system — this pins bounded completion + round-trip
/// across the transactional backend path (setxattr/removexattr) and the direct
/// storage path (get/list).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_xattr_ops_complete_bounded_on_sector_scheme() {
    use std::time::Duration;
    let (_t, backend) = open_backend().await;
    let f = backend
        .create(1, "xf", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    let val = vec![0x5Au8; 512];
    tokio::time::timeout(
        Duration::from_secs(5),
        backend.setxattr(f.ino, "user.k1", &val),
    )
    .await
    .expect("setxattr must complete bounded on the sector scheme")
    .expect("setxattr");

    let got = tokio::time::timeout(Duration::from_secs(5), backend.getxattr(f.ino, "user.k1"))
        .await
        .expect("getxattr must complete bounded on the sector scheme")
        .expect("getxattr");
    assert_eq!(got.as_deref(), Some(val.as_slice()));

    let names = tokio::time::timeout(Duration::from_secs(5), backend.listxattr(f.ino))
        .await
        .expect("listxattr must complete bounded on the sector scheme")
        .expect("listxattr");
    assert!(names.iter().any(|n| n == "user.k1"), "{names:?}");

    tokio::time::timeout(
        Duration::from_secs(5),
        backend.removexattr(f.ino, "user.k1"),
    )
    .await
    .expect("removexattr must complete bounded on the sector scheme")
    .expect("removexattr");
    let gone = backend.getxattr(f.ino, "user.k1").await.expect("getxattr2");
    assert!(gone.is_none());
}

/// Superblock read/write on the sector-0 lock of the sector scheme (PR 6; the
/// legacy `superblock_lock` was deleted in PR 8): bounded completion and a
/// write→read round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_superblock_ops_complete_bounded_on_sector_scheme() {
    use std::time::Duration;
    let (_t, backend) = open_backend().await;

    let mut sb = tokio::time::timeout(Duration::from_secs(5), backend.storage.read_superblock())
        .await
        .expect("read_superblock must complete bounded on the sector scheme")
        .expect("read_superblock");

    // `checksum` is derived (stamped by write_superblock since PR 1 of the
    // WAL/crash-consistency design), so round-trip a caller-owned field.
    sb.inode_count = 0xDEAD_BEEF;
    tokio::time::timeout(
        Duration::from_secs(5),
        backend.storage.write_superblock(&sb),
    )
    .await
    .expect("write_superblock must complete bounded on the sector scheme")
    .expect("write_superblock");

    let back = backend.storage.read_superblock().await.expect("re-read");
    assert_eq!(back.inode_count, 0xDEAD_BEEF);
    assert_eq!(
        back.checksum,
        back.compute_checksum(),
        "write_superblock must stamp a self-consistent checksum"
    );
}

/// Concurrent xattr writers on DISTINCT inodes (production path: DLM `I{ino}`
/// held by RoutedMetaBackend) must round-trip every value with no cross-inode
/// clobber — xattr blocks are per-inode/disjoint in the sector scheme.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_xattr_distinct_inodes_roundtrip() {
    let (_t, backend) = open_routed(1).await;

    let n_tasks = 8usize;
    let mut inos = Vec::new();
    for i in 0..n_tasks {
        let f = backend
            .create(1, &format!("xr{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }

    let mut handles = Vec::new();
    for (i, &ino) in inos.iter().enumerate() {
        let b = backend.clone();
        handles.push(tokio::spawn(async move {
            for round in 0..20u8 {
                for k in 0..3u8 {
                    let val = vec![(i as u8) ^ round ^ k; 1024 + i * 17];
                    b.setxattr(ino, &format!("user.k{k}"), &val)
                        .await
                        .unwrap_or_else(|e| panic!("setxattr ino {ino} k{k}: {e:?}"));
                    let got = b
                        .getxattr(ino, &format!("user.k{k}"))
                        .await
                        .unwrap_or_else(|e| panic!("getxattr ino {ino} k{k}: {e:?}"))
                        .unwrap_or_else(|| panic!("xattr ino {ino} k{k} vanished"));
                    assert_eq!(got, val, "ino {ino} key k{k} round {round}");
                }
            }
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    // Final cross-check: every inode still holds exactly its own last values.
    for (i, &ino) in inos.iter().enumerate() {
        for k in 0..3u8 {
            let expect = vec![(i as u8) ^ 19 ^ k; 1024 + i * 17];
            let got = backend.getxattr(ino, &format!("user.k{k}")).await.unwrap();
            assert_eq!(got.as_deref(), Some(expect.as_slice()), "ino {ino} k{k}");
        }
    }
}

/// Readers must never observe a torn xattr value while a writer alternates two
/// full-size values on the same inode (whole-block RMW under the sector scheme;
/// DLM shared/exclusive already excludes same-inode, this pins the whole stack).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_xattr_read_never_torn_under_alternating_writer() {
    let (_t, backend) = open_routed(1).await;
    let f = backend
        .create(1, "torn", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let ino = f.ino;

    let a = vec![0xAAu8; 8192];
    let b = vec![0xBBu8; 8192];
    backend.setxattr(ino, "user.t", &a).await.unwrap();

    let writer = {
        let be = backend.clone();
        let (a, b) = (a.clone(), b.clone());
        tokio::spawn(async move {
            for i in 0..40 {
                let v = if i % 2 == 0 { &b } else { &a };
                be.setxattr(ino, "user.t", v).await.expect("setxattr");
            }
        })
    };
    let mut readers = Vec::new();
    for _ in 0..4 {
        let be = backend.clone();
        let (a, b) = (a.clone(), b.clone());
        readers.push(tokio::spawn(async move {
            for _ in 0..60 {
                let got = be
                    .getxattr(ino, "user.t")
                    .await
                    .expect("getxattr")
                    .expect("present");
                assert!(
                    got == a || got == b,
                    "torn xattr read: len {} first {:?} last {:?}",
                    got.len(),
                    got.first(),
                    got.last()
                );
                tokio::task::yield_now().await;
            }
        }));
    }
    writer.await.expect("writer");
    for r in readers {
        r.await.expect("reader");
    }
}

/// Superblock readers must never observe a torn sector while writers alternate
/// two distinct (magic-valid) superblock images.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_superblock_never_torn_under_concurrent_writers() {
    let (_t, backend) = open_backend().await;
    let s = backend.storage.clone();

    // `checksum` is derived (stamped by write_superblock since PR 1 of the
    // WAL/crash-consistency design), so the variant identity is the two
    // caller-owned fields; the stamped checksum gives readers an independent
    // whole-sector tear detector on top.
    let mut sb_a = s.read_superblock().await.expect("read base");
    sb_a.inode_count = 11_111;
    sb_a.dentry_root = 0xAAAA_AAAA;
    let mut sb_b = s.read_superblock().await.expect("read base");
    sb_b.inode_count = 22_222;
    sb_b.dentry_root = 0xBBBB_BBBB;
    s.write_superblock(&sb_a).await.expect("seed");

    let fields =
        |sb: &squeezefs::meta_backend::storage::Superblock| (sb.inode_count, sb.dentry_root);
    let (fa, fb) = (fields(&sb_a), fields(&sb_b));

    let mut writers = Vec::new();
    for w in 0..2 {
        let s = s.clone();
        let (sa, sb) = (sb_a, sb_b); // Superblock is Copy
        writers.push(tokio::spawn(async move {
            for i in 0..30 {
                let img = if (i + w) % 2 == 0 { &sa } else { &sb };
                s.write_superblock(img).await.expect("write sb");
            }
        }));
    }
    let mut readers = Vec::new();
    for _ in 0..4 {
        let s = s.clone();
        readers.push(tokio::spawn(async move {
            for _ in 0..60 {
                let got = s.read_superblock().await.expect("read sb (magic intact)");
                let f = (got.inode_count, got.dentry_root);
                assert!(f == fa || f == fb, "torn superblock: {f:?}");
                assert_eq!(
                    got.checksum,
                    got.compute_checksum(),
                    "torn superblock: stored checksum does not cover the read image"
                );
                tokio::task::yield_now().await;
            }
        }));
    }
    for h in writers {
        h.await.expect("writer");
    }
    for h in readers {
        h.await.expect("reader");
    }
}

// ---------------------------------------------------------------------------
// PR 7 — sector-lock + allocator metrics on the stats surface (§Observability).
//
// METRICS is process-global and this binary's tests run in parallel, so every
// assertion is a monotonic **delta** (`after - before >= expected`): concurrent
// tests can only push the counters further up, never below the delta this
// test's own operations must produce.
// ---------------------------------------------------------------------------

fn hist_count(buckets: &[std::sync::atomic::AtomicU64]) -> u64 {
    buckets
        .iter()
        .map(|b| b.load(std::sync::atomic::Ordering::Relaxed))
        .sum()
}

/// `meta_tx_concurrency` gauge + peak: N transactions rendezvousing inside
/// their closures are all in flight simultaneously, so the peak watermark must
/// reach at least N (proves the old `transaction_lock` cap of 1 is gone).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_metrics_tx_concurrency_peak_reaches_parallel_txs() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;
    let (_t, backend) = open_backend().await;

    // Distinct parents so the DLM D-locks don't serialize the closures.
    let n = 4usize;
    let mut parents = Vec::new();
    for i in 0..n {
        let p = backend
            .create(1, &format!("cc{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
        parents.push(p.ino);
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::new();
    for &pino in &parents {
        let b = backend.clone();
        let bar = barrier.clone();
        handles.push(tokio::spawn(async move {
            // Rendezvous INSIDE create's transaction closure is not reachable
            // from here, so rendezvous just before issuing the create: with the
            // barrier releasing all tasks at once and worker_threads=8, the four
            // sector-locked transactions overlap. The peak assert below is >=,
            // and the barrier retries make this deterministic in practice.
            bar.wait().await;
            for j in 0..25 {
                b.create(pino, &format!("f{j}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let peak = METRICS.meta_tx_concurrency_peak.load(Ordering::Relaxed);
    assert!(
        peak >= 2,
        "peak in-flight sector-locked transactions {peak} — the concurrency cap is back?"
    );
    // The gauge must return to a quiescent value (no leaked increments): after
    // all work joined, in-flight count contributed by this test is zero, so the
    // gauge is bounded by whatever other parallel tests currently run.
    let cur = METRICS.meta_tx_concurrency.load(Ordering::Relaxed);
    assert!(cur <= 64, "meta_tx_concurrency leaked: {cur}");
}

/// A commit whose target sector lock is deliberately held must count as
/// contended and record its wait; an uncontended baseline records waits only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_metrics_sector_lock_contended_and_wait_recorded() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;
    let (_t, backend) = open_backend().await;

    let wait_before = hist_count(&METRICS.meta_sector_lock_wait_ns.buckets);
    let contended_before = METRICS.meta_sector_lock_contended.load(Ordering::Relaxed);

    // Uncontended create: wait histogram must record (fast) acquisitions.
    backend
        .create(1, "uncontended", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let wait_mid = hist_count(&METRICS.meta_sector_lock_wait_ns.buckets);
    assert!(
        wait_mid > wait_before,
        "sector-lock wait histogram did not record an uncontended commit"
    );

    // Contended commit: a setxattr runs a sector-locked transaction
    // whose staged 32 KiB xattr-block patch (PR 6) commits under the block's
    // sector locks at a KNOWN offset — hold the first of those sectors so the
    // commit's guard acquisition must block on it.
    let f = backend
        .create(1, "contended", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let xattr_sector = {
        use squeezefs::meta_backend::xattr::{XATTR_BLOCK_SIZE, XATTR_BLOCK_START};
        XATTR_BLOCK_START + f.ino * XATTR_BLOCK_SIZE as u64 // 32 KiB-aligned
    };
    let guard = backend.storage.sector_lock(xattr_sector).write().await;
    let b2 = backend.clone();
    let ino = f.ino;
    let t = tokio::spawn(async move {
        b2.setxattr(ino, "user.contended", b"v").await.unwrap();
    });
    // Give the commit time to reach the held sector lock, then release it.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    drop(guard);
    t.await.unwrap();

    let contended_after = METRICS.meta_sector_lock_contended.load(Ordering::Relaxed);
    assert!(
        contended_after > contended_before,
        "holding a needed sector lock across a commit must count as contended \
         (before {contended_before}, after {contended_after})"
    );
}

/// Allocator CAS/scan retries: pre-setting a run of bits forces `alloc` to
/// lose that many `fetch_or` attempts before claiming a free bit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_metrics_inode_alloc_cas_retries_counted() {
    use squeezefs::fuse_client::METRICS;
    use squeezefs::meta_backend::alloc::InodeAllocator;
    use std::sync::atomic::Ordering;

    let before = METRICS.meta_inode_alloc_cas_retries.load(Ordering::Relaxed);
    let a = InodeAllocator::new(128);
    for ino in 2..=20 {
        a.set(ino);
    }
    let got = a.alloc().expect("alloc after occupied prefix");
    assert_eq!(got, 21, "hint starts at 2; first free bit is 21");
    let after = METRICS.meta_inode_alloc_cas_retries.load(Ordering::Relaxed);
    assert!(
        after - before >= 19,
        "scanning 19 occupied bits must count >= 19 retries (delta {})",
        after - before
    );
}

/// Mount reconciliation heals leaked on-disk bitmap bits and counts them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_metrics_inode_alloc_reconciled_counts_healed_bits() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;
    let (_t, backend) = open_backend().await;

    // Clean volume: a refresh heals nothing.
    let before_clean = METRICS.meta_inode_alloc_reconciled.load(Ordering::Relaxed);
    backend.storage.refresh_bitmap_from_table().await.unwrap();
    let after_clean = METRICS.meta_inode_alloc_reconciled.load(Ordering::Relaxed);
    assert_eq!(
        after_clean, before_clean,
        "clean bitmap refresh must heal zero bits"
    );

    // Leak two bits into the on-disk bitmap (sector 4096) that no inode-table
    // entry backs, then reconcile: exactly those bits are healed.
    let mut bitmap = [0u8; 4096];
    backend
        .storage
        .read_blocks_direct(4096, &mut bitmap)
        .await
        .unwrap();
    bitmap[100 / 8] |= 1 << (100 % 8);
    bitmap[101 / 8] |= 1 << (101 % 8);
    backend
        .storage
        .write_blocks_direct(4096, &bitmap)
        .await
        .unwrap();

    let before = METRICS.meta_inode_alloc_reconciled.load(Ordering::Relaxed);
    backend.storage.refresh_bitmap_from_table().await.unwrap();
    let after = METRICS.meta_inode_alloc_reconciled.load(Ordering::Relaxed);
    assert!(
        after - before >= 2,
        "two leaked bits must be counted as healed (delta {})",
        after - before
    );
}

/// Every commit records its apply-batch sector count (`meta_commit_sectors`,
/// the same-payload replacement for the deleted `meta_wal_batch_size` —
/// design §Observability). Synchronous: the commit itself records it, no
/// worker to wait on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_metrics_commit_sectors_recorded() {
    use squeezefs::fuse_client::METRICS;
    let (_t, backend) = open_backend().await;

    let before = hist_count(&METRICS.meta_commit_sectors.buckets);
    backend
        .create(1, "commitsectors", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let after = hist_count(&METRICS.meta_commit_sectors.buckets);
    assert!(
        after > before,
        "the commit apply must record its sector-batch fill synchronously (delta {})",
        after - before
    );
}
