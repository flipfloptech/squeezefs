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
//! 4. **W5 fenced close**: a GENUINELY fenced close (the D0 custody
//!    poison / `WriterGuardFenced`) publishes NOTHING and frees NOTHING
//!    (successor accounting), loudly counted — while a PROCESS-LOCAL
//!    lease rotation (a stale token with a newer generation in this
//!    process) CONVERGES by re-presenting the current generation (4b;
//!    the 2026-08-06 tail-loss law applied to the swap).
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
fn pressure_closes() -> u64 {
    METRICS
        .rewrite_shadow_pressure_closes
        .load(Ordering::Relaxed)
}
fn pressure_close_busy() -> u64 {
    METRICS
        .rewrite_shadow_pressure_close_busy
        .load(Ordering::Relaxed)
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

/// The GENUINE fence class (design-rewrite-program §5.4 KD-1.8: "the D0
/// `failed` latch refuses the commit"): the process-wide custody poison
/// the D0 fence sets. A poisoned holder's close publishes NOTHING, frees
/// NOTHING, discards the epoch, and refuses `WriterGuardFenced` — the
/// error class every other publish site treats as the fence.
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

    struct Unpoison;
    impl Drop for Unpoison {
        fn drop(&mut self) {
            squeezefs::data_custody::test_clear_poison();
        }
    }
    let _unpoison = Unpoison;
    squeezefs::data_custody::poison("contract 4: the D0 fence fired");

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let (f0, fd0) = (terminal_frees(), shadow_fence_drops());
    let res = h.fs.router.close_rewrite_epoch(ino, token).await;
    assert!(
        matches!(
            res,
            Err(squeezefs::error::SqueezefsError::WriterGuardFenced)
        ),
        "a fenced close must refuse loudly in the fence's own class: {res:?}"
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

/// Contract 4b — a PROCESS-LOCAL lease rotation is not a fence (the
/// 2026-08-06 tail-loss law, `tests/fsync_writeback_tail_loss_tests.rs`,
/// applied to the swap): within one process `FencingTokenExpired` from
/// the swap's save can only mean this same daemon re-acquired the ino's
/// lease between the closer's token capture and the revalidation
/// (sibling handles, stripe grants) — the epoch's RAM-only bindings are
/// the newest acked custody in existence. The close RE-PRESENTS the
/// current generation and converges: the swap persists (durable map =
/// B), the parked A keys free, the retry is counted, and NO fence drop
/// is recorded. Before this contract the stale token took the W5 arm on
/// a live mount — acked bytes discarded, the covered A keys' local
/// hygiene lost (the s11 co-writers' `CLAIM ANOMALY` lineage,
/// `.benchmarks/2026-09-06-cowriter-free-residual-lineage.md`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_local_lease_rotation_converges_the_close() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_rotation_converges").await;
    let blocks = 3u64;
    let ino = striped_fixture(&h, "f1", blocks, 4).await;
    let map_a = ram_block_map(&h, ino).await;

    let v1 = pattern(FBS as usize, 5);
    write_at(&h, ino, 0, &v1).await;
    quiesce(&h).await;
    assert!(open_epochs() > 0, "premise: the epoch is open");
    let map_b = ram_block_map(&h, ino).await;
    assert_ne!(
        map_b.get(&0),
        map_a.get(&0),
        "premise: block 0 is shadow-bound"
    );

    // The rotation: a newer lease in THIS process (the fsync-vs-sibling
    // shape), the closer still holding the older token.
    let path = squeezefs::keys::inode_path(ino);
    let stale = squeezefs::dlm::test_bump_fencing_generation(&path) - 1;

    let (f0, fd0, r0, s0) = (
        terminal_frees(),
        shadow_fence_drops(),
        METRICS.rewrite_shadow_close_retries.load(Ordering::Relaxed),
        shadow_swaps(),
    );
    let res = h.fs.router.close_rewrite_epoch(ino, stale).await;
    assert!(
        matches!(res, Ok(true)),
        "a rotated token converges — the close publishes under the current generation: {res:?}"
    );
    assert_eq!(
        METRICS.rewrite_shadow_close_retries.load(Ordering::Relaxed) - r0,
        1,
        "the convergence is counted"
    );
    assert_eq!(shadow_fence_drops() - fd0, 0, "a rotation is not a fence");
    assert_eq!(shadow_swaps() - s0, 1, "the swap persisted");
    assert_eq!(
        terminal_frees() - f0,
        1,
        "the parked A key of the rewritten block freed after the swap"
    );
    let durable = durable_block_map(&h, ino).await;
    assert_eq!(
        durable.get(&0),
        map_b.get(&0),
        "the durable map names the epoch's B binding — the acked bytes are durable"
    );
    for b in 1..blocks as u32 {
        assert_eq!(
            durable.get(&b),
            map_a.get(&b),
            "block {b}: untouched blocks keep their A binding"
        );
    }
    assert_eq!(open_epochs(), 0, "the epoch closed");
    assert_eq!(
        read_all(&h, ino, FBS as usize).await,
        v1,
        "the rewritten bytes read back (no fence-era discard)"
    );
}

// ---------------------------------------------------------------------------
// Contract 5 — ENOSPC early-close frees parked supply and converges.
// ---------------------------------------------------------------------------

/// KD-1.7: a mid-epoch `StorageFull` closes the epoch (the swap frees the
/// parked A supply) and the allocation retries once. Since record §4.4cd
/// the close runs at the ONE allocation act (`allocate_placed_block`'s
/// pressure close, `rewrite_shadow_pressure_closes`) — so the write
/// pipeline's own arm (`rewrite_shadow_fallbacks`, the PARKING close that
/// follows a `StorageFull` the act still surfaced) is reached only when
/// the act's non-parking close found the ino's stripe held; here nothing
/// holds it, so the act closes and the arm counts nothing. The law the
/// contract pins is the SUM: exactly one early-close of either face, the
/// rewrite converged, no refusal.
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

    let (fb0, pc0, busy0) = (shadow_fallbacks(), pressure_closes(), pressure_close_busy());
    let refused0 = METRICS
        .write_fresh_block_enospc_refusals
        .load(Ordering::Relaxed);
    let v1 = pattern(len, 13);
    write_at(&h, ino, 0, &v1).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(&h).await;

    let early_closes = (shadow_fallbacks() - fb0) + (pressure_closes() - pc0);
    assert!(
        early_closes >= 1,
        "the mid-epoch StorageFull must be a counted early-close on one of its two faces \
         (rewrite_shadow_pressure_closes at the allocation act, or rewrite_shadow_fallbacks \
         at the pipeline's parking arm)"
    );
    assert_eq!(
        pressure_closes() - pc0,
        1,
        "the allocation act's non-parking close is the face that fires when nothing holds \
         the ino's stripe — the pipeline's arm is its fallback"
    );
    assert_eq!(
        pressure_close_busy() - busy0,
        0,
        "no stripe was held: the act never skipped the epoch"
    );
    assert_eq!(
        METRICS
            .write_fresh_block_enospc_refusals
            .load(Ordering::Relaxed),
        refused0,
        "the parked supply was never a refusal"
    );
    assert_eq!(read_all(&h, ino, len).await, v1, "the rewrite converged");
    squeezefs::block_reclaim::set_elision_class_all(false);
}

// ---------------------------------------------------------------------------
// Contract 5b — a WRITEBACK flush's StorageFull closes the epoch that
// holds the supply (record §4.4cd — release chain attempt 12, generic/551).
// ---------------------------------------------------------------------------

/// The 1.3.0 release chain's attempt 12 hung in fstests generic/551 for
/// 4.6 h: a random O_DIRECT overwrite of one file parked every displaced
/// block in the file's open rewrite epoch until the 24 GiB scratch volume
/// read FULL (6,089 of 6,144 blocks SET, 55 referenced), and the
/// exhaustion landed on the WRITEBACK worker's flush of a staged partial
/// block — `flush_one_active_block`'s allocation, which had no KD-1.7 arm
/// — so the flush retried `StorageFull` for ever with the space it needed
/// parked in the epoch only a close could free; the file's next
/// `truncate` parked behind it and the mount was dead. The write path's
/// own StorageFull closes the epoch and retries (contract 5); the fsync
/// ladder flushes the staged blocks BEFORE its own epoch close (step 1
/// before step 2), so an fsync met the same wall. The law: the ONE
/// allocation act (`BackendRouter::allocate_placed_block`) answers a
/// `StorageFull` by closing every open rewrite epoch of the mount (the
/// parked supply is this mount's own) and retrying the mint once —
/// counted per closed epoch on `rewrite_shadow_pressure_closes` — so
/// every fresh-block site meets KD-1.7 where only the pipeline's arm
/// did. Shape: capacity 8, a 4-block file, blocks 0–1 rewritten twice
/// (four displaced keys parked, the volume exactly full, nothing refused
/// yet), then a 1 KiB write into a NEW block (staged, acked) and `fsync`:
/// the flush's mint meets `StorageFull`, closes the epoch (4 blocks
/// free), lands. RED on the unfixed tree: the fsync fails `ENOSPC` and
/// the epoch stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writeback_flush_that_meets_storage_full_closes_the_epoch_and_lands() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let h = make_harness_on("shadow_wb_enospc", backing, m, true, Some(8)).await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 21).await;
    let ba = &h.fs.router.block_allocator;

    // Two partial rewrites of blocks 0–1: each displaces the current
    // binding into the epoch (KD-1.6 parks it), none closes it (coverage
    // < file) — four parked keys, the volume exactly full.
    let (oe0, pb0) = (open_epochs(), parked_bytes());
    for seed in [31u8, 41] {
        write_at(&h, ino, 0, &pattern(2 * FBS as usize, seed)).await;
        quiesce(&h).await;
    }
    assert_eq!(open_epochs() - oe0, 1, "premise: one epoch open");
    assert!(parked_bytes() > pb0, "premise: displaced keys parked");
    assert_eq!(
        ba.free_supply_blocks(),
        0,
        "premise: the epoch's parked keys + the live map fill the volume exactly"
    );
    assert!(!ba.fresh_supply_latched(), "premise: nothing refused yet");

    // A 1 KiB write into a NEW block: staged and acked (the pre-ack
    // probe sees no latch).
    let tail = pattern(1024, 51);
    write_at(&h, ino, blocks * FBS, &tail).await;
    let refused0 = METRICS
        .write_fresh_block_enospc_refusals
        .load(Ordering::Relaxed);
    let (pc0, fb0) = (pressure_closes(), shadow_fallbacks());
    // The fsync's data step flushes the staged block: its mint meets
    // StorageFull with the supply parked in THIS file's epoch — the flush
    // closes it and lands.
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("the writeback flush closes the epoch that holds the supply and lands");
    assert_eq!(
        open_epochs() - oe0,
        0,
        "the pressure close retired the epoch"
    );
    assert_eq!(
        pressure_closes() - pc0,
        1,
        "the close is counted on the allocation act's own face"
    );
    assert_eq!(
        shadow_fallbacks() - fb0,
        0,
        "the flush unit has no pipeline arm — the act's close is the one that fired"
    );
    assert_eq!(
        METRICS
            .write_fresh_block_enospc_refusals
            .load(Ordering::Relaxed),
        refused0,
        "acked custody was never refused"
    );
    assert!(
        !ba.fresh_supply_latched(),
        "the landed allocation cleared the latch"
    );
    let got = read_all(&h, ino, (blocks * FBS) as usize + 1024).await;
    assert_eq!(
        &got[(blocks * FBS) as usize..],
        &tail[..],
        "the staged tail is durable"
    );
    assert_eq!(
        &got[..2 * FBS as usize],
        &pattern(2 * FBS as usize, 41)[..],
        "the last rewrite's bytes stand"
    );
    squeezefs::block_reclaim::set_elision_class_all(false);
}

// ---------------------------------------------------------------------------
// Contract 5c — the pressure close's LOCK LAW: never park on a held 3.5
// stripe (record §4.4cd).
// ---------------------------------------------------------------------------

/// A mint may run under a `BLOCK_FLUSH_LOCKS` guard (the writeback flush
/// unit) and the epoch's close takes the ino's `INODE_META_LOCKS` stripe —
/// so the pressure close acquires it in the NON-PARKING form: a held
/// stripe (a write or persist of that ino mid-flight, or the closing task's
/// own hold on a re-entry) SKIPS the epoch (`rewrite_shadow_pressure_close_
/// busy`), the `StorageFull` stands for that attempt, and the mint's own
/// retry ladder runs the close again once the stripe drops. Shape: the
/// contract-5b premise (one epoch, the volume exactly full), the ino's 3.5
/// stripe HELD by the test, one direct call of the allocation act — it must
/// return within the bound with `StorageFull`, `busy` +1, the epoch still
/// open; the stripe dropped, the next call closes (`pressure_closes` +1)
/// and lands. On the parking form the first call deadlocks against the
/// test's own guard and the bound fails it loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pressure_close_never_parks_on_a_held_meta_stripe() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let h = make_harness_on("shadow_busy_stripe", backing, m, true, Some(8)).await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 21).await;
    let ba = &h.fs.router.block_allocator;

    let oe0 = open_epochs();
    for seed in [31u8, 41] {
        write_at(&h, ino, 0, &pattern(2 * FBS as usize, seed)).await;
        quiesce(&h).await;
    }
    assert_eq!(open_epochs() - oe0, 1, "premise: one epoch open");
    assert_eq!(
        ba.free_supply_blocks(),
        0,
        "premise: the volume is exactly full"
    );

    let (pc0, busy0) = (pressure_closes(), pressure_close_busy());
    let held = squeezefs::routing::meta_lock_acquire(ino).await;
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        h.fs.router.backend_router.allocate_placed_block(),
    )
    .await
    .expect("the act never parks behind a held 3.5 stripe (the parking form deadlocks here)");
    let verdict = res.as_ref().map(|_| "landed").map_err(|e| e.to_string());
    assert!(
        matches!(&res, Err(squeezefs::error::SqueezefsError::Io(io))
            if io.kind() == std::io::ErrorKind::StorageFull),
        "with the epoch's stripe held the refusal stands for this attempt: {verdict:?}"
    );
    assert_eq!(
        pressure_close_busy() - busy0,
        1,
        "the held stripe's epoch was skipped, counted"
    );
    assert_eq!(
        pressure_closes() - pc0,
        0,
        "nothing closed under the held stripe"
    );
    assert_eq!(open_epochs() - oe0, 1, "the epoch stays registered");
    drop(held);

    let (_, allocator, _, offset) =
        h.fs.router
            .backend_router
            .allocate_placed_block()
            .await
            .expect("the stripe dropped: the act closes the epoch and the mint lands");
    assert_eq!(pressure_closes() - pc0, 1, "the retry's close is counted");
    assert_eq!(open_epochs() - oe0, 0, "the epoch closed");
    assert!(
        !ba.fresh_supply_latched(),
        "the landed allocation cleared the latch"
    );
    // The probe's block never enters a map: hand it back.
    let _ = allocator.abandon_unpublished_offset(offset).await;
    squeezefs::block_reclaim::set_elision_class_all(false);
}

// ---------------------------------------------------------------------------
// Contract 5d — an fsync whose flush met a Busy skip lands (record §4.4cd,
// review round 1 Issue 4).
// ---------------------------------------------------------------------------

/// The pressure close skips an epoch whose ino's level-3.5 stripe a
/// sibling holds (contract 5c), and the fsync ladder's step-1 flush had
/// no retry below it — the skip surfaced as `ENOSPC` to `fsync(2)` on a
/// volume whose free space was parked in this mount's own epoch. Now the
/// ladder answers a `StorageFull` from its data-flush step by running its
/// own epoch close EARLY (behind a data barrier — DUR-1) and retrying the
/// step once (`fsync_flush_enospc_retries`). Shape: the contract-5b
/// premise, the ino's stripe HELD by a sibling task that releases it
/// 300 ms into the fsync — the flush's mint meets the wall (the hook reads
/// the stripe `Busy`), the ladder's own PARKING close waits the sibling
/// out, closes, and the retried flush lands. RED without the arm:
/// `Errno(28)` at the fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_fsync_whose_flush_met_a_busy_skip_closes_its_own_epoch_and_lands() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let h = make_harness_on("shadow_fsync_busy", backing, m, true, Some(8)).await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 21).await;
    let ba = &h.fs.router.block_allocator;

    let oe0 = open_epochs();
    for seed in [31u8, 41] {
        write_at(&h, ino, 0, &pattern(2 * FBS as usize, seed)).await;
        quiesce(&h).await;
    }
    assert_eq!(open_epochs() - oe0, 1, "premise: one epoch open");
    assert_eq!(
        ba.free_supply_blocks(),
        0,
        "premise: the volume is exactly full"
    );
    let tail = pattern(1024, 51);
    write_at(&h, ino, blocks * FBS, &tail).await;

    let (pc0, busy0, retries0) = (
        pressure_closes(),
        pressure_close_busy(),
        METRICS.fsync_flush_enospc_retries.load(Ordering::Relaxed),
    );
    // The sibling: holds the epoch ino's stripe across the fsync's first
    // mint and releases it 300 ms later.
    let held = squeezefs::routing::meta_lock_acquire(ino).await;
    let sibling = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        drop(held);
    });
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        h.fs.fsync(h.req, ino, 0, false),
    )
    .await
    .expect("the fsync's own close waits the sibling out and returns");
    sibling.await.unwrap();
    res.expect("the fsync lands once the sibling's stripe drops: its own close frees the supply");
    assert_eq!(
        METRICS.fsync_flush_enospc_retries.load(Ordering::Relaxed) - retries0,
        1,
        "the ladder ran its early close + one retry"
    );
    assert!(
        pressure_close_busy() - busy0 >= 1,
        "the hook read the held stripe Busy at least once"
    );
    assert_eq!(open_epochs() - oe0, 0, "the epoch closed");
    assert_eq!(
        pressure_closes() - pc0,
        0,
        "the ladder's own PARKING close retired the epoch, so the retry's mint found nothing \
         parked and the act's face never fired"
    );
    let got = read_all(&h, ino, (blocks * FBS) as usize + 1024).await;
    assert_eq!(
        &got[(blocks * FBS) as usize..],
        &tail[..],
        "the staged tail is durable"
    );
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
