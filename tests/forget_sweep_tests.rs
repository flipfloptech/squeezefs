//! RES-13 (pre-RC engineering spec §7): the per-inode side maps that
//! `forget` / `batch_forget` never swept.
//!
//! `dir_gen` (one `AtomicU64` per directory ever mutated) and
//! `killpriv_clean` (one word per ino ever priv-checked) both grow with
//! the set of inodes a mount has TOUCHED, not with the set it currently
//! holds — and the kernel's own "I no longer reference this inode"
//! signal, FORGET, went straight past them. Every other per-inode side
//! structure is swept there (`attr_cache`, `active_inode_locks`, the
//! reclaim enqueue), so the two are pure omissions rather than a design
//! choice: after a `drop_caches` sweep the kernel holds nothing and the
//! daemon still holds an entry per inode it ever saw.
//!
//! Contracts pinned here:
//!
//! 1. **FORGET sweeps both maps** for the forgotten inode.
//! 2. **BATCH_FORGET behaves exactly like N FORGETs** (the standing law
//!    for that handler — it is the drop_caches / memory-pressure path,
//!    i.e. exactly when the leak matters).
//! 3. **The sweep is not a semantic change**: a directory whose
//!    generation entry was dropped reads generation 0 and the next
//!    mutation bumps it, so a readdir snapshot taken before the FORGET
//!    can never match — the fail-safe direction.
//!
//! RED against dev 7d1ec2e1: both maps keep their entries across FORGET
//! and BATCH_FORGET.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn harness(tag: &str) -> H {
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let alloc = Arc::new(BlockAllocator::new(tag).await.expect("allocator"));
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    H {
        fs,
        req: Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
            ..Default::default()
        },
        _b: b,
        _m: m,
        _s: s,
    }
}

/// Create a directory (touches `dir_gen` for it AND for the root) and a
/// file inside it whose write marks it `killpriv_clean`.
async fn touched_pair(h: &H, dir_name: &str, file_name: &str) -> (u64, u64) {
    let dir =
        h.fs.mkdir(h.req, 1, OsStr::new(dir_name), libc::S_IFDIR | 0o755, 0)
            .await
            .expect("mkdir")
            .attr
            .ino;
    let file =
        h.fs.create(h.req, dir, OsStr::new(file_name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino;
    // A FUSE_WRITE_KILL_SUIDGID write is what latches killpriv_clean
    // (the D4 economy pin: membership short-circuits the priv probe).
    h.fs.write(
        h.req,
        file,
        0,
        0,
        bytes::Bytes::from_static(b"hi"),
        fuse3::raw::flags::FUSE_WRITE_KILL_SUIDGID,
        0,
    )
    .await
    .expect("write");
    (dir, file)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_sweeps_dir_gen_and_killpriv_clean() {
    let h = harness("res13_forget").await;
    let (dir, file) = touched_pair(&h, "d1", "f1").await;

    assert!(
        h.fs.dir_generation(dir) > 0,
        "fixture premise: the directory has a generation entry"
    );
    assert!(
        h.fs.killpriv_clean_holds(file),
        "fixture premise: the written file is latched killpriv-clean"
    );

    h.fs.forget(h.req, file, 1).await;
    h.fs.forget(h.req, dir, 1).await;

    assert!(
        !h.fs.killpriv_clean_holds(file),
        "RES-13: FORGET must sweep killpriv_clean — the map grows with every \
         ino the mount ever priv-checked, and FORGET is the kernel telling us \
         it holds no reference"
    );
    assert!(
        !h.fs.dir_gen_holds(dir),
        "RES-13: FORGET must sweep dir_gen — one AtomicU64 per directory ever \
         mutated, never reclaimed"
    );
    // Contract 3: dropping the entry is fail-safe, not a semantic change.
    assert_eq!(
        h.fs.dir_generation(dir),
        0,
        "a swept directory reads generation 0; any snapshot built under a \
         nonzero generation can never match again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_forget_sweeps_exactly_like_n_forgets() {
    let h = harness("res13_batch").await;
    let (d1, f1) = touched_pair(&h, "d1", "f1").await;
    let (d2, f2) = touched_pair(&h, "d2", "f2").await;

    // FUSE-3k: each entry carries the count the kernel is returning; the
    // fixture's `create` took exactly one lookup reference per ino.
    h.fs.batch_forget(h.req, &[(f1, 1), (f2, 1), (d1, 1), (d2, 1)])
        .await;

    for ino in [f1, f2] {
        assert!(
            !h.fs.killpriv_clean_holds(ino),
            "RES-13: BATCH_FORGET must behave exactly like N FORGETs — it IS \
             the drop_caches / memory-pressure path, i.e. exactly when the \
             leak matters (ino {ino} still latched)"
        );
    }
    for ino in [d1, d2] {
        assert!(
            !h.fs.dir_gen_holds(ino),
            "RES-13: BATCH_FORGET must sweep dir_gen too (ino {ino} retained)"
        );
    }
}
