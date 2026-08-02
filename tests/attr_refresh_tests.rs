//! PR M5 (design-metadata-throughput §5.2 D2.c): the trailing-GETATTR
//! economy. The kernel invalidates its parent-dir attrs on every
//! unlink/rename (`fuse_dir_changed`) and re-GETATTRs them on the next
//! path walk — traffic the daemon cannot suppress (M2 measured GETATTR
//! 1.17/create and **1.82/unlink at ~45 µs each**, the biggest trailing-op
//! population). What the daemon CAN control is the **cost**: pre-M5 the
//! unlink/rename handlers also invalidated the daemon's own `attr_cache`
//! for the parent (and child), so each forced kernel GETATTR became a
//! contended backend fetch. D2.c refreshes instead: the handler re-seeds
//! the cache from the RAM-authoritative backend (exact values, monotone
//! through the M6 pending-times fold), so the kernel's revalidation is a
//! ~µs cache hit.
//!
//! Contract pinned here (per mutated inode):
//! - post-unlink: parent AND child attrs are PRESENT in the cache and
//!   byte-agree with backend truth (fresh mtime/ctime, decremented nlink);
//! - post-rename: both parents present-and-true;
//! - `fuse_attr_cache_refreshes` counts each refresh (the acceptance
//!   session's live signal);
//! - the values must come from the BACKEND, never a handler-side clock —
//!   pinned by exact equality with a subsequent backend read.
//!
//! Counter deltas are exact under the sanctioned serial gate
//! (`--test-threads=1`).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::Timestamp;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("attrref_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_9ABC_DEF0,
            uuid: *b"attr-refresh-v3!",
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

fn refreshes() -> u64 {
    METRICS.fuse_attr_cache_refreshes.load(Ordering::Relaxed)
}

fn ts(ns: u64) -> Timestamp {
    Timestamp::new((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
}

/// Read the RAW cached attr (never through getattr — `get_attr_internal`
/// would repopulate the cache on a miss and mask an invalidate).
fn cached_attr(h: &H, ino: u64) -> Option<fuse3::raw::reply::FileAttr> {
    h.fs.attr_cache.get(&ino).map(|(a, _)| a)
}

async fn backend_inode(h: &H, ino: u64) -> squeezefs::meta_backend::Inode {
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .getattr(ino)
        .await
        .unwrap()
}

async fn create(h: &H, parent: u64, name: &str) -> u64 {
    let ino =
        h.fs.create(h.req, parent, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    // Close the create handle so unlink -> reclaim lifecycles stay clean.
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();
    ino
}

async fn mkdir(h: &H, name: &str) -> u64 {
    h.fs.mkdir(h.req, 1, OsStr::new(name), 0o755, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Post-unlink, the parent's and the (still-alive-until-FORGET) child's
/// attrs are cached FRESH — present, and byte-agreeing with the backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_refreshes_parent_and_child_attr_cache() {
    let h = make().await;
    let dir = mkdir(&h, "d").await;
    let child = create(&h, dir, "victim").await;

    let r0 = refreshes();
    h.fs.unlink(h.req, dir, OsStr::new("victim")).await.unwrap();

    // Parent: present and true (the kernel's forced revalidation GETATTR
    // must be a cache hit serving the post-op times).
    let cached = cached_attr(&h, dir)
        .expect("post-unlink the PARENT attrs must be cached (refreshed, not invalidated)");
    let truth = backend_inode(&h, dir).await;
    assert_eq!(
        cached.mtime,
        ts(truth.mtime),
        "cached parent mtime must be the backend's post-unlink value"
    );
    assert_eq!(
        cached.ctime,
        ts(truth.ctime),
        "cached parent ctime must be the backend's post-unlink value"
    );

    // Child: present and true (nlink dropped; inode alive until FORGET).
    let cached_child = cached_attr(&h, child)
        .expect("post-unlink the CHILD attrs must be cached (refreshed, not invalidated)");
    let child_truth = backend_inode(&h, child).await;
    assert_eq!(cached_child.nlink, child_truth.nlink, "post-unlink nlink");
    assert_eq!(cached_child.ctime, ts(child_truth.ctime));

    assert_eq!(
        refreshes() - r0,
        2,
        "unlink must refresh exactly parent + child"
    );
}

/// Post-rename, both parents' attrs are cached fresh (the M6 +0.21
/// fuse_ops/rename revalidation lands on the cache, not the backend).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_refreshes_both_parents() {
    let h = make().await;
    let src_dir = mkdir(&h, "src").await;
    let dst_dir = mkdir(&h, "dst").await;
    create(&h, src_dir, "mover").await;

    let r0 = refreshes();
    h.fs.rename(
        h.req,
        src_dir,
        OsStr::new("mover"),
        dst_dir,
        OsStr::new("moved"),
    )
    .await
    .unwrap();

    for (dir, tag) in [(src_dir, "source parent"), (dst_dir, "dest parent")] {
        let cached = cached_attr(&h, dir)
            .unwrap_or_else(|| panic!("post-rename the {tag} attrs must be cached"));
        let truth = backend_inode(&h, dir).await;
        assert_eq!(
            cached.mtime,
            ts(truth.mtime),
            "{tag}: cached mtime must be the backend's post-rename value"
        );
    }
    assert_eq!(
        refreshes() - r0,
        2,
        "cross-dir rename with no dest refreshes exactly the two parents"
    );
}

/// Same-dir rename refreshes the one parent once (no double fetch).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_dir_rename_refreshes_parent_once() {
    let h = make().await;
    let dir = mkdir(&h, "one").await;
    create(&h, dir, "a").await;

    let r0 = refreshes();
    h.fs.rename(h.req, dir, OsStr::new("a"), dir, OsStr::new("b"))
        .await
        .unwrap();
    assert_eq!(
        refreshes() - r0,
        1,
        "same-parent rename must refresh the shared parent exactly once"
    );
    assert!(cached_attr(&h, dir).is_some());
}

/// Post-create, the PARENT's cached attrs must be the backend's post-bump
/// values — never the pre-create snapshot left over from an earlier
/// getattr.
///
/// Found by the VL10 release gate: pjdfstest `open/00.t` subtests 33–34
/// ("update parent directory ctime/mtime if file didn't exist") observed
/// the parent's PRE-create mtime/ctime through a stat issued immediately
/// after `open(O_CREAT)` — the create handler kept the parent's stale
/// cache entry ("Keep parent attr in cache") while the backend had just
/// bumped the parent's times, so every parent GETATTR inside the attr-TTL
/// window served pre-bump times. unlink/rename already refresh (D2.c);
/// create was the one dir-mutating handler that neither refreshed nor
/// invalidated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_refreshes_parent_attr_cache() {
    let h = make().await;
    let dir = mkdir(&h, "pdir").await;

    // Seed the daemon attr cache with the PRE-create parent attrs (the
    // kernel's path-walk GETATTR does exactly this before an open(O_CREAT)).
    let pre = h.fs.getattr(h.req, dir, None, 0).await.unwrap().attr;
    assert!(cached_attr(&h, dir).is_some(), "seed must be cached");

    // Advance the wall clock past the times' observable resolution so the
    // backend's parent-times bump provably differs from the seeded snapshot
    // (clock movement, not synchronization — no event is being awaited).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let r0 = refreshes();
    create(&h, dir, "newborn").await;

    let cached = cached_attr(&h, dir)
        .expect("post-create the PARENT attrs must be cached (refreshed, not left stale)");
    let truth = backend_inode(&h, dir).await;
    assert_eq!(
        cached.mtime,
        ts(truth.mtime),
        "cached parent mtime must be the backend's post-create value"
    );
    assert_eq!(
        cached.ctime,
        ts(truth.ctime),
        "cached parent ctime must be the backend's post-create value"
    );
    assert_ne!(
        cached.mtime, pre.mtime,
        "the backend bumped parent mtime on create — a cache still holding \
         the pre-create snapshot is the pjdfstest open/00.t 33-34 bug"
    );
    assert!(
        refreshes() - r0 >= 1,
        "create must account a parent attr refresh"
    );
}

/// fstests generic/258 repro-port (VL10 release gate): pre-epoch
/// (negative) timestamps must round-trip. The daemon stored times as
/// unsigned ns (`sec as u64 * 1e9`), so `utimensat` with a negative
/// second wrapped into year-576 territory ("Timestamp wrapped:
/// 18131146533"). Times are now i64 nanoseconds carried in the u64
/// storage word (two's complement — every existing positive value reads
/// identically), sign-restored at the FUSE boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_timestamps_round_trip() {
    let h = make().await;
    let ino = create(&h, 1, "epoch-minus").await;

    // The generic/258 shape: one day before the epoch.
    let want = Timestamp::new(-86_400, 0);
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            atime: Some(want),
            mtime: Some(want),
            ..Default::default()
        },
    )
    .await
    .expect("setattr with a negative timestamp");

    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        got.mtime, want,
        "pre-epoch mtime must round-trip, not wrap (generic/258)"
    );
    assert_eq!(got.atime, want, "pre-epoch atime must round-trip");
    assert!(got.mtime.sec < 0, "the sign must survive the storage word");

    // Sub-second negative shape too (sec = -1, nsec 500e6 = -0.5 s).
    let frac = Timestamp::new(-1, 500_000_000);
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mtime: Some(frac),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(got.mtime, frac, "negative sec + positive nsec round-trip");
}

/// fstests generic/426 repro-port (VL10 release gate): the fuse3 INIT
/// reply advertises `FUSE_EXPORT_SUPPORT`, whose kernel contract is that
/// `LOOKUP(nodeid, ".")` revives an evicted nodeid (the
/// `open_by_handle_at` decode path — `fuse_get_dentry`). The daemon
/// forwarded "." to the backend dentry walk, which stores no "."
/// records, so every handle whose inode had been evicted came back
/// ESTALE ("returned 116 incorrectly on a linked file"). `LOOKUP(".")`
/// must resolve to the nodeid ITSELF; `LOOKUP("..")` at the root must
/// resolve to the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_dot_revives_the_nodeid_itself() {
    let h = make().await;
    let dir = mkdir(&h, "exp").await;
    let file = create(&h, dir, "handle-me").await;

    for ino in [dir, file, 1] {
        let got =
            h.fs.lookup(h.req, ino, OsStr::new("."))
                .await
                .expect("LOOKUP(nodeid, \".\") must succeed — the EXPORT_SUPPORT contract")
                .attr;
        assert_eq!(got.ino, ino, "\".\" resolves to the nodeid itself");
    }

    let got =
        h.fs.lookup(h.req, 1, OsStr::new(".."))
            .await
            .expect("LOOKUP(root, \"..\") must succeed (root is its own parent)")
            .attr;
    assert_eq!(got.ino, 1);
}

/// The directory half of the EXPORT_SUPPORT contract (fstests
/// generic/467's "on a linked dir!" row): reconnecting an evicted
/// DIRECTORY handle walks `LOOKUP(nodeid, "..")` up to a connected
/// ancestor. No parent pointer exists in the inode record (v3 keeps the
/// linkage in the dentry tree only), so ".." resolves by a reverse
/// dentry scan — the rare, cold handle-reconnect path, priced and
/// documented at the resolver.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_dotdot_resolves_the_real_parent() {
    let h = make().await;
    let outer = mkdir(&h, "outer").await;
    let inner =
        h.fs.mkdir(h.req, outer, OsStr::new("inner"), 0o755, 0)
            .await
            .unwrap()
            .attr
            .ino;

    let got =
        h.fs.lookup(h.req, inner, OsStr::new(".."))
            .await
            .expect("LOOKUP(dir, \"..\") must resolve — directory handle reconnection")
            .attr;
    assert_eq!(got.ino, outer, "inner/.. is outer");

    let got =
        h.fs.lookup(h.req, outer, OsStr::new(".."))
            .await
            .expect("LOOKUP(outer, \"..\")")
            .attr;
    assert_eq!(got.ino, 1, "outer/.. is the root");
}

/// fstests generic/306 repro-port (VL10 release gate): device-node
/// rdev must PERSIST. mknod stored the mode but dropped the device
/// number (`rdev: 0` hardcoded in the attr conversion), so a mknod'd
/// `c 1 3` read back as a char device pointing at device 0:0 —
/// "No such device or address" on every open (306 writes to a
/// mknod'd null device on a ro-remounted fs). The rdev rides the
/// inode value's `rdev` wire word (32-bit new_encode_dev — historically
/// the reserved `flags2`, whose bit 0's retired migrate-era quarantine
/// meaning was never written by any live binary, so every existing
/// volume holds 0 there; zero on-disk format change).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mknod_rdev_round_trips() {
    let h = make().await;
    let rdev: u32 = libc::makedev(1, 3) as u32; // /dev/null's numbers

    let reply =
        h.fs.mknod(
            h.req,
            1,
            OsStr::new("nullnode"),
            libc::S_IFCHR | 0o666,
            rdev,
        )
        .await
        .expect("mknod");
    assert_eq!(reply.attr.rdev, rdev, "mknod reply carries the rdev");
    let ino = reply.attr.ino;

    // Through a COLD attr cache (the durable read path, not the reply
    // echo).
    h.fs.attr_cache.invalidate(&ino);
    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        got.rdev, rdev,
        "getattr after cache invalidation must serve the persisted rdev \
         (generic/306's devnull was 0:0)"
    );
    assert_eq!(got.kind, fuse3::FileType::CharDevice);
}

/// fstests generic/634 adjudication companion (VL10 release gate): the
/// on-disk timestamp word is i64 NANOSECONDS — a deliberate ±292-year
/// range (1677-09-21 .. 2262-04-11), the same class of finite-range
/// choice as ext4's u34 seconds or xfs bigtime. Out-of-range setattr
/// times must SATURATE deterministically to the range edge (never wrap,
/// never error) and the CLAMPED value must be what every subsequent
/// getattr serves — the daemon's half of the clamp-and-persist
/// contract. (The kernel half — incore clamping — needs sb->s_time_max,
/// which the FUSE protocol cannot advertise; that is 634's documented
/// expected shape in tests/run_fstests.sh.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_timestamps_saturate_deterministically() {
    let h = make().await;
    let ino = create(&h, 1, "y2514").await;

    // generic/634's u34_max: May 30 01:53:03 UTC 2514 — beyond i64 ns.
    let huge = Timestamp::new(17_179_869_183, 0);
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mtime: Some(huge),
            ..Default::default()
        },
    )
    .await
    .expect("out-of-range setattr must clamp, not error");
    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        got.mtime.sec, 9_223_372_036,
        "beyond-range mtime saturates to the i64-ns ceiling (2262-04-11)"
    );
    assert_eq!(got.mtime.nsec, 854_775_807);

    // The floor: year 0 — before the i64-ns floor (1677-09-21).
    let tiny = Timestamp::new(-62_167_219_200, 0);
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mtime: Some(tiny),
            ..Default::default()
        },
    )
    .await
    .expect("below-range setattr must clamp, not error");
    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        got.mtime.sec, -9_223_372_037,
        "below-range mtime saturates to the i64-ns floor (1677-09-21)"
    );

    // In-range values keep round-tripping exactly.
    let exact = Timestamp::new(7_956_915_742, 0); // all-twos, year 2222
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mtime: Some(exact),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(got.mtime, exact, "in-range times are exact (generic/634)");
}

/// fstests generic/683 (VL10 release gate): an unprivileged fallocate
/// must DROP suid/sgid — the vfs killpriv machinery covers write and
/// truncate in the kernel, but FUSE fallocate leaves the strip to the
/// daemon (683's Tests 1–4 golden: 6666 → 666 after a qa_user falloc;
/// the pre-fix daemon kept the non-group-exec sgid: 2666). Root keeps
/// its bits (683 Tests 5–8). The strip matches setattr_prepare's law:
/// suid always; sgid regardless of group-exec (the modern vfs shape
/// the 683 golden encodes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unprivileged_fallocate_drops_suid_sgid() {
    let h = make().await;
    let ino = create(&h, 1, "setuid.bin").await;
    // 6666: suid + sgid, non-exec (683 Test 1's shape).
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mode: Some(0o6666),
            size: Some(196_608),
            ..Default::default()
        },
    )
    .await
    .expect("seed mode+size");

    // An UNPRIVILEGED caller's fallocate (uid 1000 != root).
    let user_req = Request {
        unique: 2,
        uid: 1000,
        gid: 1000,
        pid: 2,
    };
    h.fs.fallocate(user_req, ino, 0, 0, 65_536, 0)
        .await
        .expect("fallocate");
    let got = backend_inode(&h, ino).await;
    assert_eq!(
        got.mode & 0o7777,
        0o666,
        "unprivileged fallocate must drop BOTH suid and sgid (generic/683)"
    );

    // Root keeps the bits (683 Test 5).
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mode: Some(0o6666),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let root_req = Request {
        unique: 3,
        uid: 0,
        gid: 0,
        pid: 3,
    };
    h.fs.fallocate(root_req, ino, 0, 0, 65_536, 0)
        .await
        .expect("root fallocate");
    let got = backend_inode(&h, ino).await;
    assert_eq!(
        got.mode & 0o7777,
        0o6666,
        "root fallocate keeps suid/sgid (generic/683 Test 5)"
    );
}

/// The kernel's inode-timestamp clock domain: `CLOCK_REALTIME_COARSE`
/// (what `inode_set_ctime_current()` reads — the stamp `fuse_link` /
/// write dirtying author LOCALLY for regular files under the writeback
/// cache we mount with). i64 ns in the u64 storage word, same convention
/// as the daemon's stamps.
fn kernel_coarse_now_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with a valid clock id and a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) };
    t.tv_sec.wrapping_mul(1_000_000_000).wrapping_add(t.tv_nsec) as u64
}

/// fstests generic/423 repro-port (statx `ts=C,c`, the hard-link leg) —
/// THE mechanism: under the default writeback cache the kernel authors a
/// regular file's link-ctime LOCALLY from `CLOCK_REALTIME_COARSE`
/// (`fuse_update_ctime` → `inode_set_ctime_current`), while the daemon
/// stamped every OTHER inode (423's socket) from the fine-grained
/// `CLOCK_REALTIME`. The fine clock runs AHEAD of the coarse clock by up
/// to one kernel tick (measured 1.85 ms on this host at HZ=1000), so a
/// daemon-stamped sibling created moments BEFORE an `ln` can carry a
/// ctime LATER than the kernel's link stamp — 423's observed 162 µs
/// nsec regression. The pinned contract: **a daemon-authored inode
/// timestamp must never exceed a coarse-clock reading taken AFTER the
/// operation returned** (that reading IS the earliest stamp the kernel
/// could author next). Every daemon stamp must ride the kernel's clock
/// domain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_inode_stamps_never_lead_the_kernel_coarse_clock() {
    let h = make().await;
    for round in 0..100u32 {
        let as_ns = |t: Timestamp| (t.sec as u64).wrapping_mul(1_000_000_000) + t.nsec as u64;
        let check = |round: u32, tag: &str, stamp: u64, fence: u64| {
            assert!(
                (stamp as i64) <= (fence as i64),
                "round {round}: {tag} {stamp} ns LEADS the kernel coarse clock \
                 ({fence} ns read AFTER the op) by {} ns — the kernel's next \
                 locally-authored wb-cache stamp (fuse_update_ctime at `ln`) \
                 would land BEHIND it: fstests generic/423 ts=C,c inversion",
                (stamp as i64) - (fence as i64),
            );
        };

        // create (423's mknod-the-socket analog: a fresh daemon-stamped
        // inode) …
        let f = create(&h, 1, &format!("c423-{round}")).await;
        let coarse = kernel_coarse_now_ns();
        let t = backend_inode(&h, f).await;
        check(round, "create ctime", t.ctime, coarse);
        check(round, "create mtime", t.mtime, coarse);

        // … a data write (the write path publishes its own attr times) …
        let w =
            h.fs.write(h.req, f, 0, 0, bytes::Bytes::from(vec![0xEEu8; 512]), 0, 0)
                .await
                .unwrap();
        assert_eq!(w.written, 512);
        let coarse = kernel_coarse_now_ns();
        let served = h.fs.getattr(h.req, f, None, 0).await.unwrap().attr;
        check(
            round,
            "post-write served mtime",
            as_ns(served.mtime),
            coarse,
        );
        check(
            round,
            "post-write served ctime",
            as_ns(served.ctime),
            coarse,
        );

        // … and link (the daemon-side ctime bump itself).
        let link_attr =
            h.fs.link(h.req, f, 1, OsStr::new(&format!("cl423-{round}")))
                .await
                .unwrap()
                .attr;
        let coarse = kernel_coarse_now_ns();
        let t = backend_inode(&h, f).await;
        check(round, "link-reply ctime", as_ns(link_attr.ctime), coarse);
        check(round, "post-link backend ctime", t.ctime, coarse);
    }
}

/// fstests generic/423 (statx `ts=C,c`, the hard-link leg): after
/// `link()`, the linked file's ctime must be >= the ctime/btime of ANY
/// object created before the link — and in particular >= its own
/// just-observed pre-link ctime and >= a sibling created moments before.
/// The failing shape on the gate: the post-link ctime landed ~160 µs
/// BEHIND a socket created before the `ln` ran (nsec regression within
/// the same second). Pin the whole ordering chain, tightly, many rounds
/// (the regression is a sub-millisecond clock/serve-path skew).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_ctime_is_monotone_against_prior_observations() {
    let h = make().await;
    for round in 0..200u32 {
        let fname = format!("f423-{round}");
        let sname = format!("s423-{round}");
        let lname = format!("l423-{round}");
        let f = create(&h, 1, &fname).await;

        // dd-analog data write (the write path publishes its own times).
        let w =
            h.fs.write(h.req, f, 0, 0, bytes::Bytes::from(vec![0xABu8; 4096]), 0, 0)
                .await
                .unwrap();
        assert_eq!(w.written, 4096);

        // Kernel writeback times-echo analog: a bare mtime/ctime SETATTR
        // (the absorb arm parks it as a pending refinement).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let echo = Timestamp::new(now.as_secs() as i64, now.subsec_nanos());
        h.fs.setattr(
            h.req,
            f,
            None,
            fuse3::SetAttr {
                mtime: Some(echo),
                ctime: Some(echo),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // The sibling created BEFORE the link (423's socket): its btime /
        // ctime are the reference the linked file must not fall behind.
        let s = create(&h, 1, &sname).await;
        let ref_attr = h.fs.getattr(h.req, s, None, 0).await.unwrap().attr;

        // Pre-link self-observation: the file's own ctime as a reference.
        let pre = h.fs.getattr(h.req, f, None, 0).await.unwrap().attr;

        // link() — must bump the file's ctime to now (>= both refs).
        let link_attr =
            h.fs.link(h.req, f, 1, OsStr::new(&lname))
                .await
                .unwrap()
                .attr;
        let post = h.fs.getattr(h.req, f, None, 0).await.unwrap().attr;

        for (tag, got) in [("link-reply", link_attr.ctime), ("getattr", post.ctime)] {
            let ge = |a: Timestamp, b: Timestamp| (a.sec, a.nsec) >= (b.sec, b.nsec);
            assert!(
                ge(got, ref_attr.ctime),
                "round {round}: {tag} ctime {}.{:09} regressed below the PRIOR sibling's \
                 ctime {}.{:09} (generic/423 ts=C,c)",
                got.sec,
                got.nsec,
                ref_attr.ctime.sec,
                ref_attr.ctime.nsec
            );
            assert!(
                ge(got, pre.ctime),
                "round {round}: {tag} ctime {}.{:09} regressed below the file's OWN \
                 pre-link ctime {}.{:09} (generic/423)",
                got.sec,
                got.nsec,
                pre.ctime.sec,
                pre.ctime.nsec
            );
        }
    }
}
