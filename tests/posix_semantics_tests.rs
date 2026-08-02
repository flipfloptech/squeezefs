//! Pre-RC POSIX-semantics contracts (spec §5 items POSIX-1..4) — the
//! user-visible correctness class that breaks `df`, `cp --sparse`,
//! `tar -S`, `rsync -S`, `ls`, `find`, and `du` on a public RC.
//!
//! Every test drives the real [`SqueezefsFilesystem`] handlers in-process
//! against a real v3 KV metadata volume and a real block device file, so
//! the contracts are pinned at the FUSE-op boundary without needing a
//! mount (deterministic — no TTL sleeps, no kernel dcache in the way).
//!
//! * **POSIX-1** — `statfs.f_ffree` must derive from a LIVE inode gauge:
//!   a create/delete loop must return `IUsed` to its baseline. It derived
//!   from the §4.8 monotonic ino watermark, which only ever rises, so a
//!   long-lived create/delete workload reported a full filesystem on an
//!   empty one and tools gating on `IUse%` refused to write.
//! * **POSIX-3** — `lstat()` on a symlink must report `st_size ==
//!   strlen(target)` from the DURABLE inode, not just inside the daemon's
//!   attr-cache window: `symlink()` patched the size into the reply and
//!   the cache only, so tools that size a `readlink()` buffer from
//!   `st_size` recorded empty targets once the TTL lapsed.
//! * **POSIX-2** — sparse files must be visible: `lseek(SEEK_DATA)` /
//!   `lseek(SEEK_HOLE)` from the block map, and `st_blocks` from actual
//!   allocation rather than synthesized from size.
//! * **POSIX-4** — `readdir`'s `..` must be correct AND must not pay an
//!   unindexed full dentry-tree scan per directory.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB block size so one harness covers all three layouts:
/// inline (<= 4 KiB), staged (4 KiB .. 64 KiB), striped (> 64 KiB).
const BS: u64 = 65536;
const GIB: u64 = 1024 * 1024 * 1024;
const INODE_QUOTA: u64 = 1_000_000;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "posix_sem_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
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
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid: *b"posix-sem-v3-rc!",
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
    // statfs reads both quota cells (set at FUSE init from the format
    // config on a real mount).
    let _ = fs.inodes_limit.set(INODE_QUOTA);
    let _ = fs.capacity_limit.set(8 * GIB);

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

/// CREATE + RELEASE (the `open(O_CREAT)`/`close` pair): reclaim refuses
/// to destroy an ino with a live open handle, so tests that delete must
/// close first, exactly like userspace.
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

async fn mkdir_in(h: &H, parent: u64, name: &str) -> u64 {
    h.fs.mkdir(h.req, parent, OsStr::new(name), 0o755, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// `f_files - f_ffree` — the `IUsed` column `df -i` prints.
async fn iused(h: &H) -> u64 {
    let st = h.fs.statfs(h.req, 1).await.unwrap();
    assert_eq!(st.files, INODE_QUOTA, "f_files must be the format quota");
    st.files - st.ffree
}

// ---------------------------------------------------------------------------
// POSIX-1 — statfs IUsed must recover on delete (live inode gauge).
// ---------------------------------------------------------------------------

/// The headline repro: N creates followed by N deletes must return
/// `IUsed` to the baseline. Before the live gauge, `f_ffree` derived from
/// the monotonic ino watermark, so this asserted-forever-rising number
/// made `df -i` report a full filesystem on an empty one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_iused_returns_to_baseline_after_create_delete_loop() {
    let h = make().await;
    let base = iused(&h).await;

    const N: usize = 48;
    let mut inos = Vec::with_capacity(N);
    for i in 0..N {
        inos.push(create_in(&h, 1, &format!("posix1_{i}")).await);
    }
    let peak = iused(&h).await;
    assert_eq!(
        peak,
        base + N as u64,
        "IUsed must account for the {N} live files (base {base}, peak {peak})"
    );

    for i in 0..N {
        h.fs.unlink(h.req, 1, OsStr::new(&format!("posix1_{i}")))
            .await
            .unwrap();
    }
    h.fs.reclaim_orphaned_batch(inos).await;

    let after = iused(&h).await;
    assert_eq!(
        after, base,
        "IUsed must return to the baseline after the deletes are reclaimed \
         (base {base}, peak {peak}, after {after}) — a monotonic-watermark \
         derivation reports a FULL filesystem on an empty one"
    );
}

/// Directories are inodes too, and `rmdir` must give the slot back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_iused_recovers_for_directories_too() {
    let h = make().await;
    let base = iused(&h).await;

    const N: usize = 16;
    let mut inos = Vec::with_capacity(N);
    for i in 0..N {
        inos.push(mkdir_in(&h, 1, &format!("posix1_dir_{i}")).await);
    }
    assert_eq!(iused(&h).await, base + N as u64);

    for i in 0..N {
        h.fs.rmdir(h.req, 1, OsStr::new(&format!("posix1_dir_{i}")))
            .await
            .unwrap();
    }
    h.fs.reclaim_orphaned_batch(inos).await;

    assert_eq!(
        iused(&h).await,
        base,
        "IUsed must recover after rmdir + reclaim"
    );
}

/// An unlinked-but-still-referenced inode is NOT free: the gauge must
/// only drop when the destroy transaction actually removes the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_iused_counts_hardlinked_and_open_inodes_as_live() {
    let h = make().await;
    let base = iused(&h).await;

    let ino = create_in(&h, 1, "posix1_linked").await;
    h.fs.link(h.req, ino, 1, OsStr::new("posix1_linked2"))
        .await
        .unwrap();
    assert_eq!(iused(&h).await, base + 1);

    // One name gone, one remains: still exactly one live inode.
    h.fs.unlink(h.req, 1, OsStr::new("posix1_linked"))
        .await
        .unwrap();
    h.fs.reclaim_orphaned_batch(vec![ino]).await;
    assert_eq!(
        iused(&h).await,
        base + 1,
        "an inode with a surviving link must stay counted"
    );

    h.fs.unlink(h.req, 1, OsStr::new("posix1_linked2"))
        .await
        .unwrap();
    h.fs.reclaim_orphaned_batch(vec![ino]).await;
    assert_eq!(
        iused(&h).await,
        base,
        "the last link's reclaim must free the slot"
    );
    // Silence the unused-import lint until the POSIX-4 legs land.
    let _ = METRICS.fuse_ops.load(Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// POSIX-3 — lstat() on a symlink must report strlen(target) DURABLY.
// ---------------------------------------------------------------------------

/// Targets across the interesting shapes: short, a realistic absolute
/// path, and a long (multi-component) one.
const SYMLINK_TARGETS: [&str; 3] = [
    "a",
    "/usr/lib/x86_64-linux-gnu/libsqueezefs_il.so.1.1.0",
    "../../../../var/lib/squeezefs/very/deep/relative/target/with/many/components/file.dat",
];

/// The durable inode record — not the reply, not the daemon attr cache —
/// must carry `size == strlen(target)`. `symlink()` patched the size into
/// the ReplyEntry and the cache only; the create transaction committed
/// size 0, and the `get_attr_internal` size-coherency repair is
/// regular-files-only, so the FIRST stat after the attr TTL lapsed
/// reported `st_size == 0` forever. Tools that size a `readlink()` buffer
/// from `st_size` (tar, rsync, cpio, `cp -a`) then record EMPTY targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_commits_target_length_into_the_durable_inode() {
    use squeezefs::meta_backend::Metadata;
    let h = make().await;
    for (i, target) in SYMLINK_TARGETS.iter().enumerate() {
        let name = format!("posix3_durable_{i}");
        let entry =
            h.fs.symlink(h.req, 1, OsStr::new(&name), OsStr::new(target))
                .await
                .unwrap();
        let ino = entry.attr.ino;
        assert_eq!(
            entry.attr.size,
            target.len() as u64,
            "the symlink reply must size the link"
        );

        let durable =
            h.fs.meta_backend
                .as_ref()
                .unwrap()
                .getattr(ino)
                .await
                .unwrap();
        assert_eq!(
            durable.size,
            target.len() as u64,
            "the DURABLE inode record for '{name}' -> '{target}' must carry \
             size = strlen(target); got {}",
            durable.size
        );
    }
}

/// The user-visible face: once the attr cache no longer holds the entry
/// (the 1 s TTL lapsing on a real mount — forced deterministically here
/// by an invalidation rather than a sleep), `getattr` must still report
/// the link length.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_size_survives_attr_cache_expiry() {
    let h = make().await;
    for (i, target) in SYMLINK_TARGETS.iter().enumerate() {
        let name = format!("posix3_ttl_{i}");
        let ino =
            h.fs.symlink(h.req, 1, OsStr::new(&name), OsStr::new(target))
                .await
                .unwrap()
                .attr
                .ino;

        // Deterministic TTL lapse: drop the daemon-side cached attr, so
        // the next stat is served from the durable record.
        h.fs.attr_cache.invalidate(&ino);

        let attr = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
        assert_eq!(
            attr.size,
            target.len() as u64,
            "post-TTL lstat of '{name}' must report strlen('{target}') = {}; \
             got {} — a readlink() buffer sized from this records an EMPTY target",
            target.len(),
            attr.size
        );

        // And a fresh LOOKUP (the path every cold `ls -l` takes) agrees.
        h.fs.attr_cache.invalidate(&ino);
        let looked = h.fs.lookup(h.req, 1, OsStr::new(&name)).await.unwrap().attr;
        assert_eq!(
            looked.size,
            target.len() as u64,
            "LOOKUP must size the link"
        );

        // The target itself is unchanged by any of this.
        let data = h.fs.readlink(h.req, ino).await.unwrap().data;
        assert_eq!(
            data.as_ref(),
            target.as_bytes(),
            "readlink must still return the exact target"
        );
    }
}

// ---------------------------------------------------------------------------
// POSIX-2 — sparse files must be visible: lseek(SEEK_DATA/SEEK_HOLE) and
// an st_blocks derived from actual allocation.
// ---------------------------------------------------------------------------

const SEEK_DATA: u32 = libc::SEEK_DATA as u32;
const SEEK_HOLE: u32 = libc::SEEK_HOLE as u32;

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn seek(h: &H, ino: u64, off: u64, whence: u32) -> Result<u64, i32> {
    match h.fs.lseek(h.req, ino, 0, off, whence).await {
        Ok(r) => Ok(r.offset),
        Err(e) => Err(-libc::c_int::from(e)),
    }
}

/// `st_blocks` in 512-byte units, as `stat(2)` reports it.
async fn st_blocks(h: &H, ino: u64) -> u64 {
    h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.blocks
}

async fn fsync_ino(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

/// A striped file with two written blocks separated by two never-written
/// ones: the canonical `dd seek=` sparse shape. `SEEK_HOLE`/`SEEK_DATA`
/// must land on the block boundaries — with no lseek handler the kernel
/// falls back to "the whole file is data", and `cp --sparse`, `tar -S`,
/// `rsync -S`, and `qemu-img` all expand the holes to full-size copies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lseek_finds_holes_and_data_in_a_striped_file() {
    let h = make().await;
    let ino = create_in(&h, 1, "posix2_striped").await;

    // Blocks 0 and 3 written; blocks 1 and 2 never touched. Size 4 blocks.
    write_at(&h, ino, 0, &vec![0x11u8; BS as usize]).await;
    write_at(&h, ino, 3 * BS, &vec![0x33u8; BS as usize]).await;
    fsync_ino(&h, ino).await;
    let size = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size;
    assert_eq!(size, 4 * BS, "sparse write must extend the size");

    assert_eq!(
        seek(&h, ino, 0, SEEK_DATA).await,
        Ok(0),
        "block 0 holds data"
    );
    assert_eq!(
        seek(&h, ino, 0, SEEK_HOLE).await,
        Ok(BS),
        "the hole starts where block 1 does"
    );
    assert_eq!(
        seek(&h, ino, BS, SEEK_HOLE).await,
        Ok(BS),
        "an offset already inside a hole returns itself"
    );
    assert_eq!(
        seek(&h, ino, BS, SEEK_DATA).await,
        Ok(3 * BS),
        "the next data after the hole is block 3"
    );
    assert_eq!(
        seek(&h, ino, 2 * BS + 17, SEEK_DATA).await,
        Ok(3 * BS),
        "an unaligned offset inside the hole still finds block 3"
    );
    assert_eq!(
        seek(&h, ino, 3 * BS, SEEK_HOLE).await,
        Ok(4 * BS),
        "the implicit hole at EOF terminates the last data run"
    );

    // Past EOF is ENXIO for both whences (POSIX).
    assert_eq!(seek(&h, ino, 4 * BS, SEEK_DATA).await, Err(libc::ENXIO));
    assert_eq!(seek(&h, ino, 4 * BS, SEEK_HOLE).await, Err(libc::ENXIO));
}

/// `st_blocks` must reflect ALLOCATION, not size: two of four blocks are
/// mapped, so `du` must report half the apparent size. Synthesizing it
/// from size is what makes `cp --sparse=auto` (whose heuristic is
/// `st_blocks * 512 < st_size`) never even look for holes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn st_blocks_reflects_allocation_for_a_sparse_striped_file() {
    let h = make().await;
    let ino = create_in(&h, 1, "posix2_blocks").await;

    write_at(&h, ino, 0, &vec![0x11u8; BS as usize]).await;
    write_at(&h, ino, 3 * BS, &vec![0x33u8; BS as usize]).await;
    fsync_ino(&h, ino).await;

    let blocks = st_blocks(&h, ino).await;
    let dense = (4 * BS) / 512;
    let sparse = (2 * BS) / 512;
    assert_eq!(
        blocks, sparse,
        "st_blocks must count the 2 allocated blocks ({sparse} × 512-B units), \
         not the {dense} a size-derived synthesis reports"
    );
}

/// `fallocate(PUNCH_HOLE)` must become visible to both instruments: the
/// punched block is a hole for lseek and stops counting toward st_blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn punched_hole_is_visible_to_lseek_and_st_blocks() {
    let h = make().await;
    let ino = create_in(&h, 1, "posix2_punch").await;

    write_at(&h, ino, 0, &vec![0x55u8; 4 * BS as usize]).await;
    fsync_ino(&h, ino).await;
    let dense_blocks = st_blocks(&h, ino).await;
    assert_eq!(
        dense_blocks,
        (4 * BS) / 512,
        "a dense file counts every block"
    );
    assert_eq!(
        seek(&h, ino, 0, SEEK_HOLE).await,
        Ok(4 * BS),
        "a dense file has no hole before EOF"
    );

    h.fs.fallocate(
        h.req,
        ino,
        0,
        BS,
        BS,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .unwrap();

    assert_eq!(
        seek(&h, ino, 0, SEEK_HOLE).await,
        Ok(BS),
        "the punched block must read as a hole"
    );
    assert_eq!(
        seek(&h, ino, BS, SEEK_DATA).await,
        Ok(2 * BS),
        "data resumes at the block after the punch"
    );
    assert_eq!(
        st_blocks(&h, ino).await,
        dense_blocks - BS / 512,
        "the punched block must stop counting toward st_blocks"
    );
}

/// Inline and staged layouts hold no hole information — the honest (and
/// the only POSIX-safe) answer there is "all data": SEEK_DATA returns the
/// offset, SEEK_HOLE returns EOF. Reporting a hole where data lives would
/// make `cp --sparse` silently drop bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lseek_reports_inline_and_staged_files_as_fully_populated() {
    let h = make().await;
    for (size, name) in [(2048usize, "posix2_inline"), (40_000usize, "posix2_staged")] {
        let ino = create_in(&h, 1, name).await;
        write_at(&h, ino, 0, &vec![0x7Eu8; size]).await;
        let size = size as u64;

        assert_eq!(
            seek(&h, ino, 0, SEEK_DATA).await,
            Ok(0),
            "{name}: data at 0"
        );
        assert_eq!(
            seek(&h, ino, 0, SEEK_HOLE).await,
            Ok(size),
            "{name}: the only hole is the implicit one at EOF"
        );
        assert_eq!(
            seek(&h, ino, size / 2, SEEK_DATA).await,
            Ok(size / 2),
            "{name}: an interior offset is data"
        );
        assert_eq!(
            seek(&h, ino, size, SEEK_DATA).await,
            Err(libc::ENXIO),
            "{name}: EOF is ENXIO"
        );
    }
}

/// Dirty custody is DATA: a block whose bytes are still parked in the RAM
/// overlay / staging ring (never yet published into the block map) must
/// never be reported as a hole — a `cp --sparse` racing writeback would
/// otherwise skip acked bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unflushed_partial_block_is_data_not_a_hole() {
    let h = make().await;
    let ino = create_in(&h, 1, "posix2_dirty").await;

    // Block 0 complete (write-through), block 3 only PARTIALLY written —
    // its bytes are parked, not mapped.
    write_at(&h, ino, 0, &vec![0x11u8; BS as usize]).await;
    write_at(&h, ino, 3 * BS, &vec![0x33u8; 4096]).await;

    assert_eq!(
        seek(&h, ino, 3 * BS, SEEK_DATA).await,
        Ok(3 * BS),
        "parked (unflushed) bytes are DATA"
    );
    assert_eq!(
        seek(&h, ino, 2 * BS, SEEK_DATA).await,
        Ok(3 * BS),
        "the hole before them still resolves to the parked block"
    );
}
