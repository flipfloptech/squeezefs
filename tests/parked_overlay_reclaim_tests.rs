//! Parked-overlay ledger closure at inode reclaim — the 2026-08-05 field
//! leak (`parked_full_buffer_bytes = 230.3 GiB`, flat at idle, R5 Red,
//! box RSS ≈ gauge — REAL memory).
//!
//! The convicted edge: `unlink → FORGET → reclaim_orphaned_batch` tears
//! down the DATA plane through `DataRouter::delete_file`, which removes
//! the STAGED overlay families (`active_block:` / `active_block_ext:`
//! ring records) and frees the mapped blocks — but the FUSE-layer RAM
//! overlay map (`active_block_buffers`, whose `ActiveBlockBuf` values
//! RAII-charge `parked_full_buffer_bytes` / `parked_extent_bytes`) is
//! never touched. A block parked at delete time (partial coverage, or a
//! close-time background flush that lost the race to `rm`) is orphaned
//! forever, keyed by a dead ino:
//!
//! - the R5 Red drain (`drain_parked_toward`) cannot retire it — its
//!   flush errors NotFound on the destroyed meta (the deferred-seed
//!   fetch), the map length never shrinks, and the no-forward-progress
//!   exit fires (the field's "flat across 60 s");
//! - fsync/close never come again (the file is unlinked);
//! - the writeback worker never sees it (RAM custody is not staged).
//!
//! The law pinned here: **a reclaimed ino's parked overlays retire with
//! the inode — quiesce converges the parked gauges to their baseline.**
//! This is the verified-orphan-discard arm of FIND-M11-A ("reclaimed-ino
//! NotFound units discard their orphans") applied to RAM custody: the
//! admitted set is `nlink == 0`, FORGET'd, not open — discarding is the
//! RAM twin of `delete_file`'s staged-record removal, never a loss of
//! live acked data. The scoping control pins the other half: reclaim of
//! ino A must not disturb ino B's live parked custody.
//!
//! RED against integrate/zcrx-wave 3318c216: both tests fail — the
//! gauges stay charged and the gate count never returns to baseline.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536; // striped at small sizes; keeps ledger deltas exact
const HALF: u64 = BS / 2;

/// Process-global METRICS deltas + knob overrides: suite serializes
/// (house pattern, `write_through_coverage_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the adaptive pipeline default on scope exit (knob hygiene,
/// `write_pipeline_tests` pattern).
struct OverrideGuard;
impl Drop for OverrideGuard {
    fn drop(&mut self) {
        squeezefs::write_pipeline::set_depth_override(None);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Sub-block overwrites at this downscaled BS would be W1
    // patch-eligible (production shapes are patch-oversize) — pin the
    // patch path OFF so the parked-overlay machinery under test engages
    // (same reason as `write_through_coverage_tests`).
    squeezefs::fuse_client::set_patch_max_bytes(0);
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
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
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
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x9A47_ED00_2026_0805,
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

/// CREATE + immediate RELEASE (clean-handle fast path — no background
/// flush is scheduled, so the parked custody built afterwards drains
/// through NOTHING but the edge under test). Reclaim refuses inos with
/// live open handles, exactly like userspace close-before-rm.
async fn create_closed(h: &H, name: &str) -> u64 {
    let created =
        h.fs.create(
            h.req,
            1,
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

#[derive(Clone, Copy, Debug)]
struct ParkedLedger {
    full: u64,
    extent: u64,
    gate: usize,
}

fn parked(h: &H) -> ParkedLedger {
    ParkedLedger {
        full: METRICS.parked_full_buffer_bytes.load(Ordering::Relaxed),
        extent: METRICS.parked_extent_bytes.load(Ordering::Relaxed),
        gate: h.fs.parked_overlay_gate_count(),
    }
}

/// A durable striped file whose covering writes retired synchronously
/// (the depth-override-0 lever pins the pipeline inline, so the fixture
/// leaves NOTHING parked), plus one partial-coverage full-repr park and
/// one small-write extent park — the two W2 gauge classes.
async fn striped_with_parked_custody(h: &H, name: &str, tag: u8) -> u64 {
    let ino = create_closed(h, name).await;
    write_at(h, ino, 0, &pattern(6 * BS as usize, tag)).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");

    let before = parked(h);
    // Full-repr park: a half-block overwrite (not `small`, patch off) —
    // partial coverage keeps it parked, item-B deferred.
    write_at(h, ino, 2 * BS + HALF, &pattern(HALF as usize, tag ^ 0x11)).await;
    let mid = parked(h);
    assert_eq!(
        mid.full - before.full,
        BS,
        "fixture engagement: the half-block overwrite must park a \
         full-repr ActiveBlockBuf (one block-size gauge charge)"
    );
    // Extent park: a small (< bs/4) overwrite of another block.
    write_at(h, ino, 4 * BS + 4096, &pattern(2048, tag ^ 0x22)).await;
    let after = parked(h);
    assert!(
        after.extent > mid.extent,
        "fixture engagement: the 2 KiB overwrite must park as a W2 \
         extent overlay (parked_extent_bytes {} -> {})",
        mid.extent,
        after.extent
    );
    assert_eq!(
        after.gate - before.gate,
        2,
        "fixture engagement: exactly two overlays parked"
    );
    ino
}

/// THE LEAK LAW (red on integrate/zcrx-wave): unlink + FORGET-driven
/// reclaim of an ino with parked overlays must retire them — the parked
/// gauges and the O(1) overlay gate return to their pre-file baseline,
/// and the custody memory is freed (RAII: the gauge IS the buffer's
/// lifetime, which is why the field box's RSS matched the gauge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlink_reclaim_retires_parked_overlays_and_closes_the_gauge() {
    let _s = serial().await;
    let _o = OverrideGuard;
    squeezefs::write_pipeline::set_depth_override(Some(0));
    let h = make(*b"parked-leak-red!", "parked_leak_a").await;

    let base = parked(&h);
    let ino = striped_with_parked_custody(&h, "victim", 0xA1).await;

    // rm: unlink, then the FORGET-driven reclaim (what queue_reclaim's
    // worker runs on a live mount).
    h.fs.unlink(h.req, 1, OsStr::new("victim")).await.unwrap();
    h.fs.reclaim_orphaned_batch(vec![ino]).await;

    // Reclaim engaged: the inode record is destroyed (fetch_metadata
    // would synthesize a default — the backend getattr is the honest
    // probe).
    assert!(
        h.fs.meta_backend
            .as_ref()
            .unwrap()
            .getattr(ino)
            .await
            .is_err(),
        "sanity: reclaim must have destroyed the inode record"
    );

    // The ledger-closure law.
    let end = parked(&h);
    assert_eq!(
        end.full, base.full,
        "parked_full_buffer_bytes must return to baseline after the \
         ino's reclaim (leaked full-repr overlay = the 230 GiB field \
         class: orphaned custody keyed by a dead ino)"
    );
    assert_eq!(
        end.extent, base.extent,
        "parked_extent_bytes must return to baseline after the ino's \
         reclaim (the W2 extent-overlay face of the same missed edge)"
    );
    assert_eq!(
        end.gate, base.gate,
        "the O(1) parked-overlay gate must return to baseline — a \
         nonzero residue means dead-ino entries still occupy the map \
         (read-path probes pay for them forever)"
    );
    assert_eq!(
        h.fs.parked_buffer_bytes(),
        base.full + base.extent,
        "quiesce must converge the parked byte gauge to its baseline"
    );
}

/// The never-lossy scoping control: reclaiming ino A must retire ONLY
/// A's overlays — B's live parked custody (acked, un-flushed bytes)
/// survives byte-exact and still folds/flushes durably afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaim_scopes_to_its_ino_and_keeps_live_custody() {
    let _s = serial().await;
    let _o = OverrideGuard;
    squeezefs::write_pipeline::set_depth_override(Some(0));
    let h = make(*b"parked-leak-scop", "parked_leak_b").await;

    let base = parked(&h);
    let doomed = striped_with_parked_custody(&h, "doomed", 0xB1).await;
    let live = create_closed(&h, "live").await;
    write_at(&h, live, 0, &pattern(6 * BS as usize, 0xC2)).await;
    fsync(&h, live).await;
    // B's live parked custody: a half-block overwrite of block 1.
    let live_patch = pattern(HALF as usize, 0xC3);
    write_at(&h, live, BS, &live_patch).await;
    let staged = parked(&h);
    assert_eq!(
        staged.gate - base.gate,
        3,
        "fixture: two doomed overlays + one live overlay parked"
    );

    h.fs.unlink(h.req, 1, OsStr::new("doomed")).await.unwrap();
    h.fs.reclaim_orphaned_batch(vec![doomed]).await;

    let end = parked(&h);
    assert_eq!(
        end.full - base.full,
        BS,
        "exactly the live ino's full-repr park must survive the \
         neighbor's reclaim (never-lossy: live acked custody is not \
         collateral)"
    );
    assert_eq!(
        end.gate - base.gate,
        1,
        "exactly one (live) overlay must remain parked"
    );

    // The survivor's custody is intact: flush it durably and read back.
    fsync(&h, live).await;
    let got =
        h.fs.read(h.req, live, 0, BS, HALF as u32, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(
        got, live_patch,
        "the live ino's parked bytes must survive the neighbor's \
         reclaim and flush byte-exact"
    );
    let drained = parked(&h);
    assert_eq!(
        drained.full, base.full,
        "after the live ino's own fsync the gauge closes fully"
    );
}
