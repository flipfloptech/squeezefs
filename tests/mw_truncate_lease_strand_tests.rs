//! **The s11-range leg's finding, repro-ported** (the repro-port mandate;
//! S11 rung 15, KD-MW-7): an FD-LESS `truncate(2)` (SETATTR with `size`,
//! no open handle — the MPI rank-0 *create-truncate-then-ranks-write*
//! shape) on an **mw-armed** mount STRANDED its whole-file lease in
//! `active_leases`: with no open episode there is no last-close RELEASE to
//! retire it, and whole-file EX custody conflicts with every span — so
//! every co-writer's first range acquire on the shared file waited out its
//! whole budget and failed EIO, forever. Found live: both co-writers'
//! halves failed `dd: fsync failed … Input/output error` on the leg's
//! first run (2026-08-17), refusal text "held by foreign custody
//! overlapping REQUIRED after the 5s wait budget".
//!
//! The law pinned here, both directions:
//!
//! 1. **mw-armed** (a custody authority or co-writer client is installed):
//!    a size-bearing setattr with NO open handle releases its whole-file
//!    lease at op end — the object is range-grantable immediately after
//!    the truncate returns (the durable save already presented the token,
//!    so custody did its serialization work).
//! 2. **solo** (nothing installed — every shipped mount): the cached-lease
//!    reuse is UNCHANGED (the O_TRUNC transient-race fix depends on it,
//!    and nothing contends with a solo mount's cache): the lease stays
//!    cached and a foreign range acquire still refuses.
//!
//! RED against the pre-fix write path: arm 1 refuses the range acquire
//! (the stranded lease holds the file), verified by reverting the
//! setattr-side release.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_grant::{self, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The staged_truncate_stale_tests fixture, minimal: one file-backed
/// volume set, default 4 MiB blocks.
async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
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
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_5511_0015,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
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
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

/// Create a file the way the kernel's `truncate(2)`-by-path leaves it:
/// created, then CLOSED (no live handle — the fd-less shape).
async fn create_closed(h: &H, name: &str) -> u64 {
    let created =
        h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    let ino = created.attr.ino;
    h.fs.release(h.req, ino, created.fh, 0, 0, false)
        .await
        .unwrap();
    ino
}

async fn truncate_to(h: &H, ino: u64, size: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// The finding, both directions in ONE serialized test (the custody-owner
/// install is process-global; one test owns it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fd_less_truncate_strands_no_lease_on_mw_and_keeps_the_solo_cache() {
    // ---- Arm 1: mw-armed (the leg's shape) --------------------------------
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("clock law");
    let owner = WriteCustodyOwner::arm(
        "strand-owner",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        LeaseClock::monotonic(),
        None,
    )
    .expect("authority arms");
    data_grant::install_custody_owner(owner);

    let h = make(*b"strand-lease-mw!", "strand-mw").await;
    let ino = create_closed(&h, "rank0-created.dat").await;
    truncate_to(&h, ino, 64 * 1024 * 1024).await;

    // The law: the fd-less truncate on an mw-armed mount left NO stranded
    // whole-file custody — a range acquire (the co-writers' first ask)
    // grants within a short budget instead of starving into EIO.
    let peer = DlmClient::new().unwrap();
    let path = format!("inode_{ino}");
    let lease = tokio::time::timeout(
        Duration::from_secs(5),
        peer.acquire_lock(&path, Some((0, 4096)), Duration::from_millis(800)),
    )
    .await
    .expect("range acquire hung")
    .unwrap_or_else(|e| {
        panic!(
            "the fd-less truncate STRANDED its whole-file lease on an mw-armed \
             mount — the peer's range acquire starved: {e:?} (the s11-range \
             leg's live finding, 2026-08-17)"
        )
    });
    lease.release().await.expect("release");
    data_grant::uninstall_custody_owner();

    // ---- Arm 2: solo — the cached-lease reuse is UNCHANGED ----------------
    let ino2 = create_closed(&h, "solo-truncated.dat").await;
    truncate_to(&h, ino2, 64 * 1024 * 1024).await;
    let path2 = format!("inode_{ino2}");
    assert!(
        peer.acquire_lock(&path2, Some((0, 4096)), Duration::from_millis(300))
            .await
            .is_err(),
        "a SOLO mount's setattr must keep its cached whole-file lease (the \
         O_TRUNC transient-race fix's reuse — nothing contends with a solo \
         mount's cache, and dropping it would re-open that race)"
    );
}
