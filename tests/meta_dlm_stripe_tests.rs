//! Meta-DLM stripe-conversion contracts.
//!
//! The per-metadata-op lock manager moves from an unbounded
//! `DashMap<String, Arc<RwLock>>` (3 heap allocations per acquisition) to
//! fixed per-class stripe arrays (`I`-locks by inode, `D`-locks by
//! `(parent, name)`), selected by hash — zero allocation, zero map growth.
//!
//! Stripes make *collisions* part of the model: two distinct objects may
//! share one lock. That is only benign if every multi-lock operation
//! acquires in canonical order — all `I` locks before any `D` lock, each
//! class deduplicated by stripe index and acquired in ascending order, with
//! child inodes discovered under a first phase re-locked and re-validated in
//! a second. These tests force the collisions deterministically (via the
//! manager's public stripe accessors) and assert bounded completion and
//! correct results where naive striping self-deadlocks (same-stripe double
//! acquire) or ABBA-deadlocks (I-after-D acquisition orders).

use squeezefs::meta_backend::dlm::DlmLockManager;
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata, RoutedMetaBackend};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

const ROOT: u64 = 1;

async fn backend() -> (Arc<RoutedMetaBackend>, NamedTempFile) {
    let f = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(f.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&ms).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![Arc::new(MetaLvBackend::new(
        ms,
    ))]));
    (routed, f)
}

async fn mk_file(b: &RoutedMetaBackend, parent: u64, name: &str) -> u64 {
    b.create(parent, name, libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"))
        .ino
}

async fn mk_dir(b: &RoutedMetaBackend, parent: u64, name: &str) -> u64 {
    b.create(parent, name, libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap_or_else(|e| panic!("mkdir {name}: {e:?}"))
        .ino
}

/// Create files until two of them share an inode stripe; returns their inos.
async fn colliding_ino_pair(b: &RoutedMetaBackend, dlm: &DlmLockManager, dir: u64) -> (u64, u64) {
    let mut seen: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    for i in 0..4096 {
        let ino = mk_file(b, dir, &format!("cf_{i}")).await;
        let stripe = dlm.inode_stripe(ino);
        if let Some(&prev) = seen.get(&stripe) {
            return (prev, ino);
        }
        seen.insert(stripe, ino);
    }
    panic!("no inode stripe collision in 4096 creates");
}

/// Two distinct names in `dir` that share a dentry stripe.
fn colliding_dentry_names(dlm: &DlmLockManager, dir: u64) -> (String, String) {
    let mut seen: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    for i in 0..100_000 {
        let name = format!("dn_{i}");
        let stripe = dlm.dentry_stripe(dir, &name);
        if let Some(prev) = seen.get(&stripe) {
            return (prev.clone(), name);
        }
        seen.insert(stripe, name);
    }
    panic!("no dentry stripe collision in 100k names");
}

/// Contract 1: `link` where the child's inode stripe collides with the
/// parent directory's must complete (naive striping double-acquires the
/// same exclusive stripe and self-deadlocks) and produce a correct nlink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_link_parent_child_inode_stripe_collision() {
    let (b, _f) = backend().await;
    let dlm = b.volume_dlm(0);

    // A directory and a file whose inode stripes collide: create dirs until
    // one collides with a subsequently created file (or vice versa).
    let mut pair = None;
    let mut dirs: Vec<(usize, u64)> = Vec::new();
    for i in 0..2048 {
        let d = mk_dir(&b, ROOT, &format!("cd_{i}")).await;
        dirs.push((dlm.inode_stripe(d), d));
        let f = mk_file(&b, ROOT, &format!("cx_{i}")).await;
        let fs_ = dlm.inode_stripe(f);
        if let Some(&(_, d_hit)) = dirs.iter().find(|(s, _)| *s == fs_) {
            pair = Some((d_hit, f));
            break;
        }
    }
    let (dir, file) = pair.expect("no dir/file inode stripe collision in 2048 rounds");

    let linked = tokio::time::timeout(
        Duration::from_secs(10),
        b.link(file, dir, "hardlink_collide"),
    )
    .await
    .expect("link self-deadlocked on colliding parent/child inode stripes")
    .expect("link failed");
    assert_eq!(linked.nlink, 2, "nlink after link");

    let via_dir = b.lookup(dir, "hardlink_collide").await.expect("lookup");
    assert_eq!(via_dir.ino, file);
}

/// Contract 2: rename where BOTH dentry names collide on one dentry stripe
/// (including RENAME_EXCHANGE) must complete and produce exact results —
/// dedup-by-stripe means both names are guarded by a single exclusive lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_and_exchange_on_colliding_dentry_stripes() {
    let (b, _f) = backend().await;
    let dlm = b.volume_dlm(0);
    let dir = mk_dir(&b, ROOT, "rn").await;
    let (name_a, name_b) = colliding_dentry_names(dlm, dir);

    let ino_a = mk_file(&b, dir, &name_a).await;

    // Plain rename a -> b (colliding stripes).
    tokio::time::timeout(
        Duration::from_secs(10),
        b.rename(dir, &name_a, dir, &name_b, 0),
    )
    .await
    .expect("rename self-deadlocked on colliding dentry stripes")
    .expect("rename failed");
    assert_eq!(b.lookup(dir, &name_b).await.expect("lookup b").ino, ino_a);
    assert!(
        b.lookup(dir, &name_a).await.is_err(),
        "old name must be gone"
    );

    // RENAME_EXCHANGE with colliding stripes.
    let ino_c = mk_file(&b, dir, &name_a).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        b.rename(dir, &name_a, dir, &name_b, libc::RENAME_EXCHANGE),
    )
    .await
    .expect("exchange self-deadlocked on colliding dentry stripes")
    .expect("exchange failed");
    assert_eq!(b.lookup(dir, &name_a).await.expect("a").ino, ino_a);
    assert_eq!(b.lookup(dir, &name_b).await.expect("b").ino, ino_c);
}

/// Contract 3: the ABBA shape — create holds `I{parent}` then wants
/// `D{parent,name}`; unlink holds `D` then (naively) wants a colliding
/// `I{child}`. With engineered I- and D-collisions across two directories,
/// interleaved create/unlink storms must stay deadlock-free (canonical
/// class-layered order: unlink re-locks child inos in a validated second
/// phase, never taking `I` after `D`).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_create_unlink_storm_across_colliding_stripes() {
    let (b, _f) = backend().await;
    let dlm = b.volume_dlm(0);

    let dir_x = mk_dir(&b, ROOT, "sx").await;
    let dir_y = mk_dir(&b, ROOT, "sy").await;
    let (fx, fy) = colliding_ino_pair(&b, dlm, dir_x).await;
    // Names in dir_y whose dentry stripes collide with names in dir_x.
    let (nx, ny) = {
        let mut hit = None;
        for i in 0..100_000 {
            let cand = format!("ab_{i}");
            for j in 0..64 {
                let other = format!("ba_{j}");
                if dlm.dentry_stripe(dir_x, &cand) == dlm.dentry_stripe(dir_y, &other) {
                    hit = Some((cand.clone(), other));
                    break;
                }
            }
            if hit.is_some() {
                break;
            }
        }
        hit.expect("no cross-dir dentry stripe collision found")
    };
    // Hard links of the colliding-ino files under the colliding names give
    // the storm below maximum lock overlap.
    b.link(fx, dir_x, &nx).await.expect("seed link x");
    b.link(fy, dir_y, &ny).await.expect("seed link y");

    let deadline = Duration::from_secs(60);
    let storm = async {
        let mut tasks = Vec::new();
        for t in 0..8u32 {
            let b = b.clone();
            let (dir_a, dir_b) = if t % 2 == 0 {
                (dir_x, dir_y)
            } else {
                (dir_y, dir_x)
            };
            let (na, nb) = if t % 2 == 0 {
                (nx.clone(), ny.clone())
            } else {
                (ny.clone(), nx.clone())
            };
            let (fa, fb) = if t % 2 == 0 { (fx, fy) } else { (fy, fx) };
            tasks.push(tokio::spawn(async move {
                for round in 0..50u32 {
                    let tag = format!("{na}_t{t}_{round}");
                    // create -> lookup -> unlink in dir_a, racing the mirror
                    // task doing the same in dir_b with colliding stripes.
                    let _ = b.link(fa, dir_a, &tag).await;
                    let _ = b.lookup(dir_a, &tag).await;
                    let _ = b.unlink(dir_a, &tag).await;
                    let _ = b.getattr(fb).await;
                    let _ = b.lookup(dir_b, &nb).await;
                }
            }));
        }
        for t in tasks {
            t.await.expect("storm task panicked");
        }
    };
    tokio::time::timeout(deadline, storm)
        .await
        .expect("create/unlink storm deadlocked across colliding stripes");

    // Both seed links must still resolve — nothing was lost to the storm.
    assert_eq!(b.lookup(dir_x, &nx).await.expect("nx").ino, fx);
    assert_eq!(b.lookup(dir_y, &ny).await.expect("ny").ino, fy);
}

/// Contract 4: shared/exclusive mixing on one collided inode stripe —
/// getattr storms against setattr must complete with intact attributes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_shared_exclusive_mix_on_collided_inode_stripe() {
    let (b, _f) = backend().await;
    let dlm = b.volume_dlm(0);
    let dir = mk_dir(&b, ROOT, "mx").await;
    let (a, c) = colliding_ino_pair(&b, dlm, dir).await;

    let work = async {
        let mut tasks = Vec::new();
        for t in 0..4u32 {
            let b = b.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..100u32 {
                    let mode = 0o600 + ((i + t) % 8);
                    let got = b
                        .setattr(
                            a,
                            Some(libc::S_IFREG | mode),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                        .await
                        .expect("setattr");
                    assert_eq!(got.mode & 0o777, mode, "setattr result torn");
                    let ga = b.getattr(c).await.expect("getattr");
                    assert_eq!(ga.ino, c);
                }
            }));
        }
        for t in tasks {
            t.await.expect("mix task panicked");
        }
    };
    tokio::time::timeout(Duration::from_secs(30), work)
        .await
        .expect("shared/exclusive stripe mix deadlocked");
}

/// Contract 5: unlink's two-phase child discovery must revalidate — racing
/// rename of the same dentry never double-frees, never panics, and always
/// leaves exactly one consistent outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_unlink_vs_rename_revalidation_races() {
    let (b, _f) = backend().await;
    let dir = mk_dir(&b, ROOT, "rv").await;

    for round in 0..100u32 {
        let name = format!("victim_{round}");
        let alt = format!("moved_{round}");
        let ino = mk_file(&b, dir, &name).await;

        let b1 = b.clone();
        let b2 = b.clone();
        let n1 = name.clone();
        let (n2, a2) = (name.clone(), alt.clone());
        let (r_unlink, r_rename) = tokio::join!(
            tokio::spawn(async move { b1.unlink(dir, &n1).await }),
            tokio::spawn(async move { b2.rename(dir, &n2, dir, &a2, 0).await }),
        );
        let r_unlink = r_unlink.expect("unlink task panicked");
        let r_rename = r_rename.expect("rename task panicked");

        let at_old = b.lookup(dir, &name).await;
        let at_new = b.lookup(dir, &alt).await;
        assert!(at_old.is_err(), "round {round}: old name must be gone");
        match (r_unlink.is_ok(), r_rename.is_ok()) {
            (true, true) => {
                // rename won the dentry then unlink consumed it at the new
                // name — impossible: unlink targets the old name. Both
                // succeeding is only legal if unlink got the dentry first
                // and rename... cannot have. Flag loudly.
                panic!("round {round}: unlink and rename both claimed the dentry");
            }
            (true, false) => {
                assert!(at_new.is_err(), "round {round}: unlink won but alt exists");
            }
            (false, true) => {
                assert_eq!(
                    at_new.expect("alt must exist when rename won").ino,
                    ino,
                    "round {round}: rename won but alt wrong"
                );
                b.unlink(dir, &alt).await.expect("cleanup unlink");
            }
            (false, false) => {
                panic!("round {round}: both unlink and rename failed: {r_unlink:?} {r_rename:?}");
            }
        }
    }
}
