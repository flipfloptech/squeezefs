//! Rebind-starvation → EIO repro (field capture 2026-08-04, daemon
//! `3049b6d`, 6.19.14-sqz, nvme-tcp fabric): a 32-job fio read phase
//! racing the prior write phase's writeback/fold promotion backlog
//! produced hundreds of `FUSE Read error: "block N of inode_M did not
//! settle after 24 binding rebinds"` — every fio `Input/output error` in
//! the run. The same offsets read perfectly once churn drained.
//!
//! The mechanism: `get_block_for_index`'s serve loop snapshots the
//! binding, fetches, then rechecks the binding AFTER the bytes are in
//! hand (the settled reused-key stale-fill proof). Under sustained LEGAL
//! displacement — every generation durably published before the old key
//! is freed — a reader whose fetch window always overlaps one
//! displacement loses every attempt. Even the VL8-item-7 escalated
//! attempts lose forever: they hold the block stripe across ONE fetch of
//! a binding snapshot taken OUTSIDE the guard, so the reader chases the
//! writer one generation behind, and after `MAX_REBINDS` losses the exit
//! converts reader starvation under legal writer churn into EIO on a
//! pure read. "Load-dependent hangs are first-class product bugs" —
//! this is the same law wearing an EIO.
//!
//! Contracts under test:
//!
//! - **Stripe-free displacement storm** (the `write_striped` /
//!   `copy_file_range` legal publish shape: allocate → device write →
//!   `merge_block_mappings` under `INODE_META_LOCKS` only → free
//!   displaced): a concurrent reader with the fetch→recheck window
//!   widened by the registered `TEST_BINDING_RECHECK_DELAY_MS` seam must
//!   ALWAYS return bytes that were the block's current content at some
//!   point during the read — one of the legally-published generations,
//!   untorn — never the rebind-exhaustion EIO.
//! - **Stripe-holding displacement storm** (the field shape — the
//!   writeback/fold/pipeline promotion publish holds
//!   `BLOCK_FLUSH_LOCKS`): same guarantee, through the raw primitive AND
//!   the FUSE read handler.
//! - **The honest EIO survives**: a fetch failure on a binding the
//!   CURRENT map still holds (a `damaged:` fsck-quarantined mapping) is a
//!   real error on real current state and must stay a loud EIO — the
//!   refusal the rw5a/staging-generation suites call "correct" keeps its
//!   meaning; only STARVATION was split out of that exit.
//!
//! The seam is the schedule control (no sleeps for synchronization): a
//! 25 ms fetch→recheck window guarantees every ladder attempt overlaps a
//! displacement from the storm task, deterministically.

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
use squeezefs::routing::{clean_block_key, BlockMapOp, DataRouter, LayoutFlip};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Sandbox block size (the extent_patch_tests quantum): the starvation
/// anatomy is block-size-relative — 64 KiB here, 4 MiB in the field
/// capture; same ladder, same exit.
const BS: u64 = 64 * 1024;

/// Seam widths — the schedule control. The stripe-HOLDING storm (face 2)
/// alternates with the reader on the block stripe, so a 25 ms window is
/// already deterministic (the parked writer displaces in EVERY guard
/// gap). The stripe-FREE storm (face 1) lands displacements on its own
/// clock — one merge+commit+free cycle, measured ≈ 25 ms worst on the
/// file-backed sandbox — so its window is 4× that cycle: every ladder
/// attempt provably overlaps at least one displacement.
const SEAM_MS: u64 = 25;
const SEAM_STRIPE_FREE_MS: u64 = 100;

/// The victim block index (interior — never the EOF tail).
const BLK: u32 = 2;

/// Process-global METRICS + seam: serialize tests (the extent_patch_tests
/// pattern; the cargo gate runs `--test-threads=1`, this makes the suite
/// order-robust on its own too).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// RAII seam reset — a panicking test must not leak a 25 ms delay into
/// every later binding recheck in this shared-process binary.
struct SeamGuard;
impl SeamGuard {
    fn arm(ms: u64) -> Self {
        squeezefs::routing::TEST_BINDING_RECHECK_DELAY_MS.store(ms, Ordering::Relaxed);
        SeamGuard
    }
}
impl Drop for SeamGuard {
    fn drop(&mut self) {
        squeezefs::routing::TEST_BINDING_RECHECK_DELAY_MS.store(0, Ordering::Relaxed);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn open_fs(
    tag: &str,
    meta_path: &std::path::Path,
    backing_path: &std::path::Path,
    staging: &std::path::Path,
) -> SqueezefsFilesystem {
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_path.to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta_path).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    fs
}

async fn format_meta(path: &std::path::Path, uuid: [u8; 16]) {
    path.metadata().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    format_meta(m.path(), uuid).await;
    let s = tempdir().unwrap();
    let fs = open_fs(alloc_ns, m.path(), b.path(), s.path()).await;
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

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let written =
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
        .written;
    assert_eq!(written as usize, data.len(), "short write at {off}");
}

fn pat(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

/// Uniform generation image: byte `gen_tag(g)` × BS — uniformity is the
/// untorn proof, the value identifies the generation.
fn gen_tag(g: u64) -> u8 {
    (g % 200) as u8 + 1
}

/// Drop every RAM/NVMe read tier for `ino` so later reads are device-honest.
async fn purge_read_tiers(h: &H, ino: u64) {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
        if let Some(bm) = m.block_map.as_ref() {
            for bk in bm.values() {
                h.fs.router.cache.purge_block_key(bk);
            }
        }
    }
}

/// A durable, cold, freshly-striped 4-block fixture.
async fn durable_striped(h: &H, name: &str) -> (u64, Vec<u8>) {
    let ino = create(h, name).await;
    let base = pat((4 * BS) as usize, 0x33);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    purge_read_tiers(h, ino).await;
    (ino, base)
}

/// One legal displacement of block [`BLK`]: allocate a fresh key, write
/// generation `g`'s image durably, publish the binding through the §5.3
/// one-merge primitive, then free the displaced key — exactly the
/// `write_striped` publish order (device bytes strictly before the map,
/// frees strictly after).
async fn displace_once(h: &H, ino: u64, g: u64) {
    let (be_id, alloc, writer) = h.fs.router.backend_router.get_active_backend().unwrap();
    let off = alloc.allocate_block().await.unwrap();
    let img =
        h.fs.router
            .get_crypto()
            .process_write_async(bytes::Bytes::from(vec![gen_tag(g); BS as usize]))
            .await
            .unwrap();
    writer.write_block(off, img).await.unwrap();
    alloc.publish_block(off);
    let key = h.fs.router.backend_router.persist_block_key(&be_id, off);
    // The fresh key's offset may REUSE a previously-displaced offset: purge
    // any dead-incarnation tier entry before the map names it (the
    // write_striped discipline).
    h.fs.router.cache.purge_block_key(&key);
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let entries = [(BLK, key)];
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                4 * BS,
                LayoutFlip::KeepLayout,
                token,
            )
            .await
            .unwrap();
    for bk in &displaced {
        let _ =
            h.fs.router
                .backend_router
                .free_block(&clean_block_key(bk))
                .await;
    }
}

/// The legality verdict: served bytes must be the base block image or ONE
/// generation's uniform image published so far — untorn, never foreign.
fn assert_legal_generation(got: &[u8], base_block: &[u8], gens_published: u64, what: &str) {
    assert_eq!(got.len(), base_block.len(), "{what}: whole-block length");
    if got == base_block {
        return;
    }
    let v = got[0];
    assert!(
        got.iter().all(|&x| x == v),
        "{what}: torn serve — bytes are neither the base image nor one \
         uniform generation (first byte {v:#04x})"
    );
    let legal = (1..=gens_published).any(|g| gen_tag(g) == v);
    assert!(
        legal,
        "{what}: uniform byte {v:#04x} is not a published generation \
         (gens so far: {gens_published})"
    );
}

/// Face 1 — the STRIPE-FREE displacement storm (the `write_striped` /
/// `copy_file_range` publish shape: `merge_block_mappings` serializes on
/// `INODE_META_LOCKS` only, never the block stripe). With the seam
/// widening every fetch→recheck window past the storm period, every
/// ladder attempt — fast AND stripe-escalated — loses, and the current
/// exit converts that starvation into EIO on a pure read. Reads must
/// always make progress and serve a legally-published generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_survives_stripe_free_displacement_storm_no_rebind_eio() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rebind-starve-01", "rbs_ns_1").await);
    let (ino, base) = durable_striped(&h, "starved.dat").await;
    let base_block = &base[(BLK as u64 * BS) as usize..((BLK as u64 + 1) * BS) as usize];
    let file_path = squeezefs::keys::inode_path(ino);

    let stop = Arc::new(AtomicBool::new(false));
    let gens = Arc::new(AtomicU64::new(0));
    let storm = {
        let stop = stop.clone();
        let gens = gens.clone();
        let h = h.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let g = gens.load(Ordering::Relaxed) + 1;
                displace_once(&h, ino, g).await;
                gens.store(g, Ordering::Release);
                tokio::task::yield_now().await;
            }
        })
    };

    let _seam = SeamGuard::arm(SEAM_STRIPE_FREE_MS);
    let escalations_before = METRICS.stale_binding_escalations.load(Ordering::Relaxed);
    // Engagement rounds: a fixed 3-round window flaked under load (a
    // throttled box — Tctl ≥ 100 °C soak, ~2/8 — can stall the STORM's
    // alloc/free cycle past the seam window inside a round, letting a
    // ladder attempt see a stable binding and serve early; observed on
    // BOTH read-mostly arms, 2026-08-08, so it is a venue sensitivity of
    // the storm, not a product change). Rounds retry — bounded — until
    // the settle arm engages; every round still pins the never-EIO and
    // legal-generation halves verbatim.
    let mut engaged = false;
    for round in 0..12u32 {
        purge_read_tiers(&h, ino).await;
        // The read-path posture: escalate_contended = true (no caller-held
        // stripe) — exactly what the FUSE/ipc read handlers pass.
        let val =
            h.fs.router
                .get_block_for_index(&file_path, BLK, None, false, true)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "round {round}: pure read must never EIO because writers \
                         are busy (the field rebind-exhaustion signature), got {e:?}"
                    )
                })
                .expect("block {BLK} is mapped, never a hole");
        let published = gens.load(Ordering::Acquire);
        assert_legal_generation(&val, base_block, published, &format!("round {round}"));
        if round >= 2
            && METRICS.stale_binding_escalations.load(Ordering::Relaxed) > escalations_before
        {
            engaged = true;
            break;
        }
    }
    assert!(
        engaged,
        "the settle arm is the engagement the storm must have forced \
         within 12 seam-widened rounds (escalations stayed at \
         {escalations_before})"
    );

    stop.store(true, Ordering::Release);
    storm.await.unwrap();
}

/// Face 2 — the STRIPE-HOLDING displacement storm (the field shape: the
/// writeback/fold/pipeline promotion publish holds `BLOCK_FLUSH_LOCKS`
/// across its merge). The current escalated attempts hold the stripe
/// across a fetch of a binding snapshot taken OUTSIDE the guard, so the
/// reader chases one generation behind forever. Reads — through the raw
/// primitive AND the FUSE read handler — must make progress and serve a
/// legal generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_survives_stripe_holding_promotion_storm_no_rebind_eio() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rebind-starve-02", "rbs_ns_2").await);
    let (ino, base) = durable_striped(&h, "promoted.dat").await;
    let base_block = &base[(BLK as u64 * BS) as usize..((BLK as u64 + 1) * BS) as usize];
    let file_path = squeezefs::keys::inode_path(ino);

    let stop = Arc::new(AtomicBool::new(false));
    let gens = Arc::new(AtomicU64::new(0));
    let storm = {
        let stop = stop.clone();
        let gens = gens.clone();
        let h = h.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let g = gens.load(Ordering::Relaxed) + 1;
                // The promotion/writeback publish shape: the whole
                // displacement runs under the block's stripe.
                let guard = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS
                    .get_lock(ino, BLK)
                    .lock()
                    .await;
                displace_once(&h, ino, g).await;
                drop(guard);
                gens.store(g, Ordering::Release);
                tokio::task::yield_now().await;
            }
        })
    };

    let _seam = SeamGuard::arm(SEAM_MS);
    for round in 0..2u32 {
        purge_read_tiers(&h, ino).await;
        let val =
            h.fs.router
                .get_block_for_index(&file_path, BLK, None, false, true)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "round {round} (primitive): pure read must never EIO \
                         because writers are busy, got {e:?}"
                    )
                })
                .expect("block {BLK} is mapped, never a hole");
        let published = gens.load(Ordering::Acquire);
        assert_legal_generation(
            &val,
            base_block,
            published,
            &format!("round {round} (primitive)"),
        );

        // The handler face: an 8 KiB read inside the stormed block must
        // serve an untorn slice of a legal generation.
        let off = BLK as u64 * BS + 16384;
        let reply =
            h.fs.read(h.req, ino, 0, off, 8192, 0)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "round {round} (handler): storm-raced read must make \
                         progress, got {e:?}"
                    )
                });
        let got = &reply.data[..];
        assert_eq!(got.len(), 8192, "round {round} (handler): length");
        let published = gens.load(Ordering::Acquire);
        let base_slice = &base[off as usize..off as usize + 8192];
        if got != base_slice {
            let v = got[0];
            assert!(
                got.iter().all(|&x| x == v),
                "round {round} (handler): torn serve (first byte {v:#04x})"
            );
            assert!(
                (1..=published).any(|g| gen_tag(g) == v),
                "round {round} (handler): uniform byte {v:#04x} is not a \
                 published generation (gens so far: {published})"
            );
        }
    }

    stop.store(true, Ordering::Release);
    storm.await.unwrap();
}

/// Face 3 — the honest exit survives: a fetch failure on a binding the
/// CURRENT map still holds (here a `damaged:` fsck-quarantine mapping —
/// the §5.6a EIO contract) must stay a loud error through the ladder AND
/// through any escalation: it is a real error on real current state, not
/// starvation. This is the refusal rw5a/staging-generation call
/// "correct", pinned green on both sides of the starvation fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_error_on_current_binding_stays_loud_eio() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rebind-starve-03", "rbs_ns_3").await);
    let (ino, _base) = durable_striped(&h, "damaged.dat").await;
    let file_path = squeezefs::keys::inode_path(ino);

    // Quarantine block BLK's mapping (the fsck C-class repair shape): the
    // CURRENT map binds BLK to a key no fetch may serve.
    let meta = h.fs.router.fetch_metadata(&file_path).await.unwrap();
    let cur = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&BLK).cloned())
        .expect("block mapped");
    let damaged = format!("{}{cur}", squeezefs::routing::DAMAGED_MAPPING_PREFIX);
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let entries = [(BLK, damaged)];
    h.fs.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&entries),
            4 * BS,
            LayoutFlip::KeepLayout,
            token,
        )
        .await
        .unwrap();
    purge_read_tiers(&h, ino).await;

    let err = match h
        .fs
        .router
        .get_block_for_index(&file_path, BLK, None, false, true)
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("a damaged CURRENT binding must fail loud, never serve"),
    };
    let msg = format!("{err:?}");
    assert!(
        !msg.contains("did not settle"),
        "the damaged-mapping EIO is the propagate arm (current binding), \
         never the exhaustion exit: {msg}"
    );
}

/// Face 4 — the STRIPE-HOLDING SEED fetch (MW rung-18 residual (a); the
/// s11-subblock live finding, `.benchmarks/2026-08-17-s11-zeros-interleave-fix.md`
/// §"The s11-subblock priced leg"): under two-holder same-block extent
/// churn the AUTHORITY assembler's fold exhausted the NON-escalating
/// 24-rebind ladder and surfaced `EIO "block 0 of inode_2 did not settle
/// after 24 binding rebinds"` on a co-writer's `FlushExtents` — an
/// fsync-path data-plane EIO under LEGAL publish churn, the 2026-08-04
/// field starvation wearing the fold's clothes.
///
/// The fold's seed fetch (`fetch_seed_image` — the item-B deferred RMW
/// base) runs UNDER the fold's held `BLOCK_FLUSH_LOCKS` stripe, so the
/// read path's settle arm — which ACQUIRES (3) — was structurally
/// unavailable to it, and the ladder's non-escalating exhaustion claimed
/// "genuinely broken binding". That claim is falsified by the
/// (3.5)-only publisher class (`write_striped` promotions,
/// `copy_file_range`, the Lever-B publish conveyor, a concurrent
/// assembler publish of a NEIGHBOR representation): legal churn that
/// never takes this block's stripe can starve a stripe-holding seed
/// fetch forever. The law pinned: exhaustion on the stripe-held seed
/// posture hands off to the CALLER-STRIPE settle arm — one
/// resolve-then-fetch under `INODE_META_LOCKS` (3.5) alone, the caller's
/// held (3) completing the write path's own extended order — counted in
/// `seed_settle_escalations`, never EIO.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fold_seed_survives_stripe_free_displacement_storm_no_rebind_eio() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rebind-starve-04", "rbs_ns_4").await);
    let (ino, _base) = durable_striped(&h, "folded.dat").await;

    let stop = Arc::new(AtomicBool::new(false));
    let gens = Arc::new(AtomicU64::new(0));
    let storm = {
        let stop = stop.clone();
        let gens = gens.clone();
        let h = h.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let g = gens.load(Ordering::Relaxed) + 1;
                // The (3.5)-only publisher class: displaces the fold's
                // seed binding without ever touching the block stripe
                // the fold holds.
                displace_once(&h, ino, g).await;
                gens.store(g, Ordering::Release);
                tokio::task::yield_now().await;
            }
        })
    };

    let _seam = SeamGuard::arm(SEAM_STRIPE_FREE_MS);
    let before = METRICS.seed_settle_escalations.load(Ordering::Relaxed);
    // Engagement rounds (the face-1 venue-sensitivity note applies
    // verbatim): every round pins the never-EIO half; the loop retries —
    // bounded — until the caller-stripe settle arm provably engaged.
    let mut engaged = false;
    for round in 0..12u32 {
        // Park a fresh extent overlay with a DEFERRED seed in the stormed
        // block (an unaligned sub-block span is patch-ineligible — the W2
        // park; write-path seed fetches are deleted, so the seed defers
        // to the fold).
        write_at(&h, ino, BLK as u64 * BS + 1037, &pat(100, 0x5A)).await;
        let ran = h.fs.fold_extent_block(ino, BLK).await.unwrap_or_else(|e| {
            panic!(
                "round {round}: the fold's deferred seed fetch must never \
                     EIO because (3.5)-only publishers are busy (the \
                     s11-subblock FlushExtents EIO signature), got {e:?}"
            )
        });
        assert!(ran, "round {round}: the parked extent state must fold");
        if round >= 2 && METRICS.seed_settle_escalations.load(Ordering::Relaxed) > before {
            engaged = true;
            break;
        }
    }
    assert!(
        engaged,
        "the caller-stripe settle arm is the engagement the storm must \
         have forced within 12 seam-widened rounds (seed_settle_escalations \
         stayed at {before})"
    );

    stop.store(true, Ordering::Release);
    storm.await.unwrap();
}
