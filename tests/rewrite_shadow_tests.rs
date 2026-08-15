//! Idea 1 — the shadow dual-map rewrite epoch
//! (`docs/design-rewrite-program.md` §5; the rewrite program's
//! fresh-shaped-overwrite vehicle).
//!
//! A rewrite epoch makes a sequential overwrite structurally identical
//! to fresh ingest: complete-block write-throughs on a striped ino
//! allocate fresh (map B) blocks and record their bindings in the
//! RAM-authoritative metadata cache ONLY (reads are RYW by the
//! dirty-authority law; the durable map A stays untouched), displaced A
//! keys PARK in the epoch, and ONE whole-tx save publishes the swap at
//! the close triggers (full coverage / fsync / RELEASE / idle). Parked
//! A keys free only after a durable save that no longer references them
//! (the §5.2 deferred-free law — the safety keystone that also makes
//! intermediate dirty-persists legal partial swaps); crash recovery
//! owns every other window (W1–W6).
//!
//! Contracts:
//! 1. **RAM-only records + the swap boundary**: mid-epoch the DURABLE
//!    layout still names A while reads serve B (RYW, surviving a cache
//!    eviction — the refetch-compose hook); displaced frees are flat
//!    until the close and exact after it; the epoch gauges track.
//! 2. **Full coverage auto-closes**: a whole-file rewrite swaps without
//!    any fsync; durable map = B; bytes on device exact.
//! 3. **W1 crash pre-swap**: dropping the daemon mid-epoch leaves A
//!    intact durably — a reopened volume reads the OLD bytes and the
//!    recovery walk reclaims every B block (un-fsynced acked writes are
//!    lost: the writeback-class contract, unchanged).
//! 4. **W5 fenced close**: a stale-token close publishes NOTHING and
//!    frees NOTHING (successor accounting), loudly counted.
//! 5. **ENOSPC early-close**: a mid-epoch StorageFull closes the epoch
//!    (the swap frees parked A supply) and the rewrite converges;
//!    counted `rewrite_shadow_fallbacks`.
//! 6. **fsck composition**: mid-epoch B offsets are live-owner
//!    registered (the §5.6 in-flight-registry C2/C3 exemption hook);
//!    deregistered after the swap.
//! 7. **The lever**: `SQUEEZEFS_REWRITE_SHADOW=0` restores per-block
//!    durable publishes verbatim.
//!
//! RED against `e8fb912`: no epoch machinery exists — every rewrite
//! publish commits durably per block.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::Metadata;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore default posture on scope exit (knob hygiene). The W1 patch
/// path is disabled (`set_patch_max_bytes(0)`): lone whole-block
/// overwrites must ride the write-through pipeline these contracts pin.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::set_rewrite_shadow(true);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    backing: NamedTempFile,
    m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness_on(
    test_id: &str,
    backing: NamedTempFile,
    m: NamedTempFile,
    format: bool,
    capacity_blocks: Option<u64>,
) -> H {
    // B4c-ii: the overwrite OVERLAY (default ON) would take this
    // suite's mapped whole-block overwrites instead of the ACCUMULATION
    // shadow feed under test — lever OFF (LeverGuard restores; the
    // overlay-fed epoch laws are pinned in
    // tests/overlay_overwrite_tests.rs).
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);

    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    if let Some(blocks) = capacity_blocks {
        ba.set_capacity_bytes(blocks * ba.chunk_size());
    }
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    if format {
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
        .expect("format v3 meta volume");
    }
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    if !format {
        // Remount shape: rebuild the allocator population from durable
        // maps (the recovery walk — what reclaims W1's B orphans).
        for kv in &routed.volumes {
            ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs: Arc::new(fs),
        req,
        backing,
        m,
        _s: s,
    }
}

async fn make_harness(test_id: &str) -> H {
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    make_harness_on(test_id, backing, m, true, None).await
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

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
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

async fn read_all(h: &H, ino: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, 0, len as u32, 0)
        .await
        .expect("read")
        .data
        .to_vec()
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

/// Fresh striped fixture of `blocks` full blocks, drained + fsync'd.
async fn striped_fixture(h: &H, name: &str, blocks: u64, seed: u8) -> u64 {
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    write_at(h, ino, 0, &pattern((blocks * FBS) as usize, seed)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(h).await;
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "fixture premise: every block mapped"
    );
    ino
}

/// The DURABLE layout's block map, read straight from the metadata
/// backend (bypasses the RAM-authoritative cache — the swap-boundary
/// instrument).
async fn durable_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let bytes =
        h.fs.meta_backend
            .as_ref()
            .expect("backend")
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
    let layout: squeezefs::layout_wire::LayoutMetadata = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).expect("json layout")
    } else {
        bincode::deserialize(&bytes).expect("bincode layout")
    };
    layout.block_map.unwrap_or_default()
}

/// The RAM-authoritative map (what reads resolve through).
async fn ram_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    (*h.fs
        .router
        .fetch_metadata(&path)
        .await
        .expect("meta")
        .block_map
        .expect("mapped"))
    .clone()
}

fn shadow_swaps() -> u64 {
    METRICS.rewrite_shadow_swaps.load(Ordering::Relaxed)
}
fn shadow_bytes() -> u64 {
    METRICS.rewrite_shadow_bytes.load(Ordering::Relaxed)
}
fn shadow_fallbacks() -> u64 {
    METRICS.rewrite_shadow_fallbacks.load(Ordering::Relaxed)
}
fn shadow_fence_drops() -> u64 {
    METRICS.rewrite_shadow_fence_drops.load(Ordering::Relaxed)
}
fn open_epochs() -> u64 {
    METRICS.rewrite_shadow_open_epochs.load(Ordering::Relaxed)
}
fn parked_bytes() -> u64 {
    METRICS.rewrite_shadow_parked_bytes.load(Ordering::Relaxed)
}
fn terminal_frees() -> u64 {
    METRICS.block_free_reclaim_queued.load(Ordering::Relaxed)
        + METRICS.block_free_reclaim_elided.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Contract 1 — RAM-only records, RYW (+ eviction), the swap boundary.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn epoch_records_are_ram_only_until_the_one_save_swap() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_swap_boundary").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 1).await;
    let map_a = ram_block_map(&h, ino).await;

    // Partial rewrite (blocks 0–1): the epoch stays open (coverage < file).
    let (f0, ob0, pb0) = (terminal_frees(), open_epochs(), parked_bytes());
    let v1 = pattern(2 * FBS as usize, 7);
    write_at(&h, ino, 0, &v1).await;
    quiesce(&h).await;

    // Mid-epoch: durable = A, RAM = B (RYW), frees flat, gauges track.
    let durable = durable_block_map(&h, ino).await;
    for b in 0..blocks as u32 {
        assert_eq!(
            durable.get(&b),
            map_a.get(&b),
            "block {b}: the DURABLE map must still name A mid-epoch (the \
             swap has not happened)"
        );
    }
    let ram = ram_block_map(&h, ino).await;
    assert_ne!(ram.get(&0), map_a.get(&0), "block 0 rebinds in RAM");
    assert_ne!(ram.get(&1), map_a.get(&1), "block 1 rebinds in RAM");
    assert_eq!(ram.get(&2), map_a.get(&2), "block 2 untouched");
    assert_eq!(
        terminal_frees() - f0,
        0,
        "displaced A keys PARK — no free may happen before the swap is \
         durable (the §5.2 deferred-free law)"
    );
    assert_eq!(open_epochs() - ob0, 1, "one open epoch gauged");
    assert_eq!(
        parked_bytes() - pb0,
        2 * FBS,
        "parked A bytes gauged (the VL preflight transient)"
    );

    // RYW survives a cache eviction (the refetch-compose hook).
    h.fs.router.metadata_cache.remove(&ino);
    let got = read_all(&h, ino, 2 * FBS as usize).await;
    assert_eq!(
        got, v1,
        "reads after an eviction must still observe the epoch's bindings \
         (fetch_metadata_from_backend composes the shadow — KD-1.9)"
    );

    // fsync = the swap: ONE save, then the parked A frees.
    let s0 = shadow_swaps();
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert_eq!(shadow_swaps() - s0, 1, "the close is one counted swap");
    assert_eq!(
        terminal_frees() - f0,
        2,
        "exactly the displaced A keys free after the durable swap"
    );
    assert_eq!(open_epochs() - ob0, 0, "epoch gauge returns");
    assert_eq!(parked_bytes() - pb0, 0, "parked gauge returns");
    let durable = durable_block_map(&h, ino).await;
    assert_eq!(durable.get(&0), ram.get(&0), "block 0 swapped durably");
    assert_eq!(durable.get(&1), ram.get(&1), "block 1 swapped durably");
    assert_eq!(durable.get(&2), map_a.get(&2), "block 2 keeps A");
}

// ---------------------------------------------------------------------------
// Contract 2 — full coverage auto-closes (no fsync needed).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_coverage_auto_closes_the_epoch() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_auto_close").await;
    let blocks = 3u64;
    let len = (blocks * FBS) as usize;
    let ino = striped_fixture(&h, "f1", blocks, 2).await;
    let map_a = ram_block_map(&h, ino).await;

    let (s0, sb0) = (shadow_swaps(), shadow_bytes());
    let v1 = pattern(len, 9);
    write_at(&h, ino, 0, &v1).await;
    quiesce(&h).await;

    assert!(
        shadow_swaps() - s0 >= 1,
        "full coverage must auto-close the epoch (the natural end of a \
         sequential overwrite)"
    );
    assert!(
        shadow_bytes() - sb0 >= blocks * FBS,
        "swapped bytes account the epoch"
    );
    let durable = durable_block_map(&h, ino).await;
    for b in 0..blocks as u32 {
        assert_ne!(
            durable.get(&b),
            map_a.get(&b),
            "block {b}: the durable map names B after the auto-close"
        );
    }
    assert_eq!(read_all(&h, ino, len).await, v1, "read-back exact");

    // Bytes on the device at the swapped offsets (passthrough volume).
    use std::os::unix::fs::FileExt;
    let dev = std::fs::File::open(h.backing.path()).expect("open backing");
    let mut buf = vec![0u8; FBS as usize];
    for b in 0..blocks as u32 {
        let key = durable.get(&b).expect("mapped");
        let (_, off) = h.fs.router.backend_router.parse_block_key(key).unwrap();
        dev.read_exact_at(&mut buf, off).expect("pread device");
        assert_eq!(
            buf,
            v1[(b as usize * FBS as usize)..((b as usize + 1) * FBS as usize)],
            "block {b}: device bytes exact at the B offset"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — W1: crash pre-swap leaves A intact; recovery reclaims B.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_pre_swap_leaves_a_intact_and_reclaims_b() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_w1_crash").await;
    let blocks = 4u64;
    let v0 = pattern((blocks * FBS) as usize, 3);
    let ino = striped_fixture(&h, "f1", blocks, 3).await;

    // Partial rewrite mid-epoch — RAM-only bindings, never persisted.
    write_at(&h, ino, 0, &pattern(2 * FBS as usize, 8)).await;
    quiesce(&h).await;
    assert!(open_epochs() > 0, "premise: the epoch is open");

    // CRASH: drop the daemon without close/unmount (the
    // drop-without-shutdown replay shape).
    let H {
        fs, backing, m, _s, ..
    } = h;
    drop(fs);
    drop(_s);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Remount: fresh harness over the SAME meta + data backings.
    let h2 = make_harness_on("shadow_w1_crash_remount", backing, m, false, None).await;

    // A intact: the file reads the PRE-REWRITE bytes entirely (un-fsynced
    // acked writes lost — the writeback-class contract, unchanged).
    let got = read_all(&h2, ino, (blocks * FBS) as usize).await;
    assert_eq!(
        got, v0,
        "W1: the durable map (A) serves the pre-rewrite image after a \
         mid-epoch crash"
    );

    // Recovery reclaimed the B orphans: the allocator tracks exactly the
    // A blocks, and B offsets are re-allocatable.
    assert_eq!(
        h2.fs.router.block_allocator.get_used_blocks(),
        blocks,
        "W1: recovery seeds refcounts from durable maps only — B offsets \
         re-enter the free pool"
    );

    // The volume stays fully writable (reuse guarded by
    // write-before-publish + the incarnation seqlock).
    let v2 = pattern((blocks * FBS) as usize, 11);
    write_at(&h2, ino, 0, &v2).await;
    h2.fs.fsync(h2.req, ino, 0, false).await.expect("fsync");
    assert_eq!(read_all(&h2, ino, v2.len()).await, v2);
}

// ---------------------------------------------------------------------------
// Contract 4 — W5: a fenced close publishes nothing and frees nothing.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_close_publishes_nothing_and_frees_nothing() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_w5_fence").await;
    let blocks = 3u64;
    let ino = striped_fixture(&h, "f1", blocks, 4).await;
    let map_a = ram_block_map(&h, ino).await;

    write_at(&h, ino, 0, &pattern(FBS as usize, 5)).await;
    quiesce(&h).await;
    assert!(open_epochs() > 0, "premise: the epoch is open");

    // A STALE token (the wt_fencing pattern). The DLM generation-bump seam
    // advances the file's generator under the write path's live whole-file
    // lease; pre-S11 this was a `(0,1)` range lock, which byte-range
    // custody now (correctly) treats as a conflict with that lease.
    let path = squeezefs::keys::inode_path(ino);
    let stale = squeezefs::dlm::test_bump_fencing_generation(&path) - 1;

    let (f0, fd0) = (terminal_frees(), shadow_fence_drops());
    let res = h.fs.router.close_rewrite_epoch(ino, stale).await;
    assert!(
        matches!(
            res,
            Err(squeezefs::error::SqueezefsError::FencingTokenExpired { .. })
        ),
        "a fenced close must refuse loudly: {res:?}"
    );
    assert_eq!(shadow_fence_drops() - fd0, 1, "counted fence drop");
    assert_eq!(
        terminal_frees() - f0,
        0,
        "W5: a fenced holder frees NOTHING — not A (the durable map may \
         still reference it), not B (successor accounting)"
    );
    let durable = durable_block_map(&h, ino).await;
    for b in 0..blocks as u32 {
        assert_eq!(
            durable.get(&b),
            map_a.get(&b),
            "block {b}: the durable map is untouched by the fenced close"
        );
    }
    assert_eq!(open_epochs(), 0, "the epoch is discarded (remount law)");
}

// ---------------------------------------------------------------------------
// Contract 5 — ENOSPC early-close frees parked supply and converges.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enospc_early_close_frees_supply_and_converges() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    // Elision keeps freed offsets instantly reallocatable in this
    // schedule (bdev-classification seam for the file-backed harness).
    squeezefs::block_reclaim::set_elision_class_all(true);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    // Capacity 6 blocks: a 4-block file leaves 2 virgin blocks — a full
    // shadow rewrite MUST hit StorageFull mid-epoch and early-close.
    let h = make_harness_on("shadow_enospc", backing, m, true, Some(6)).await;
    let blocks = 4u64;
    let len = (blocks * FBS) as usize;
    let ino = striped_fixture(&h, "f1", blocks, 6).await;

    let fb0 = shadow_fallbacks();
    let v1 = pattern(len, 13);
    write_at(&h, ino, 0, &v1).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(&h).await;

    assert!(
        shadow_fallbacks() - fb0 >= 1,
        "the mid-epoch StorageFull must be a counted early-close \
         (rewrite_shadow_fallbacks — the loud fallback to today's CoW)"
    );
    assert_eq!(read_all(&h, ino, len).await, v1, "the rewrite converged");
    squeezefs::block_reclaim::set_elision_class_all(false);
}

// ---------------------------------------------------------------------------
// Contract 6 — fsck composition: B offsets are live-owner registered.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_epoch_b_offsets_are_inflight_registered() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_fsck_hook").await;
    let blocks = 3u64;
    let ino = striped_fixture(&h, "f1", blocks, 5).await;
    let map_a = ram_block_map(&h, ino).await;

    write_at(&h, ino, 0, &pattern(FBS as usize, 6)).await;
    quiesce(&h).await;

    // The B offset (RAM map, block 0) is allocated + tree-unreferenced —
    // exactly fsck C2's shape — and must be shielded by a LIVE in-flight
    // registration for the whole epoch (the §5.6 exemption hook, KD-1.3).
    let ram = ram_block_map(&h, ino).await;
    let b_key = ram.get(&0).cloned().expect("B binding");
    assert_ne!(Some(&b_key), map_a.get(&0), "premise: block 0 rebound");
    let (_, b_off) = h.fs.router.backend_router.parse_block_key(&b_key).unwrap();
    assert!(
        h.fs.router.block_allocator.inflight_contains(b_off),
        "mid-epoch B offsets must be in-flight registered (fsck C2/C3 \
         live-owner exemption)"
    );

    // The swap releases the registration (publish visible ⇒ deregister).
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        !h.fs.router.block_allocator.inflight_contains(b_off),
        "the guard drops with the swap (deregister-after-publish-visible)"
    );
}

// ---------------------------------------------------------------------------
// Contract 7 — the lever restores per-block durable publishes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_restores_per_block_durable_publishes() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(false);
    let h = make_harness("shadow_lever_off").await;
    let blocks = 2u64;
    let len = (blocks * FBS) as usize;
    let ino = striped_fixture(&h, "f1", blocks, 7).await;
    let map_a = ram_block_map(&h, ino).await;

    let s0 = shadow_swaps();
    write_at(&h, ino, 0, &pattern(len, 21)).await;
    quiesce(&h).await;

    assert_eq!(shadow_swaps() - s0, 0, "lever off ⇒ no epochs, no swaps");
    let durable = durable_block_map(&h, ino).await;
    for b in 0..blocks as u32 {
        assert_ne!(
            durable.get(&b),
            map_a.get(&b),
            "block {b}: per-block durable publish, verbatim (no fsync \
             needed for the durable rebind)"
        );
    }
}
