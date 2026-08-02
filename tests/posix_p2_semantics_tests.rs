//! Pre-RC POSIX-semantics contracts, spec §5 **POSIX-9 / POSIX-10 /
//! POSIX-13 / POSIX-16** — the P2 band that changes what userspace
//! observes on ordinary directory walks, `copy_file_range`,
//! `posix_fallocate`, and `close`.
//!
//! Every test drives the real [`SqueezefsFilesystem`] handlers in-process
//! against a real v3 KV metadata volume and a real block device file
//! (the `posix_semantics_tests` harness shape) — deterministic, no TTL
//! sleeps, no kernel dcache in the way.
//!
//! * **POSIX-9** — `readdirplus` DROPPED any entry whose `getattr`
//!   failed, so `readdir` and `readdirplus` disagreed about what is in a
//!   directory: `rm -rf` completes its readdir, deletes what it saw, and
//!   then fails `rmdir` with ENOTEMPTY on the entry readdirplus never
//!   mentioned. An entry that exists must be REPORTED (minimal attrs,
//!   zero TTLs — the kernel re-looks-it-up immediately).
//! * **POSIX-10** — `copy_file_range` never updated the destination's
//!   mtime (a copy that leaves the destination's timestamp untouched
//!   defeats every `make`-class staleness check) and discarded the
//!   size-commit error.
//! * **POSIX-13** — `fallocate`'s extend path took a bare fencing-token
//!   SNAPSHOT (`get_fencing_token_ino`) where every sibling mutator
//!   holds the shared lease. `setattr` was already moved off that
//!   pattern; the punch/zero arm always used the lease.
//! * **POSIX-16** — close-time writeback errors were never reported:
//!   `flush` discarded its flush result and `release` discarded three
//!   more, with no errseq-equivalent latch, so a failed background
//!   writeback surfaced to the application NOWHERE. A per-inode latch is
//!   consumed once by the next `fsync`/`flush` on any fd.

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
use std::collections::HashSet;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

use futures::StreamExt;

const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    be: Arc<RoutedMetaBackend>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
            hash_seed: 0x9A5D_0000_0000_0002,
            uuid: *b"posix-p2-rc-vol!",
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
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
    };
    H {
        fs,
        req,
        be: routed,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create_in(h: &H, parent: u64, name: &str) -> u64 {
    let created =
        h.fs.create(
            h.req,
            parent,
            OsStr::new(name),
            libc::S_IFREG | 0o644,
            libc::O_RDWR as u32,
        )
        .await
        .unwrap();
    let ino = created.attr.ino;
    h.fs.release(h.req, ino, created.fh, 0, 0, false)
        .await
        .unwrap();
    ino
}

/// Leave a DENTRY whose inode record is gone — the shape a torn or
/// partially-completed reclaim leaves behind, and the only way a
/// `getattr` on a listed child legitimately fails. `destroy_inodes`
/// refuses a linked inode, so the nlink is dropped first through the
/// backend's own adjustment path (the dentry is untouched by both).
async fn orphan_the_dentry(h: &H, ino: u64) {
    let guards: std::sync::Arc<[squeezefs::meta_backend::dlm::DlmGuard]> =
        std::sync::Arc::from(Vec::new());
    h.be.volumes[0]
        .routed_nlink_adjust(ino, -1, false, guards)
        .await
        .expect("drop the link count");
    h.be.destroy_inodes(&[ino]).await.expect("destroy inode");
    h.fs.attr_cache.invalidate(&ino);
}

async fn readdir_names(h: &H, dir: u64) -> HashSet<String> {
    let reply = h.fs.readdir(h.req, dir, 0, 0).await.expect("readdir");
    reply
        .entries
        .filter_map(|e| async move { e.ok().map(|e| e.name.to_string_lossy().into_owned()) })
        .collect()
        .await
}

async fn readdirplus_names(h: &H, dir: u64) -> HashSet<String> {
    let reply =
        h.fs.readdirplus(h.req, dir, 0, 0, 0)
            .await
            .expect("readdirplus");
    reply
        .entries
        .filter_map(|e| async move { e.ok().map(|e| e.name.to_string_lossy().into_owned()) })
        .collect()
        .await
}

// ---------------------------------------------------------------------------
// POSIX-9 — readdir and readdirplus must name the same directory.
// ---------------------------------------------------------------------------

/// The `rm -rf` trap: an entry whose inode record cannot be read is
/// still an ENTRY. Dropping it makes readdirplus disagree with readdir,
/// so a recursive delete removes what it saw and then trips ENOTEMPTY on
/// what it never heard about. Destroying the child's inode record while
/// its dentry lives is exactly the shape a torn/partial reclaim leaves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readdirplus_reports_entries_whose_getattr_fails() {
    let h = make("posix9").await;
    let dir =
        h.fs.mkdir(h.req, 1, OsStr::new("d9"), 0o755, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let good = create_in(&h, dir, "good.txt").await;
    let broken = create_in(&h, dir, "broken.txt").await;

    // Destroy the child's INODE record, leaving its dentry in place.
    orphan_the_dentry(&h, broken).await;

    let plain = readdir_names(&h, dir).await;
    let plus = readdirplus_names(&h, dir).await;
    assert!(
        plain.contains("broken.txt"),
        "readdir names the entry: {plain:?}"
    );
    assert_eq!(
        plain, plus,
        "POSIX-9: readdirplus must name exactly what readdir names — a \
         dropped entry is the `rm -rf` ENOTEMPTY trap"
    );
    assert!(
        plus.contains("good.txt"),
        "the healthy sibling is still listed"
    );
    let _ = good;
}

/// The entry a failed `getattr` produces must carry ZERO ttls: the
/// kernel must not cache the placeholder attrs, and must re-`LOOKUP`
/// the name before anything trusts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_readdirplus_entry_is_never_cacheable() {
    let h = make("posix9b").await;
    let dir =
        h.fs.mkdir(h.req, 1, OsStr::new("d9b"), 0o755, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let broken = create_in(&h, dir, "gone.txt").await;
    orphan_the_dentry(&h, broken).await;
    assert!(
        h.be.getattr(broken).await.is_err(),
        "the harness must actually break the child's getattr"
    );

    let reply =
        h.fs.readdirplus(h.req, dir, 0, 0, 0)
            .await
            .expect("readdirplus");
    let entries: Vec<_> = reply
        .entries
        .filter_map(|e| async move { e.ok() })
        .collect()
        .await;
    let placeholder = entries
        .iter()
        .find(|e| e.name.to_string_lossy() == "gone.txt")
        .expect("the entry must be reported");
    assert_eq!(
        placeholder.entry_ttl,
        std::time::Duration::ZERO,
        "a placeholder entry must not be cached"
    );
    assert_eq!(
        placeholder.attr_ttl,
        std::time::Duration::ZERO,
        "placeholder attrs must not be cached"
    );
}

// ---------------------------------------------------------------------------
// POSIX-10 — copy_file_range writes to the destination, so it modifies it.
// ---------------------------------------------------------------------------

/// A copy that leaves the destination's mtime untouched breaks every
/// staleness check built on timestamps (`make`, rsync's quick check,
/// backup scanners).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_file_range_updates_the_destination_mtime() {
    let h = make("posix10").await;
    let src = create_in(&h, 1, "cfr-src").await;
    let dst = create_in(&h, 1, "cfr-dst").await;

    let data = bytes::Bytes::from(vec![0xA5u8; 4096]);
    h.fs.write(h.req, src, 0, 0, data, 0, 0)
        .await
        .expect("seed the source");

    let before = h.fs.getattr(h.req, dst, None, 0).await.unwrap().attr.mtime;
    // A distinguishable clock tick: timestamps are coarse (ns from
    // CLOCK_REALTIME_COARSE), so stamp the destination in the past
    // rather than sleeping for a tick.
    h.be.setattr(dst, None, None, None, None, None, Some(1_000_000_000), None)
        .await
        .expect("age the destination");
    h.fs.attr_cache.invalidate(&dst);

    let copied =
        h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, 4096, 0)
            .await
            .expect("copy_file_range")
            .copied;
    assert_eq!(copied, 4096);

    h.fs.attr_cache.invalidate(&dst);
    let after = h.fs.getattr(h.req, dst, None, 0).await.unwrap().attr.mtime;
    assert!(
        after.sec > 1,
        "POSIX-10: copy_file_range must stamp the destination's mtime \
         (before {before:?}, aged to 1 s, after {after:?})"
    );
}

// ---------------------------------------------------------------------------
// POSIX-16 — a writeback error must reach the application exactly once.
// ---------------------------------------------------------------------------

/// The errseq law, per inode: an error latched by a background flush is
/// reported by the NEXT `fsync` (or `flush`) on any fd, and consumed —
/// a second `fsync` with nothing new to report succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_latched_writeback_error_is_reported_once_by_fsync() {
    let h = make("posix16").await;
    let ino = create_in(&h, 1, "wb-err").await;

    h.fs.note_writeback_error(ino, libc::EIO);
    let err =
        h.fs.fsync(h.req, ino, 0, false)
            .await
            .expect_err("the latched error must surface");
    assert_eq!(libc::c_int::from(err), -libc::EIO);

    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("the error is consumed — a second fsync is clean");
}

/// `flush` (close) reports it too — the close-time report is the whole
/// point — and the CLEAN-HANDLE fast path must not swallow it: that
/// path exists to skip work, not to drop errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_latched_writeback_error_survives_the_clean_handle_flush_fastpath() {
    let h = make("posix16b").await;
    let ino = create_in(&h, 1, "wb-err-2").await;

    // A never-dirtied handle: `flush` normally short-circuits ENOSYS.
    h.fs.note_writeback_error(ino, libc::ENOSPC);
    let err =
        h.fs.flush(h.req, ino, 0, 0)
            .await
            .expect_err("a latched error outranks the clean-handle elision");
    assert_eq!(libc::c_int::from(err), -libc::ENOSPC);

    // Consumed: the next flush takes the normal fast path again.
    let after = h.fs.flush(h.req, ino, 0, 0).await;
    assert!(
        matches!(after, Err(e) if libc::c_int::from(e) == -libc::ENOSYS) || after.is_ok(),
        "after consumption the handle flushes normally, got {after:?}"
    );
}

/// The latch is per inode: one file's failure never fails another's
/// `fsync`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_writeback_latch_is_per_inode() {
    let h = make("posix16c").await;
    let a = create_in(&h, 1, "wb-a").await;
    let b = create_in(&h, 1, "wb-b").await;

    h.fs.note_writeback_error(a, libc::EIO);
    h.fs.fsync(h.req, b, 0, false)
        .await
        .expect("an unrelated inode is unaffected");
    assert_eq!(
        libc::c_int::from(h.fs.fsync(h.req, a, 0, false).await.unwrap_err()),
        -libc::EIO
    );
}

// ---------------------------------------------------------------------------
// POSIX-18 — PATH_MAX counts the NUL.
// ---------------------------------------------------------------------------

/// A 4096-byte symlink target cannot round-trip through any `PATH_MAX`
/// buffer: `readlink` into `char buf[PATH_MAX]` truncates it and
/// resolving it is ENAMETOOLONG. 4095 is the longest usable target, so
/// 4096 must be refused at creation (ext4/xfs do), not stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_targets_stop_one_byte_below_path_max() {
    let h = make("posix18").await;

    let longest = "a".repeat(4095);
    h.fs.symlink(h.req, 1, OsStr::new("p18-ok"), OsStr::new(longest.as_str()))
        .await
        .expect("4095 bytes is the longest legal target");

    let too_long = "a".repeat(4096);
    let err =
        h.fs.symlink(
            h.req,
            1,
            OsStr::new("p18-toolong"),
            OsStr::new(too_long.as_str()),
        )
        .await
        .expect_err("4096 bytes leaves no room for the NUL");
    assert_eq!(libc::c_int::from(err), -libc::ENAMETOOLONG);
}
