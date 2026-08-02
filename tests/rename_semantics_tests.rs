//! Rename semantics found by the VL10 release gate's fstests `-g auto`
//! sweep (repro-port mandate):
//!
//! - **generic/035** (`t_rename_overwrite`, directory leg): renaming a
//!   directory over another directory must zero the overwritten
//!   directory's nlink — an open fd's fstat shows `st_nlink == 0` (an
//!   empty dir loses both its "." self-link and its parent entry). The
//!   pre-fix dest-replace arm decremented once (2 → 1), the unlink/rmdir
//!   path already had the rule (`routed_unlink_local`: directory child ⇒
//!   nlink 0).
//! - **generic/078** (`_require_renameat2 whiteout`): RENAME_WHITEOUT
//!   (and any other unsupported rename2 flag) must refuse with EINVAL —
//!   the pre-fix handler silently ignored unknown flags, so the kernel
//!   believed whiteouts were created (the golden expected `char/regu`
//!   whiteout rows; the mount produced nothing).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
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
    let ba = Arc::new(BlockAllocator::new("renamesem_test").await.unwrap());
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
            hash_seed: 0xC0FF_EE00_9ABC_DEF2,
            uuid: *b"rename-sem-v3!!!",
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

async fn backend_nlink(h: &H, ino: u64) -> u32 {
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .getattr(ino)
        .await
        .unwrap()
        .nlink
}

/// generic/035 directory leg: rename dir1 over dir2 ⇒ dir2's nlink is 0
/// (the fstat-on-open-fd observation `t_rename_overwrite` makes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_over_directory_zeroes_dest_nlink() {
    let h = make().await;
    let d1 =
        h.fs.mkdir(h.req, 1, OsStr::new("dir1"), 0o755, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let d2 =
        h.fs.mkdir(h.req, 1, OsStr::new("dir2"), 0o755, 0)
            .await
            .unwrap()
            .attr
            .ino;

    h.fs.rename(h.req, 1, OsStr::new("dir1"), 1, OsStr::new("dir2"))
        .await
        .expect("dir-over-dir rename");

    assert_eq!(
        backend_nlink(&h, d2).await,
        0,
        "the overwritten directory's nlink must be 0 (generic/035)"
    );
    // The surviving name resolves to the source dir.
    let got =
        h.fs.lookup(h.req, 1, OsStr::new("dir2"))
            .await
            .unwrap()
            .attr
            .ino;
    assert_eq!(got, d1);
}

/// generic/035 regular-file leg (already-correct guard): overwritten
/// file's nlink drops 1 → 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_over_file_zeroes_dest_nlink() {
    let h = make().await;
    let _f1 =
        h.fs.create(h.req, 1, OsStr::new("f1"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let f2 =
        h.fs.create(h.req, 1, OsStr::new("f2"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    h.fs.rename(h.req, 1, OsStr::new("f1"), 1, OsStr::new("f2"))
        .await
        .expect("file-over-file rename");
    assert_eq!(backend_nlink(&h, f2).await, 0);
}

/// Any unknown rename2 flag must refuse EINVAL — never a silent plain
/// rename. (RENAME_WHITEOUT graduated from this list to a real
/// implementation — fstests generic/631, the overlayfs-upper probe; its
/// pins are below. WHITEOUT|EXCHANGE stays refused: the VFS forbids the
/// combination and a defensive daemon refusal keeps it unrepresentable.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename2_refuses_unsupported_flags() {
    let h = make().await;
    h.fs.create(h.req, 1, OsStr::new("src"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();

    for flags in [
        libc::RENAME_WHITEOUT | libc::RENAME_EXCHANGE,
        1 << 5, // any future/unknown bit
    ] {
        let err =
            h.fs.rename2(h.req, 1, OsStr::new("src"), 1, OsStr::new("dst"), flags)
                .await
                .expect_err("unsupported rename2 flags must refuse");
        let io: std::io::Error = err.into();
        assert_eq!(
            io.raw_os_error(),
            Some(libc::EINVAL),
            "flags {flags:#x} must be EINVAL, and the rename must not happen"
        );
        // Lookup misses reply as NEGATIVE entries (ino 0, the D2 kernel
        // negative-dentry caching) — absent means Err OR the negative
        // reply, never a real ino.
        let absent = match h.fs.lookup(h.req, 1, OsStr::new("dst")).await {
            Err(_) => true,
            Ok(reply) => reply.attr.ino == 0,
        };
        assert!(
            absent,
            "no dentry may appear under the refused flags {flags:#x}"
        );
    }
    // The supported flags still work.
    h.fs.rename2(
        h.req,
        1,
        OsStr::new("src"),
        1,
        OsStr::new("dst"),
        libc::RENAME_NOREPLACE,
    )
    .await
    .expect("NOREPLACE into a free name");
}

/// fstests generic/631 (overlayfs-over-squeezefs, VL10 release gate):
/// RENAME_WHITEOUT is IMPLEMENTED — the kernel forwards flag 4 to every
/// FUSE fs (proven live: the daemon logs `flags = 4`; refusing it is a
/// DAEMON gap, not a kernel-interface one), and overlayfs refuses any
/// upper fs without it ("upper fs missing required features"). The
/// rename moves src → dst and leaves a fresh whiteout at the OLD name:
/// a char device 0:0 (the VFS whiteout contract — S_IFCHR, rdev 0),
/// minted atomically inside the same-volume rename transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_whiteout_moves_and_leaves_a_char_00_node() {
    let h = make().await;
    let src =
        h.fs.create(h.req, 1, OsStr::new("wsrc"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    h.fs.rename2(
        h.req,
        1,
        OsStr::new("wsrc"),
        1,
        OsStr::new("wdst"),
        libc::RENAME_WHITEOUT,
    )
    .await
    .expect("RENAME_WHITEOUT must succeed (overlayfs-upper requires it)");

    // The move happened.
    let moved =
        h.fs.lookup(h.req, 1, OsStr::new("wdst"))
            .await
            .unwrap()
            .attr;
    assert_eq!(moved.ino, src, "dst is the moved source inode");

    // The OLD name is a whiteout: char device 0:0.
    let wh =
        h.fs.lookup(h.req, 1, OsStr::new("wsrc"))
            .await
            .unwrap()
            .attr;
    assert_ne!(wh.ino, 0, "the whiteout must exist at the old name");
    assert_ne!(wh.ino, src, "the whiteout is a FRESH inode");
    assert_eq!(
        wh.kind,
        fuse3::FileType::CharDevice,
        "whiteout = char device (VFS contract)"
    );
    assert_eq!(wh.rdev, 0, "whiteout rdev = 0:0 (VFS contract)");
    assert_eq!(wh.nlink, 1);

    // The whiteout is a real unlinkable node (overlayfs cleans them up).
    h.fs.unlink(h.req, 1, OsStr::new("wsrc"))
        .await
        .expect("whiteout unlink");
}

/// WHITEOUT composes with NOREPLACE exactly like a plain rename:
/// an existing destination refuses EEXIST (and no whiteout appears);
/// an absent destination succeeds and leaves the whiteout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_whiteout_noreplace_composes() {
    let h = make().await;
    h.fs.create(h.req, 1, OsStr::new("a"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();
    h.fs.create(h.req, 1, OsStr::new("b"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();

    let err =
        h.fs.rename2(
            h.req,
            1,
            OsStr::new("a"),
            1,
            OsStr::new("b"),
            libc::RENAME_WHITEOUT | libc::RENAME_NOREPLACE,
        )
        .await
        .expect_err("existing destination must refuse under NOREPLACE");
    let io: std::io::Error = err.into();
    assert_eq!(io.raw_os_error(), Some(libc::EEXIST));
    let a = h.fs.lookup(h.req, 1, OsStr::new("a")).await.unwrap().attr;
    assert_eq!(
        a.kind,
        fuse3::FileType::RegularFile,
        "the refused rename must not leave a whiteout"
    );

    h.fs.rename2(
        h.req,
        1,
        OsStr::new("a"),
        1,
        OsStr::new("c"),
        libc::RENAME_WHITEOUT | libc::RENAME_NOREPLACE,
    )
    .await
    .expect("absent destination succeeds");
    let wh = h.fs.lookup(h.req, 1, OsStr::new("a")).await.unwrap().attr;
    assert_eq!(wh.kind, fuse3::FileType::CharDevice);
    assert_eq!(wh.rdev, 0);
}

/// WHITEOUT over an EXISTING destination (no NOREPLACE): the replace
/// happens AND the whiteout appears at the old name — the overlayfs
/// unlink-through-whiteout shape (its workdir whiteout is renamed over
/// the victim's upper entry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_whiteout_over_existing_destination() {
    let h = make().await;
    let src =
        h.fs.create(h.req, 1, OsStr::new("s2"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    h.fs.create(h.req, 1, OsStr::new("d2"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();

    h.fs.rename2(
        h.req,
        1,
        OsStr::new("s2"),
        1,
        OsStr::new("d2"),
        libc::RENAME_WHITEOUT,
    )
    .await
    .expect("whiteout rename over an existing destination");
    let d = h.fs.lookup(h.req, 1, OsStr::new("d2")).await.unwrap().attr;
    assert_eq!(d.ino, src);
    let wh = h.fs.lookup(h.req, 1, OsStr::new("s2")).await.unwrap().attr;
    assert_eq!(wh.kind, fuse3::FileType::CharDevice);
    assert_eq!(wh.rdev, 0);
}
