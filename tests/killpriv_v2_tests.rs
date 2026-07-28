//! FUSE_HANDLE_KILLPRIV_V2 daemon-side clearing law (the 2026-07-28
//! killpriv campaign — `.benchmarks/2026-07-27-oq1-overwrite-op-economy.md`
//! §4/§5): with the capability negotiated the kernel stops its per-write(2)
//! `GETXATTR("security.capability")` killpriv probe and instead flags the
//! daemon (`FUSE_WRITE_KILL_SUIDGID` on WRITE, `FUSE_OPEN_KILL_SUIDGID` on
//! O_TRUNC OPEN, `SetAttr::kill_suidgid` on size-changing SETATTR/chown).
//! This is a SECURITY-SEMANTICS TRANSFER, and the law is pinned here:
//!
//! - flagged ⇒ clear S_ISUID **always**;
//! - flagged ⇒ clear S_ISGID **only when the file is group-executable**
//!   (S_IXGRP) — sgid without group-exec is the mandatory-locking marker
//!   and MUST survive (the classic killpriv trap);
//! - flagged ⇒ drop the `security.capability` xattr;
//! - unflagged ⇒ clear NOTHING (root / CAP_FSETID writers — the kernel
//!   never flags them, and the daemon must not invent the kill);
//! - the no-priv-bits common case costs at most a cached-mode check:
//!   **zero metadata traffic** (journal-entry-delta equality with an
//!   unflagged write — the D4 journal-entry economy);
//! - a SETATTR-carried kill FOLDS into that op's one existing commit
//!   (no second journal entry).
//!
//! Counter deltas are exact under the sanctioned serial gate
//! (`--test-threads=1`).

use fuse3::raw::flags::{FUSE_OPEN_KILL_SUIDGID, FUSE_WRITE_KILL_SUIDGID};
use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::SetAttr;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
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
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "killpriv_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
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
            hash_seed: 0xC0FF_EE00_9ABC_DEF0,
            uuid: *b"killpriv-v2-test",
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

/// Create a regular file under root with the given permission bits
/// (applied through the SETATTR handler like a real chmod, so the
/// attr cache carries the priv'd mode too). Returns the ino; the create
/// handle stays open (a live writer).
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

async fn write_bytes(h: &H, ino: u64, write_flags: u32) {
    h.fs.write(
        h.req,
        ino,
        ino,
        0,
        bytes::Bytes::from_static(b"killpriv payload"),
        write_flags,
        0,
    )
    .await
    .unwrap();
}

fn journal_entries() -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

fn clears() -> u64 {
    METRICS.fuse_killpriv_clears.load(Ordering::Relaxed)
}

/// suid is cleared on a flagged WRITE — always, exec bits irrelevant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flagged_write_clears_suid() {
    let h = make().await;
    let ino = create_with_mode(&h, "suid", 0o4755).await;
    let c0 = clears();

    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;

    assert_eq!(
        backend_mode(&h, ino).await,
        0o755,
        "S_ISUID must be cleared by a FUSE_WRITE_KILL_SUIDGID write"
    );
    assert!(
        clears() > c0,
        "fuse_killpriv_clears must count the performed clear"
    );
}

/// THE classic trap: sgid WITHOUT group-exec is the mandatory-locking
/// marker (VFS: `should_remove_suid` kills sgid only with S_IXGRP) —
/// a flagged write must PRESERVE it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flagged_write_preserves_sgid_without_group_exec() {
    let h = make().await;
    let ino = create_with_mode(&h, "sgid_noexec", 0o2644).await;

    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;

    assert_eq!(
        backend_mode(&h, ino).await,
        0o2644,
        "sgid-without-group-exec (mandatory-locking marker) must SURVIVE a \
         flagged write — clearing it is the classic killpriv-v2 bug"
    );
}

/// sgid WITH group-exec is a real setgid executable — flagged write kills
/// it (and suid alongside when both are set).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flagged_write_clears_sgid_with_group_exec() {
    let h = make().await;
    let ino = create_with_mode(&h, "sgid_exec", 0o2755).await;
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;
    assert_eq!(
        backend_mode(&h, ino).await,
        0o755,
        "sgid-with-group-exec must be cleared by a flagged write"
    );

    let both = create_with_mode(&h, "both_bits", 0o6775).await;
    write_bytes(&h, both, FUSE_WRITE_KILL_SUIDGID).await;
    assert_eq!(
        backend_mode(&h, both).await,
        0o775,
        "suid AND group-exec sgid must both clear on a flagged write"
    );
}

/// The capability xattr face: a flagged write drops security.capability
/// (the daemon owns what the kernel's killpriv GETXATTR probe used to
/// arbitrate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flagged_write_drops_security_capability_xattr() {
    let h = make().await;
    let ino = create_with_mode(&h, "caps", 0o755).await;
    let caps = [0u8, 0, 0, 2, 0x10, 0, 0, 0, 0, 0, 0, 0];
    h.fs.setxattr(h.req, ino, OsStr::new("security.capability"), &caps, 0, 0)
        .await
        .unwrap();
    let c0 = clears();

    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;

    let after =
        h.fs.meta_backend
            .as_ref()
            .unwrap()
            .getxattr(ino, "security.capability")
            .await
            .unwrap();
    assert!(
        after.is_none(),
        "security.capability must be dropped by a flagged write"
    );
    assert!(clears() > c0, "the caps drop is a counted clear");
}

/// Unflagged writes clear NOTHING: the kernel only omits the flag for
/// CAP_FSETID writers (root) — the daemon must not invent the kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unflagged_write_clears_nothing() {
    let h = make().await;
    let ino = create_with_mode(&h, "root_writer", 0o6777).await;
    let caps = [0u8, 0, 0, 2];
    h.fs.setxattr(h.req, ino, OsStr::new("security.capability"), &caps, 0, 0)
        .await
        .unwrap();
    let c0 = clears();

    write_bytes(&h, ino, 0).await;

    assert_eq!(
        backend_mode(&h, ino).await,
        0o6777,
        "an unflagged write (CAP_FSETID writer) must preserve suid+sgid"
    );
    let caps_after =
        h.fs.meta_backend
            .as_ref()
            .unwrap()
            .getxattr(ino, "security.capability")
            .await
            .unwrap();
    assert!(
        caps_after.is_some(),
        "an unflagged write must preserve security.capability"
    );
    assert_eq!(clears(), c0, "no clear may be counted for unflagged writes");
}

/// O_TRUNC open with FUSE_OPEN_KILL_SUIDGID clears (kernel sends it under
/// ATOMIC_O_TRUNC instead of a SETATTR); without the bit (CAP_FSETID
/// opener) the truncate must preserve the bits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otrunc_open_kill_flag_clears_and_absence_preserves() {
    let h = make().await;
    let ino = create_with_mode(&h, "otrunc_kill", 0o4711).await;
    write_bytes(&h, ino, 0).await;

    h.fs.open(
        h.req,
        ino,
        (libc::O_WRONLY | libc::O_TRUNC) as u32,
        FUSE_OPEN_KILL_SUIDGID,
    )
    .await
    .unwrap();
    assert_eq!(
        backend_mode(&h, ino).await,
        0o711,
        "FUSE_OPEN_KILL_SUIDGID on an O_TRUNC open must clear suid"
    );

    // The passthrough face: kill flag absent (root / CAP_FSETID opener).
    let keep = create_with_mode(&h, "otrunc_keep", 0o4711).await;
    write_bytes(&h, keep, 0).await;
    h.fs.open(h.req, keep, (libc::O_WRONLY | libc::O_TRUNC) as u32, 0)
        .await
        .unwrap();
    assert_eq!(
        backend_mode(&h, keep).await,
        0o4711,
        "an O_TRUNC open WITHOUT the kill flag must preserve suid"
    );
    // sgid-no-group-exec preserved through the kill path too.
    let marker = create_with_mode(&h, "otrunc_marker", 0o2600).await;
    write_bytes(&h, marker, 0).await;
    h.fs.open(
        h.req,
        marker,
        (libc::O_WRONLY | libc::O_TRUNC) as u32,
        FUSE_OPEN_KILL_SUIDGID,
    )
    .await
    .unwrap();
    assert_eq!(
        backend_mode(&h, marker).await,
        0o2600,
        "sgid-without-group-exec survives even a killing O_TRUNC open"
    );
}

/// Size-changing SETATTR carrying kill_suidgid clears under the same law;
/// without the bit it preserves. The clear FOLDS into the op's one
/// existing commit — journal-entry-delta equality with the same setattr
/// on an identically-shaped file without the kill (the D4 economy: never
/// a second journal entry per op).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_kill_suidgid_clears_folded_into_one_commit() {
    let h = make().await;
    let killed = create_with_mode(&h, "trunc_kill", 0o4755).await;
    let kept = create_with_mode(&h, "trunc_keep", 0o4755).await;
    write_bytes(&h, killed, 0).await;
    write_bytes(&h, kept, 0).await;

    let j0 = journal_entries();
    h.fs.setattr(
        h.req,
        killed,
        None,
        SetAttr {
            size: Some(0),
            kill_suidgid: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let kill_delta = journal_entries() - j0;

    let j1 = journal_entries();
    h.fs.setattr(
        h.req,
        kept,
        None,
        SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let plain_delta = journal_entries() - j1;

    assert_eq!(
        backend_mode(&h, killed).await,
        0o755,
        "FATTR_KILL_SUIDGID truncate must clear suid"
    );
    assert_eq!(
        backend_mode(&h, kept).await,
        0o4755,
        "a truncate without the kill bit must preserve suid"
    );
    assert_eq!(
        kill_delta, plain_delta,
        "the mode clear must FOLD into the setattr's one commit — a second \
         journal entry per killing truncate violates the D4 economy \
         (kill {kill_delta} vs plain {plain_delta})"
    );
}

/// The hot-path economy pin: on a file with NO priv bits and NO caps
/// xattr (the overwhelmingly common case) a flagged write must cost at
/// most a cached-mode check — ZERO metadata traffic, byte-identical
/// journal deltas with an unflagged write, and no counted clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_priv_bits_flagged_write_is_zero_metadata_traffic() {
    let h = make().await;
    let ino = create_with_mode(&h, "plain", 0o644).await;

    // Prime: the first flagged write may pay a one-time probe to learn
    // the ino is clean (RAM-authoritative reads; still no journal
    // growth beyond the write's own commit — asserted below by symmetry
    // of the steady-state pair).
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;

    let c0 = clears();
    let j0 = journal_entries();
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;
    let flagged_delta = journal_entries() - j0;

    let j1 = journal_entries();
    write_bytes(&h, ino, 0).await;
    let plain_delta = journal_entries() - j1;

    assert_eq!(
        flagged_delta, plain_delta,
        "a flagged write on a no-priv-bits file must add ZERO metadata \
         traffic over an unflagged write (flagged {flagged_delta} vs \
         plain {plain_delta})"
    );
    assert_eq!(
        clears(),
        c0,
        "no clear may be counted when there is nothing to clear"
    );
    assert_eq!(backend_mode(&h, ino).await, 0o644, "the mode is untouched");
}

/// Latch hygiene: once an ino is known-clean, a chmod that RE-ADDS priv
/// bits must re-arm the clearing path (the killpriv-clean latch cannot
/// go stale), and a fresh security.capability set must re-arm the caps
/// drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_latch_rearms_on_mode_and_caps_mutation() {
    let h = make().await;
    let ino = create_with_mode(&h, "rearm", 0o644).await;
    // Learn clean.
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;

    // Re-add suid via the SETATTR handler (a real chmod).
    h.fs.setattr(
        h.req,
        ino,
        None,
        SetAttr {
            mode: Some(0o4755),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;
    assert_eq!(
        backend_mode(&h, ino).await,
        0o755,
        "a chmod re-adding suid must re-arm the flagged-write clear"
    );

    // Re-add caps via the setxattr handler.
    h.fs.setxattr(
        h.req,
        ino,
        OsStr::new("security.capability"),
        &[0u8, 0, 0, 2],
        0,
        0,
    )
    .await
    .unwrap();
    write_bytes(&h, ino, FUSE_WRITE_KILL_SUIDGID).await;
    let caps_after =
        h.fs.meta_backend
            .as_ref()
            .unwrap()
            .getxattr(ino, "security.capability")
            .await
            .unwrap();
    assert!(
        caps_after.is_none(),
        "a fresh security.capability set must re-arm the flagged-write drop"
    );
}
