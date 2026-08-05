//! The live-statfs ENOSPC-drift repro + fix contract (filed 2026-08-05:
//! `.benchmarks/2026-08-05-fleet-parity-writes.md` addendum + the
//! il-durable-write note's report-only item).
//!
//! Field shape: after ENOSPC + rm churn (field, ~190 GB drift) or plain
//! write+rm cycles (local, ~15.5 GB stuck low), the LIVE mount's statfs
//! `avail` sticks low with the block-reclaim queue fully drained, while
//! a remount converges instantly — the on-disk accounting is sound, the
//! LIVE gauge (`BackendRouter::allocated_bytes` = per-allocator
//! `highest_block − free_blocks.len()`, the statfs handler's only
//! source) drifts.
//!
//! ROOT CAUSE (pinned by ALLOC-SITE attribution on this repro) — three
//! faces of one class, all reachable from the ENOSPC + rm schedule:
//!
//! 1. **Publish-into-the-reclaim-window.** A flush unit parked in
//!    `allocate_block`'s ENOSPC pressure valve is woken by the very
//!    frees `delete_file` produces; its staged source is still present
//!    (the staged sweep runs later in `delete_file`), so it DMAs and
//!    successfully MERGES its fresh block into the layout in the window
//!    between `delete_file`'s map snapshot and `destroy_inodes` — whose
//!    record+xattr destruction deliberately never decodes layouts.
//!    Fixed by the ino-reclaim latch: `reclaim_inflight` (the existing
//!    single-drive claim held across delete_file→destroy) is probed by
//!    the layout-save funnel (`save_metadata_to_backend_ext`) — a
//!    publish for a latched ino refuses NotFound-class, which every
//!    custody mover's merge-error arm already handles by FREEING its
//!    fresh block and riding the FIND-M11-A verified-orphan ladder;
//!    `delete_file`'s meta-lock serialization point orders every
//!    in-flight merge against the latch.
//! 2. **Cancelled flush units leak their mint.** fsync's flush fan-out
//!    (`flush_due_active_blocks_for_inode`: `buffer_unordered(8)` +
//!    first-error unwind) DROPS sibling units at their awaits — a
//!    cancel landing in the claim→publish window left the offset's
//!    refcount live forever. Fixed with RES-9 mint guards
//!    (`MintedBlockGuard`, the `write_striped` task discipline) in
//!    `flush_one_active_block` / `upload_active_block_bytes` /
//!    `fold_upload_block`.
//! 3. **The sparse-promote caller leaked its batch on commit failure.**
//!    `write_file`'s staged→striped promotion `?`-propagated a failing
//!    `save_metadata_to_backend_refs` without freeing the blocks
//!    `durable_write_sparse_blocks` had just minted. Fixed by
//!    orphan-freeing the minted keys on the failing commit arm (after
//!    the meta guard drops — RES-1).
//!
//! The sandbox venue (the filing's prescription): write files toward the
//! sandbox's capacity, delete everything, drain reclaim, and assert the
//! live gauge converges to the remount-equivalent value WITHOUT
//! remounting. With every file deleted the remount-equivalent used is
//! exactly the baseline (a remount's recovery walk finds no layouts), so
//! convergence == returning to the pre-write baseline.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1024 * 1024; // 1 MiB logical blocks (whole-block write-through)
const CAP_BLOCKS: u64 = 16; // allocator chunks (4 MiB each): 64 MiB sandbox

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
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
    // The sandbox's capacity (in 4 MiB allocator chunks): writes past it
    // refuse StorageFull — the ENOSPC cycle's whole point.
    ba.set_capacity_bytes(CAP_BLOCKS * ba.chunk_size());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("16MB"),
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
            hash_seed: 0x57A7_F5D1_2026_0805,
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

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Whole-block writes (write-through, striped from birth); returns the
/// first error, if any (the ENOSPC cycle wants it).
async fn write_blocks(h: &H, ino: u64, blocks: u64) -> Option<fuse3::Errno> {
    for i in 0..blocks {
        let data = bytes::Bytes::from(vec![(i % 251) as u8; BS as usize]);
        match h.fs.write(h.req, ino, 0, i * BS, data, 0, 0).await {
            Ok(w) => assert_eq!(w.written as u64, BS, "short write at block {i}"),
            Err(e) => return Some(e),
        }
    }
    None
}

/// Delete + reclaim + drain: unlink, close the tracked handle, run the
/// reclaim batch (the RELEASE/FORGET drive, modeled directly like
/// rw5a_never_lossy_tests does), then drain the device-reclaim queue.
async fn delete_and_drain(h: &H, name: &str, ino: u64) {
    h.fs.unlink(h.req, 1, OsStr::new(name)).await.unwrap();
    let _ = h.fs.release(h.req, ino, ino, 0, 0, false).await;
    h.fs.reclaim_orphaned_batch(vec![ino]).await;
    h.fs.router.backend_router.reclaim_drain().await;
}

fn live_used(h: &H) -> u64 {
    h.fs.router.backend_router.allocated_bytes()
}

/// Bounded eventually (the §3d.3 poll-first template): valve-parked
/// custody movers settle asynchronously after a delete — re-drain and
/// poll before judging. A converging gauge is settling; a floor past the
/// deadline is the drift.
async fn settle_to(h: &H, target: u64) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let now = live_used(h);
        if now == target || std::time::Instant::now() > deadline {
            return now;
        }
        h.fs.router.backend_router.reclaim_drain().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The repro: churn (plain + ENOSPC cycles), then assert the live gauge
/// converges to the remount-equivalent value (everything deleted ⇒ the
/// baseline) WITHOUT a remount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_statfs_converges_after_enospc_and_rm_churn() {
    let h = make(*b"statfsdrift-2026", "statfs_drift_ns").await;
    let used0 = live_used(&h);

    // ---- Cycles 1..2: plain write+rm churn (the il-durable-write note's
    // local shape — write 8 blocks durable, rm, drain, converge).
    for c in 0..2u32 {
        let name = format!("churn_{c}");
        let ino = create(&h, &name).await;
        assert_eq!(write_blocks(&h, ino, 8).await, None, "cycle {c} writes");
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        let path = squeezefs::keys::inode_path(ino);
        let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
        assert_eq!(meta.file_type, "striped", "fixture must write through");
        delete_and_drain(&h, &name, ino).await;
        let after = settle_to(&h, used0).await;
        assert_eq!(
            after,
            used0,
            "plain write+rm cycle {c}: live used must converge to the \
             remount-equivalent baseline (everything deleted, reclaim \
             drained) — a residue is the il-durable-write note's local \
             drift shape ({} B stuck)",
            after.saturating_sub(used0)
        );
    }

    // ---- Cycle 3: write toward capacity until ENOSPC, then rm. The
    // refusal may surface at write(2) (the field's psync shape) or at the
    // durability barrier (never-lossy acks absorb the write and the
    // promote refuses) — either way the store genuinely ran out, which is
    // what parks flush units in the allocator's ENOSPC pressure valve:
    // the delete's own frees then wake them INTO the reclaim window (the
    // root-caused leak schedule).
    let name = "enospc_victim";
    let ino = create(&h, name).await;
    let werr = write_blocks(&h, ino, CAP_BLOCKS * 4 + 8).await;
    let ferr = h.fs.fsync(h.req, ino, 0, false).await.err();
    assert!(
        werr.is_some() || ferr.is_some(),
        "the sandbox must actually run out of space (capacity {CAP_BLOCKS} \
         chunks — fixture sanity; write err={werr:?} fsync err={ferr:?})"
    );
    delete_and_drain(&h, name, ino).await;
    let after_enospc = settle_to(&h, used0).await;
    if after_enospc != used0 {
        let (_, alloc, _) = h.fs.router.backend_router.get_active_backend().unwrap();
        eprintln!(
            "LEAK-DIAG tracked={:?} inflight={:?} free={}",
            alloc.tracked_offsets(),
            alloc.inflight_offsets(),
            alloc.free_blocks_count()
        );
    }
    assert_eq!(
        after_enospc,
        used0,
        "ENOSPC + rm churn: live used must converge to the \
         remount-equivalent baseline without remounting — a residue here is \
         the fleet-parity addendum's field drift ({} B stuck; on-disk \
         accounting is sound, the LIVE gauge is what statfs serves; the \
         leaked refcounts belong to flush units that published into the \
         delete_file→destroy window)",
        after_enospc.saturating_sub(used0)
    );

    // And the store must be fully usable again: a fresh file spanning most
    // of the capacity fits (the drift's operational face was ENOSPC on a
    // mostly-empty store).
    let ino2 = create(&h, "post_churn").await;
    assert_eq!(
        write_blocks(&h, ino2, CAP_BLOCKS - 2).await,
        None,
        "post-churn: a {}-chunk file must fit in a {}-chunk store whose \
         files were all deleted (avail is genuinely back)",
        CAP_BLOCKS - 2,
        CAP_BLOCKS
    );
}
