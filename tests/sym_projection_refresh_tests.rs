//! **A JOINED appender's PROJECTION tree follows a recycled root** (symmetric
//! PR 13 — found by the fleet's `sym-scale` N = 8 row on the defect-17
//! binary: joiner m63 read `traversal retry budget exhausted descending to
//! level 0 (routing loop — SMO protocol bug) restarts [root-seq] = 256` on
//! a user op one second after `dropped 1 stale projection node(s) inside a
//! fresh extent grant`, and its create storm failed).
//!
//! The shape: a joiner holds tree 0 (and the manager's native slot tree)
//! as a PROJECTION — a `KvTree` whose root pointer is the one it adopted
//! at open or at its last refresh. The manager compacts the tree (a root
//! swap), frees the old root's extent at the covering checkpoint, and the
//! free extent is RE-GRANTED — to the joiner itself, whose granted-extent
//! barrier (`drop_nodes_in_extents`) drops the stale image and whose next
//! mint writes a fresh node there, or to a peer whose write lands on the
//! device. Either way the image under the projection's root address now
//! carries ANOTHER node's seq, and `KvTree::descend` — which re-reads the
//! root from the tree's own pointer at every restart — spins its whole
//! budget on `root-seq` and answers `Corrupt`: an EIO to the user, for as
//! long as nothing refreshes the projection.
//!
//! The law: a root-pointer restart on a tree this mount does not WRITE
//! runs the installed projection refresh ([`NodeCache::install_projection_
//! refresh`] — the joiner's `refresh_control_projection`, which re-adopts
//! the manager's newest ledger root; the free of the old extent needed the
//! checkpoint that named the new root) every few restarts, and the walk
//! restarts on the root it installs. A writer's own trees never take the
//! arm (their root is live), nor does any flat mount (no hook, no armed
//! gate). Gauge `meta_kv_projection_root_refreshes`.
//!
//! Pinned at the tree level (deterministic; the fleet's race is not): a
//! projection-posture cache (the gate armed, NOT the manager), tree A
//! created and its root extent E0 recorded; the barrier drops A's image;
//! E0 is released and tree B — the "manager's compaction" — claims it and
//! writes its own root there under a fresh seq; A's lookup, whose root
//! pointer still names `(E0, seq_A)`, meets `seq_B`. With the hook
//! installed (it re-points A at B's root) the lookup SERVES; without it
//! (the base) the restart budget is exhausted — `Corrupt`, naming the tree,
//! its slot, the root pointer and "a PROJECTION here".

use squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator;
use squeezefs::meta_backend::kv::node::NodeLayout;
use squeezefs::meta_backend::kv::node_cache::{NodeCache, NodeCacheConfig, ProjectionRefresh};
use squeezefs::meta_backend::kv::record::{inode_key, TREE_CONTROL};
use squeezefs::meta_backend::kv::tree::{KvTree, RootPtr, SmoContext};
use squeezefs::meta_backend::kv::META_KV_PROJECTION_ROOT_REFRESHES;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::NamedTempFile;

const NODE_SIZE: usize = 64 * 1024;
const EXTENTS: u64 = 64;

struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    alloc: Arc<ExtentAllocator>,
    seq: Arc<squeezefs::meta_backend::kv::node_seq::NodeSeqHandle>,
}

impl Vol {
    fn new() -> Self {
        let file = NamedTempFile::new().expect("temp volume");
        file.as_file()
            .set_len(EXTENTS * NODE_SIZE as u64)
            .expect("size volume");
        let layout = NodeLayout::new(NODE_SIZE).expect("layout");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout,
            heap_base: 0,
            budget_bytes: EXTENTS * NODE_SIZE as u64,
            writeback_delta_bytes: 1024 * 1024,
        });
        let alloc = Arc::new(ExtentAllocator::format(EXTENTS, 0, 4096));
        let seq = Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(0));
        Self {
            _file: file,
            cache,
            alloc,
            seq,
        }
    }

    async fn control_tree(&self) -> KvTree {
        let mut ctx = SmoContext::new(self.alloc.clone());
        KvTree::create(self.cache.clone(), &mut ctx, TREE_CONTROL, self.seq.clone())
            .await
            .expect("create tree")
    }

    fn extent_of(&self, addr: u64) -> u64 {
        addr / NODE_SIZE as u64
    }
}

/// The joiner's refresh, stood in: re-point `target` at `newer` (what
/// `refresh_control_projection` does off the manager's ledger record).
struct RepointHook {
    target: Arc<KvTree>,
    newer: RootPtr,
    calls: AtomicU64,
}

impl ProjectionRefresh for RepointHook {
    fn refresh<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.target.root() == self.newer {
                return false;
            }
            self.target
                .install_recovered_root(self.newer, 0)
                .await
                .expect("the refresh installs the newer root");
            true
        })
    }
}

/// Stand the recycled-root shape: tree A (the projection) whose root
/// extent is re-claimed by tree B under a fresh seq. Returns `(A, B)`.
async fn recycled_root(vol: &Vol) -> (Arc<KvTree>, KvTree) {
    // The joiner's posture: the gate armed, not the manager — tree 0 is a
    // projection here.
    let gate = vol.cache.lease_gate();
    gate.arm();
    gate.test_set_manager(false);
    assert!(
        vol.cache.is_projection(None),
        "tree 0 is a projection on a joiner"
    );

    let a = Arc::new(vol.control_tree().await);
    a.insert(&inode_key(7), &b"seven"[..])
        .await
        .expect("a record in A");
    let root_a = a.root();
    assert_eq!(
        a.lookup(&inode_key(7)).await.unwrap().as_deref(),
        Some(&b"seven"[..])
    );

    // The granted-extent barrier: A's root image dropped (a projection
    // fold, never refused), its extent released — the manager's
    // compaction freed it — and B claims it: the joiner's mint image, or a
    // peer's node, under a fresh seq at the SAME address.
    let e0 = vol.extent_of(root_a.addr);
    assert_eq!(
        vol.cache
            .drop_nodes_in_extents(0, &[e0])
            .expect("the barrier"),
        1,
        "A's root image dropped"
    );
    vol.alloc.release_unpublished(e0);
    let b = vol.control_tree().await;
    b.insert(&inode_key(7), &b"seven-compacted"[..])
        .await
        .expect("a record in B");
    assert_eq!(
        b.root().addr,
        root_a.addr,
        "B's root took A's recycled extent (lowest-free-first)"
    );
    assert_ne!(b.root().seq, root_a.seq, "under a fresh seq");
    (a, b)
}

/// The base shape: no hook installed — the traversal exhausts its budget
/// on `root-seq`, and the error names the tree, its slot, the root
/// pointer and the projection posture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projection_whose_root_was_recycled_exhausts_its_budget_without_a_refresh() {
    let vol = Vol::new();
    let (a, _b) = recycled_root(&vol).await;
    let root_a = a.root();
    let err = a
        .lookup(&inode_key(7))
        .await
        .expect_err("the recycled root never converges without a refresh")
        .to_string();
    assert!(
        err.contains("routing loop") && err.contains("a PROJECTION here"),
        "the error names the shape: {err}"
    );
    assert!(
        err.contains(&format!("{:#x}@{}", root_a.addr, root_a.seq)),
        "and the stale root pointer: {err}"
    );
}

/// The law: with the joiner's refresh installed, the projection follows
/// the recycled root — the lookup SERVES B's record, the hook ran (once
/// per `PROJECTION_REFRESH_EVERY` root restarts, so ≥ 1), and the gauge
/// moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projection_whose_root_was_recycled_follows_the_manager_through_the_refresh() {
    let vol = Vol::new();
    let (a, b) = recycled_root(&vol).await;
    let hook = Arc::new(RepointHook {
        target: Arc::clone(&a),
        newer: b.root(),
        calls: AtomicU64::new(0),
    });
    assert!(vol.cache.install_projection_refresh(hook.clone()));
    let before = META_KV_PROJECTION_ROOT_REFRESHES.load(Ordering::Relaxed);
    let got = a
        .lookup(&inode_key(7))
        .await
        .expect("the projection follows the manager's newer root");
    assert_eq!(got.as_deref(), Some(&b"seven-compacted"[..]));
    assert!(hook.calls.load(Ordering::Relaxed) >= 1, "the refresh ran");
    assert_eq!(
        META_KV_PROJECTION_ROOT_REFRESHES.load(Ordering::Relaxed),
        before + 1,
        "one refresh advanced the projection"
    );
    assert_eq!(a.root(), b.root(), "A stands on B's root now");
    // Idempotent: the next lookup restarts nothing.
    let calls = hook.calls.load(Ordering::Relaxed);
    a.lookup(&inode_key(7)).await.expect("served");
    assert_eq!(hook.calls.load(Ordering::Relaxed), calls);
}

/// A WRITER's own tree never takes the arm: on the manager (or an
/// unarmed mount) the hook is not consulted — the root is live, and a
/// stale-root loop there IS the SMO protocol bug the budget exists to
/// name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writers_own_tree_never_consults_the_projection_refresh() {
    let vol = Vol::new();
    // The manager's posture.
    let gate = vol.cache.lease_gate();
    gate.arm();
    gate.arm_as_manager();
    assert!(
        !vol.cache.is_projection(None),
        "tree 0 is the manager's own"
    );
    let a = Arc::new(vol.control_tree().await);
    a.insert(&inode_key(7), &b"seven"[..]).await.unwrap();
    let hook = Arc::new(RepointHook {
        target: Arc::clone(&a),
        newer: a.root(),
        calls: AtomicU64::new(0),
    });
    assert!(vol.cache.install_projection_refresh(hook.clone()));
    assert_eq!(
        a.lookup(&inode_key(7)).await.unwrap().as_deref(),
        Some(&b"seven"[..])
    );
    assert_eq!(
        hook.calls.load(Ordering::Relaxed),
        0,
        "never consulted on a live tree"
    );
}
