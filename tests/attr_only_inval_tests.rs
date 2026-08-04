//! The generic/451 fix's flagged follow-ups (2026-08-04 sweep): the two
//! daemon-initiated ATTR-STAKES `notify_inval_inode` pushes — the
//! generic/683 fallocate setid strip and the killpriv-v2 clear — must
//! carry the **attrs-only form**.
//!
//! The kernel contract (`fuse_reverse_inval_inode`, fs/fuse/inode.c;
//! encoded verbatim by the fork — `crates/fuse3/src/notify.rs`
//! `inval_inode_frame` → `fuse_notify_inval_inode_out { ino, off: i64,
//! len: i64 }`):
//!
//! - `off >= 0` ⇒ invalidate pages from `off`, `len <= 0` ⇒ **to EOF**;
//! - `off < 0`  ⇒ drop the cached attrs (and ACLs) alone — **no page is
//!   touched**.
//!
//! The found bug: both sites passed `(0, 0)` — a WHOLE-FILE page
//! invalidation — for a change whose stakes are the cached mode bits. On
//! a hot mmap'd/page-cached file one unprivileged fallocate (or one
//! flagged write clearing suid) nuked the entire page cache: a perf
//! hazard and a needless refetch storm, for an invalidation whose intent
//! ("push an attrs-only INVAL_INODE so the very next stat refetches")
//! was attrs-only all along. The W1 interception hook already encodes
//! the same law (`InvalScope::AttrsOnly` ⇒ `(-1, 0)` —
//! `src/ipc_service.rs`); these two sites must match it.
//!
//! The sink is injectable (the `dio_inval_sink` /
//! `ipc_service::Invalidator` precedent): production pushes through the
//! fuse3 Notify handle; this suite records the exact `(ino, off, len)`
//! triple the kernel would receive.

use fuse3::raw::flags::FUSE_WRITE_KILL_SUIDGID;
use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::SetAttr;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{AttrInvalSink, SqueezefsFilesystem};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, NamedTempFile, TempDir};

/// `off < 0` ⇒ attrs-only; the canonical form (the W1 hook's
/// `InvalScope::AttrsOnly` arm and libfuse's
/// `fuse_lowlevel_notify_inval_inode` "negative off" convention).
const ATTRS_ONLY: (i64, i64) = (-1, 0);

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    /// Every `(ino, off, len)` FUSE_NOTIFY_INVAL_INODE triple the
    /// daemon-initiated attr-invalidation path pushed, verbatim.
    invals: Arc<Mutex<Vec<(u64, i64, i64)>>>,
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
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new("attrinval_test").await.unwrap());
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
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xA774_1214_C0FF_EE00,
            uuid: *b"attr-inval-form!",
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

    // The injectable recorder (the dio_inval_sink precedent): record the
    // exact triple; production wires the fuse3 Notify push here.
    let invals: Arc<Mutex<Vec<(u64, i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = invals.clone();
    let sink: AttrInvalSink = Arc::new(move |ino, off, len| {
        log.lock().unwrap().push((ino, off, len));
        Box::pin(std::future::ready(())) as futures::future::BoxFuture<'static, ()>
    });
    fs.attr_inval_sink.store(Arc::new(Some(sink)));

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
        invals,
        _b: b,
        _m: m,
        _s: s,
    }
}

fn invals(h: &H) -> Vec<(u64, i64, i64)> {
    h.invals.lock().unwrap().clone()
}

/// Create a regular file under root and give it `perm` through the
/// SETATTR handler (a real chmod — the attr cache carries the priv'd
/// mode too, exactly the killpriv fixture's shape).
async fn create_with_mode(h: &H, name: &str, perm: u32) -> u64 {
    let ino =
        h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    h.fs.setattr(
        h.req,
        ino,
        None,
        SetAttr {
            mode: Some(perm),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    ino
}

async fn backend_mode(h: &H, ino: u64) -> u32 {
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .getattr(ino)
        .await
        .unwrap()
        .mode
        & 0o7777
}

/// generic/683's follow-up: the setid strip after an unprivileged
/// fallocate changes ONLY the mode bits — its kernel invalidation must
/// be the ATTRS-ONLY form (`off < 0`), never `(0, 0)` (= drop every
/// page from offset 0 to EOF: a whole-file page-cache nuke on a hot
/// mmap'd/page-cached file, for an attr change).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallocate_setid_strip_pushes_attrs_only_inval() {
    let h = make().await;
    let ino = create_with_mode(&h, "setuid.bin", 0o6766).await;

    // An UNPRIVILEGED caller's extending fallocate (683 Test 1's shape).
    let user_req = Request {
        unique: 2,
        uid: 1000,
        gid: 1000,
        pid: 2,
        ..Default::default()
    };
    h.fs.fallocate(user_req, ino, 0, 0, 65_536, 0)
        .await
        .expect("fallocate");

    assert_eq!(
        backend_mode(&h, ino).await,
        0o766,
        "sanity: the strip itself ran (generic/683)"
    );
    assert_eq!(
        invals(&h),
        vec![(ino, ATTRS_ONLY.0, ATTRS_ONLY.1)],
        "the setid-strip invalidation must be ATTRS-ONLY (off < 0 — \
         fuse_reverse_inval_inode touches no page); (0, 0) is a \
         whole-file page invalidation (len <= 0 reads as to-EOF), which \
         drops every cached page of a hot file for a mode-bit change"
    );
}

/// The killpriv-v2 clear (FUSE_WRITE_KILL_SUIDGID) is the same class:
/// the mode moved, nothing else — attrs-only form required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killpriv_clear_pushes_attrs_only_inval() {
    let h = make().await;
    let ino = create_with_mode(&h, "suid", 0o4755).await;

    h.fs.write(
        h.req,
        ino,
        ino,
        0,
        bytes::Bytes::from_static(b"killpriv payload"),
        FUSE_WRITE_KILL_SUIDGID,
        0,
    )
    .await
    .unwrap();

    assert_eq!(
        backend_mode(&h, ino).await,
        0o755,
        "sanity: the flagged write cleared suid"
    );
    assert_eq!(
        invals(&h),
        vec![(ino, ATTRS_ONLY.0, ATTRS_ONLY.1)],
        "the killpriv clear's invalidation must be ATTRS-ONLY (off < 0), \
         never the (0, 0) whole-file page invalidation"
    );
}

/// The economy half: paths whose mode did NOT move push nothing at all —
/// a root fallocate (bits kept), an unprivileged fallocate on a
/// no-setid file, and a flagged write on a clean ino.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmoved_mode_pushes_no_invalidation() {
    let h = make().await;

    // Root keeps its bits (683 Test 5) — no strip, no push.
    let kept = create_with_mode(&h, "root_kept", 0o6766).await;
    let root_req = Request {
        unique: 3,
        uid: 0,
        gid: 0,
        pid: 3,
        ..Default::default()
    };
    h.fs.fallocate(root_req, kept, 0, 0, 65_536, 0)
        .await
        .expect("root fallocate");

    // No setid bits — the strip short-circuits.
    let plain = create_with_mode(&h, "plain", 0o644).await;
    let user_req = Request {
        unique: 4,
        uid: 1000,
        gid: 1000,
        pid: 4,
        ..Default::default()
    };
    h.fs.fallocate(user_req, plain, 0, 0, 65_536, 0)
        .await
        .expect("fallocate");

    // Flagged write on a clean ino — the killpriv latch short-circuits.
    h.fs.write(
        h.req,
        plain,
        plain,
        0,
        bytes::Bytes::from_static(b"clean"),
        FUSE_WRITE_KILL_SUIDGID,
        0,
    )
    .await
    .unwrap();

    assert!(
        invals(&h).is_empty(),
        "no mode movement ⇒ no kernel invalidation owed (the per-transition \
         economy: the push is never per-op)"
    );
}
