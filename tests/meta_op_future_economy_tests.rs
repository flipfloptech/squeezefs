//! The metadata handlers' FUTURE-STATE economy (symmetric PR 13f).
//!
//! Every FUSE handler future is `Box::pin`ned onto a `fuse3-tpc` lane and
//! moved once more at the lane handoff — two memmoves of the `async fn`'s
//! whole state per op. The box's gate-1 re-run on `77f4da1d`
//! (`.benchmarks/2026-09-19-sym-acceptance.md` §3.9.4.1) named that move
//! as the dominant term of the mdstorm `rename` / `unlink` DELTA: the
//! kernel sends a ctime SETATTR echo after every rename and unlink, and
//! the SETATTR handler's future had grown 18,960 → 26,016 B across PRs
//! 4–13b (the unlink handler's 7,616 → 11,088 B) — not at the routed
//! `setattr` entry, which is `async_trait`-boxed, but through the
//! TRUNCATE arm's `write_file_staged` / `truncate_layout` sub-futures
//! (and the unlink handler's overlay-drain arm), the CARRIERS: each
//! carries the data plane's layout publish, and every publish site grew
//! ≈ 3.5–4.6 KiB from ONE root — `KvMetaBackend::commit_tx` 176 →
//! 4,816 B, PR 4's door `ensure_leases_for_tx`, whose two first-touch
//! acquire arms (`manager_acquire_slots`, `joined_acquire_slot`) sat
//! inline in every commit's future (rustc `-Zprint-type-sizes` on both
//! trees; PR 13b's `publish_target` pair is ≈ 300–400 B of residue). The
//! truncate arm carries two publishes (+7 KiB), the drain one. The echo
//! never takes either arm, yet moved their state twice per op.
//!
//! The fix boxes those arms INSIDE their branches (and the door's two
//! acquires inside theirs), so the unarmed metadata-only future carries a
//! pointer to them, not them. This suite is the size INSTRUMENT
//! (`--nocapture` prints every future's `size_of_val`) and the BUDGET pin.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::{SetAttr, Timestamp};
use squeezefs::meta_backend::Metadata;
use std::ffi::OsStr;
use std::sync::Arc;

struct H {
    fs: squeezefs::fuse_client::SqueezefsFilesystem,
    routed: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    req: Request,
    _b: tempfile::NamedTempFile,
    _m: tempfile::NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make() -> H {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
    use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
    use squeezefs::meta_backend::RoutedMetaBackend;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    let dlm = DlmClient::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("meta_op_future_economy").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = tempfile::NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1313_0F0F,
            uuid: *b"meta-op-futecon1",
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        routed,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

/// The kernel's post-rename / post-unlink SETATTR echo: a ctime (and the
/// unchanged mtime), nothing else — the D4 "absorbed, not committed"
/// class, the shape that runs once per op in exactly the two DELTA phases.
fn times_echo() -> SetAttr {
    SetAttr {
        mode: None,
        uid: None,
        gid: None,
        size: None,
        lock_owner: None,
        atime: None,
        mtime: Some(Timestamp::new(1_700_000_000, 0)),
        ctime: Some(Timestamp::new(1_700_000_000, 0)),
        kill_suidgid: false,
    }
}

/// A truncate — the arm the fix boxed. Its future is the SAME type as the
/// echo's (one `async fn`), so the size is identical by construction; the
/// row is here so `--nocapture` states that.
fn truncate_to(size: u64) -> SetAttr {
    SetAttr {
        size: Some(size),
        mtime: None,
        ctime: None,
        ..times_echo()
    }
}

struct Sizes {
    fuse_setattr: usize,
    fuse_setattr_truncate: usize,
    fuse_unlink: usize,
    fuse_rename: usize,
    fuse_lookup: usize,
    fuse_getattr: usize,
    routed_getattr_local: usize,
    routed_setattr: usize,
    routed_getattr: usize,
    routed_lookup: usize,
    routed_unlink: usize,
    routed_rename: usize,
}

fn measure(h: &H) -> Sizes {
    let name = OsStr::new("f");
    let new_name = OsStr::new("g");
    Sizes {
        fuse_setattr: std::mem::size_of_val(&h.fs.setattr(h.req, 2, None, times_echo())),
        fuse_setattr_truncate: std::mem::size_of_val(&h.fs.setattr(
            h.req,
            2,
            None,
            truncate_to(4096),
        )),
        fuse_unlink: std::mem::size_of_val(&h.fs.unlink(h.req, 1, name)),
        fuse_rename: std::mem::size_of_val(&h.fs.rename(h.req, 1, name, 1, new_name)),
        fuse_lookup: std::mem::size_of_val(&h.fs.lookup(h.req, 1, name)),
        fuse_getattr: std::mem::size_of_val(&h.fs.getattr(h.req, 2, None, 0)),
        routed_getattr_local: std::mem::size_of_val(&h.routed.getattr_local(2)),
        routed_setattr: std::mem::size_of_val(&h.routed.setattr(
            2,
            None,
            None,
            None,
            None,
            None,
            Some(1),
            Some(1),
        )),
        routed_getattr: std::mem::size_of_val(&h.routed.getattr(2)),
        routed_lookup: std::mem::size_of_val(&h.routed.lookup(1, "f")),
        routed_unlink: std::mem::size_of_val(&h.routed.unlink(1, "f")),
        routed_rename: std::mem::size_of_val(&h.routed.rename(1, "f", 1, "g", 0)),
    }
}

fn print(s: &Sizes) {
    println!(
        "FUTURE SIZE fuse.setattr(times echo)  = {} B",
        s.fuse_setattr
    );
    println!(
        "FUTURE SIZE fuse.setattr(truncate)    = {} B",
        s.fuse_setattr_truncate
    );
    println!(
        "FUTURE SIZE fuse.unlink               = {} B",
        s.fuse_unlink
    );
    println!(
        "FUTURE SIZE fuse.rename               = {} B",
        s.fuse_rename
    );
    println!(
        "FUTURE SIZE fuse.lookup               = {} B",
        s.fuse_lookup
    );
    println!(
        "FUTURE SIZE fuse.getattr              = {} B",
        s.fuse_getattr
    );
    println!(
        "FUTURE SIZE routed.getattr_local      = {} B",
        s.routed_getattr_local
    );
    println!(
        "FUTURE SIZE routed.setattr (boxed)    = {} B",
        s.routed_setattr
    );
    println!(
        "FUTURE SIZE routed.getattr (boxed)    = {} B",
        s.routed_getattr
    );
    println!(
        "FUTURE SIZE routed.lookup (boxed)     = {} B",
        s.routed_lookup
    );
    println!(
        "FUTURE SIZE routed.unlink (boxed)     = {} B",
        s.routed_unlink
    );
    println!(
        "FUTURE SIZE routed.rename (boxed)     = {} B",
        s.routed_rename
    );
}

/// The instrument: print every size (run with `--nocapture`). The
/// `async_trait` routed verbs read 16 B here by construction (a boxed
/// pointer) — their heap sizes are the rustc `-Zprint-type-sizes` dump's,
/// recorded in the PR 13f note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_future_sizes_are_printed() {
    let h = make().await;
    print(&measure(&h));
}

// ---------------------------------------------------------------------------
// The budget pin.
// ---------------------------------------------------------------------------

/// **The SETATTR handler's unarmed future budget.** Measured 26,016 B on `77f4da1d` (18,960 B on the
/// pre-program `3228fcb8`); with the truncate arm and the overlay drain
/// boxed inside their branches it is **896 B** — the metadata-only
/// setattr's own state: the `async_trait` `getattr` / `setattr` boxes
/// (16 B each), the per-inode write guard's future, the inode record and
/// the attr publish's locals. The budget is 2× the fixed size: one more
/// handler-class await (a guard, a boxed verb, a small router probe) fits;
/// a data-plane arm (the smallest, `fetch_metadata`, is 2.6 KiB) does not.
const SETATTR_FUTURE_BUDGET: usize = 1_792;
/// **The UNLINK handler's unarmed future budget.** Measured 11,088 B on
/// `77f4da1d` (7,616 B on `3228fcb8`); with the overlay-drain arm boxed it
/// is **280 B** — the routed `unlink` box + two attr-cache refreshes. Same
/// 2× law as the setattr budget.
const UNLINK_FUTURE_BUDGET: usize = 576;
/// **The RENAME handler's future budget.** 392 B on both trees — it never
/// grew (its data-plane work is the routed box's); pinned at the same 2×
/// so it cannot start.
const RENAME_FUTURE_BUDGET: usize = 784;

/// The `Box::pin` + lane-handoff memmove per op is the future's SIZE; the
/// three handlers the mdstorm `rename` / `unlink` phases run per op (the
/// kernel's SETATTR echo included) must stay small on the unarmed path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unarmed_handler_futures_stay_within_budget() {
    let h = make().await;
    let s = measure(&h);
    print(&s);
    assert!(
        s.fuse_setattr <= SETATTR_FUTURE_BUDGET,
        "the SETATTR handler's future is {} B (budget {SETATTR_FUTURE_BUDGET}) — a data-plane \
         arm is inline again; box it inside its branch (PR 13f: the kernel runs this future \
         as a ctime echo after every rename / unlink)",
        s.fuse_setattr
    );
    assert_eq!(
        s.fuse_setattr, s.fuse_setattr_truncate,
        "one async fn, one size — the truncate shape cannot differ"
    );
    assert!(
        s.fuse_unlink <= UNLINK_FUTURE_BUDGET,
        "the UNLINK handler's future is {} B (budget {UNLINK_FUTURE_BUDGET}) — the overlay \
         drain (or another data-plane arm) is inline again; box it inside its branch",
        s.fuse_unlink
    );
    assert!(
        s.fuse_rename <= RENAME_FUTURE_BUDGET,
        "the RENAME handler's future is {} B (budget {RENAME_FUTURE_BUDGET}) — it carried no \
         data-plane arm before; keep it that way",
        s.fuse_rename
    );
}
