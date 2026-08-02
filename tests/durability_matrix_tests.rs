//! **The per-layout durability matrix** (pre-RC engineering spec §1
//! DUR-2 acceptance; execution plan §6.1 "the acceptance matrix").
//!
//! One parameterized suite over every write shape the product can put on
//! the data device, driven by the TEST-1 data-device power-cut harness
//! (`squeezefs::dev_power_cut`) — the first tests in this tree that can
//! observe volatile-cache loss on the DATA plane at all.
//!
//! **Leg A — fsync-then-power-cut**: every acked byte survives, and the
//! no-orphaned-mapping law holds (every block key the durable map names
//! reads back the bytes that were written).
//!
//! **Leg B — power-cut mid-write**: with no `fsync`, the cut may lose the
//! new bytes, but what is served afterwards must be a WHOLE image (the
//! pre-write one or the post-write one), never a torn mix of the two.
//!
//! Row coverage and honesty notes:
//!
//! | Row | What it exercises | Data-device exposure |
//! |---|---|---|
//! | `Inline` | payload in the metadata plane (`inline_data:`) | none — control row: a data-device cut must be a no-op |
//! | `Staged` | staging-segment payload (`file_id`) | none — durability rides the staging file, not the block device |
//! | `StripedWriteThrough` | coverage-complete block, write-through DMA | full |
//! | `StripedViaStaging` | partially-covered block — DUR-1's escalation | full |
//! | `W1Patch` | sole-owner in-place sub-block DMA | full |
//! | `W2Fold` | parked extent overlay folded at fsync | full |
//! | `InplaceOverwrite` | `SQUEEZEFS_INPLACE_OVERWRITE=1` same-key rewrite | full |
//! | `RewriteShadowClose` | rewrite epoch closed by fsync | full |
//! | IPC ring write | §5.5.2 placed sever | **not in-process** — the ring write lands in the same `ActiveBlockBuf` as the striped rows and shares their flush path verbatim; the end-to-end leg is `tests/run_preload_gate.sh` (leg 2), which needs a real mount + the shim |
//!
//! The two zero-exposure rows are kept deliberately: "this layout never
//! touches the data device" is a durability claim, and a regression that
//! silently routed inline or staged payloads through the block device
//! would show up here as a lost byte.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use rstest::rstest;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dev_power_cut;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

/// Fixed KV hash seed for every fixture in this suite.
const MATRIX_HASH_SEED: u64 = 0xD0DE_BEEF_0000_0002;

/// The suite drives process-global levers (patch cap, in-place
/// overwrite) and the process-global harness — rows serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Inline,
    Staged,
    StripedWriteThrough,
    StripedViaStaging,
    W1Patch,
    W2Fold,
    InplaceOverwrite,
    RewriteShadowClose,
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    dev_path: String,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

impl Drop for H {
    fn drop(&mut self) {
        dev_power_cut::clear_faults();
    }
}

async fn make(uuid: [u8; 16], ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // The read-lane hold would serve a completed fill from RAM after the
    // cut — this suite must read the DEVICE.
    std::env::set_var("SQUEEZEFS_READ_LANE", "0");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let dev_path = b.path().to_str().unwrap().to_string();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(&dev_path));
    let ba = Arc::new(BlockAllocator::new(ns).await.unwrap());
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
            hash_seed: MATRIX_HASH_SEED,
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
    };
    H {
        fs,
        req,
        dev_path,
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
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ tag | 1).collect()
}

/// Drop every RAM/segment tier that could answer a read from something
/// other than the data device — the cut only reverts the device.
async fn purge_tiers(h: &H, ino: u64) {
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

/// Per-row setup: returns `(ino, whole-file image, (write offset, len))`
/// for the shape under test, with the base image already durable.
async fn setup_row(h: &H, row: Row) -> (u64, Vec<u8>, (u64, usize)) {
    match row {
        Row::Inline => {
            let ino = create(h, "inline.bin").await;
            let img = pattern(2048, 0x11);
            (ino, img, (0, 2048))
        }
        Row::Staged => {
            let ino = create(h, "staged.bin").await;
            let img = pattern(16 * 1024, 0x22);
            (ino, img, (0, 16 * 1024))
        }
        Row::StripedWriteThrough => {
            let ino = create(h, "wt.bin").await;
            let img = pattern(2 * BS as usize, 0x33);
            (ino, img, (0, 2 * BS as usize))
        }
        Row::StripedViaStaging | Row::W2Fold | Row::W1Patch | Row::InplaceOverwrite => {
            // A durable striped base image; the row's own write lands on
            // top of it in `row_write`.
            let ino = create(h, "striped.bin").await;
            let img = pattern(4 * BS as usize, 0x44);
            write_at(h, ino, 0, &img).await;
            fsync(h, ino).await;
            (ino, img, (0, 4 * BS as usize))
        }
        Row::RewriteShadowClose => {
            let ino = create(h, "rewrite.bin").await;
            let img = pattern(2 * BS as usize, 0x55);
            write_at(h, ino, 0, &img).await;
            fsync(h, ino).await;
            (ino, img, (0, 2 * BS as usize))
        }
    }
}

/// The row's write under test, applied to `img` in place. Returns the
/// (offset, len) window it touched.
async fn row_write(h: &H, row: Row, ino: u64, img: &mut [u8], tag: u8) -> (u64, usize) {
    match row {
        Row::Inline | Row::Staged | Row::StripedWriteThrough => {
            let len = img.len();
            let fresh = pattern(len, tag);
            img.copy_from_slice(&fresh);
            write_at(h, ino, 0, &fresh).await;
            (0, len)
        }
        Row::StripedViaStaging => {
            // HALF a block — genuinely partial coverage (DUR-1's leg).
            let off = BS;
            let len = (BS / 2) as usize;
            let fresh = pattern(len, tag);
            img[off as usize..off as usize + len].copy_from_slice(&fresh);
            write_at(h, ino, off, &fresh).await;
            (off, len)
        }
        Row::W1Patch => {
            // Sole-owner LBA-aligned sub-block overwrite (the W1 patch:
            // one in-place DMA, no staging, no meta).
            squeezefs::fuse_client::set_patch_max_bytes(BS / 8);
            let off = 2 * BS + 4096;
            let len = 4096usize;
            let fresh = pattern(len, tag);
            img[off as usize..off as usize + len].copy_from_slice(&fresh);
            write_at(h, ino, off, &fresh).await;
            (off, len)
        }
        Row::W2Fold => {
            // Small non-patchable extents park as an overlay and FOLD at
            // fsync (patch path off so the shape cannot be absorbed).
            squeezefs::fuse_client::set_patch_max_bytes(0);
            let off = 3 * BS + 1024;
            let len = 512usize;
            for k in 0..3u64 {
                let o = off + k * 4096;
                let fresh = pattern(len, tag ^ (k as u8));
                img[o as usize..o as usize + len].copy_from_slice(&fresh);
                write_at(h, ino, o, &fresh).await;
            }
            (off, (2 * 4096 + 512) as usize)
        }
        Row::InplaceOverwrite => {
            squeezefs::fuse_client::set_inplace_overwrite(true);
            let off = BS;
            let len = BS as usize;
            let fresh = pattern(len, tag);
            img[off as usize..off as usize + len].copy_from_slice(&fresh);
            write_at(h, ino, off, &fresh).await;
            (off, len)
        }
        Row::RewriteShadowClose => {
            // Full-block rewrite of a durable block: the rewrite epoch
            // opens and is CLOSED by fsync (`close_rewrite_epoch`).
            let off = 0;
            let len = BS as usize;
            let fresh = pattern(len, tag);
            img[..len].copy_from_slice(&fresh);
            write_at(h, ino, off, &fresh).await;
            (off, len)
        }
    }
}

fn reset_levers() {
    squeezefs::fuse_client::set_patch_max_bytes(BS / 8);
    squeezefs::fuse_client::set_inplace_overwrite(false);
}

/// **Leg A — fsync then data-device power cut.** Every acked byte
/// survives, and the no-orphaned-mapping law holds: every block key the
/// durable map names reads back the bytes that were written.
#[rstest]
#[case::inline(Row::Inline, [0xA1; 16])]
#[case::staged(Row::Staged, [0xA2; 16])]
#[case::striped_write_through(Row::StripedWriteThrough, [0xA3; 16])]
#[case::striped_via_staging(Row::StripedViaStaging, [0xA4; 16])]
#[case::w1_patch(Row::W1Patch, [0xA5; 16])]
#[case::w2_fold(Row::W2Fold, [0xA6; 16])]
#[case::inplace_overwrite(Row::InplaceOverwrite, [0xA7; 16])]
#[case::rewrite_shadow_close(Row::RewriteShadowClose, [0xA8; 16])]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_bytes_survive_a_data_device_power_cut(#[case] row: Row, #[case] uuid: [u8; 16]) {
    let _s = serial().await;
    reset_levers();
    let h = make(uuid, &format!("durmatrix_a_{row:?}")).await;

    let (ino, mut img, _) = setup_row(&h, row).await;
    dev_power_cut::arm_power_cut(&h.dev_path);

    let (off, len) = row_write(&h, row, ino, &mut img, 0xEE).await;
    fsync(&h, ino).await;

    // ENGAGEMENT (the row-validity instrument — a green row that never
    // touched the device proves nothing): full-exposure rows must have
    // journaled device writes and advanced the barrier epoch; the two
    // zero-exposure rows must have written NOTHING to the data device,
    // which is their whole claim.
    let writes = dev_power_cut::next_write_seq(&h.dev_path);
    let epoch = dev_power_cut::barrier_epoch(&h.dev_path);
    assert!(epoch >= 1, "{row:?}: fsync issued no data-device barrier");
    match row {
        Row::Inline | Row::Staged => assert_eq!(
            writes, 0,
            "{row:?}: this layout must never put payload on the data device"
        ),
        _ => assert!(
            writes > 0,
            "{row:?}: no device write was journaled — the row is vacuous"
        ),
    }

    // Everything acked before the fsync is covered by its barrier.
    assert_eq!(
        dev_power_cut::volatile_writes(&h.dev_path),
        0,
        "{row:?}: fsync returned with device writes still volatile — its \
         barrier did not cover the bytes it acknowledged"
    );
    dev_power_cut::power_cut(&h.dev_path);

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, off, len).await;
    assert_eq!(
        got.len(),
        len,
        "{row:?}: short read after the cut ({} of {len})",
        got.len()
    );
    let want = &img[off as usize..off as usize + len];
    if got != want {
        let first = (0..len).find(|&i| got[i] != want[i]).unwrap();
        panic!(
            "{row:?}: acked byte lost to a data-device power cut — first \
             mismatch at +{first} (got {:#04x}, want {:#04x})",
            got[first], want[first]
        );
    }

    // No orphaned mapping: read the WHOLE file back through the map.
    let whole = read_at(&h, ino, 0, img.len()).await;
    assert_eq!(
        whole.len(),
        img.len(),
        "{row:?}: whole-file read short after the cut"
    );
    assert!(
        whole == img,
        "{row:?}: the durable map names a block whose device bytes did not \
         survive the cut (orphaned mapping)"
    );
}

/// **Leg B — power cut MID-WRITE (no fsync).** Losing the un-synced bytes
/// is legal; serving a torn mix is not. What the file reads back must be
/// a whole image: the pre-write one or the post-write one, per byte
/// window.
///
/// Rows covered: the layouts whose publish is either atomic-by-CoW or
/// off the data device entirely. `StripedWriteThrough`,
/// `InplaceOverwrite` and `RewriteShadowClose` are EXCLUDED and tracked
/// as red-today-by-design: a coverage-complete block is DMA'd and its
/// map merged with no intervening data-device barrier (spec DUR-6 §3
/// "no ordering barrier"), so a cut between the DMA and the next barrier
/// can leave the durable map naming reverted bytes. DUR-1 fixes that for
/// the `fsync` path; the un-synced publish path needs the barrier-before-
/// naming edge that DUR-6 specifies, which is a separate work item.
#[rstest]
#[case::inline(Row::Inline, [0xB1; 16])]
#[case::staged(Row::Staged, [0xB2; 16])]
#[case::striped_via_staging(Row::StripedViaStaging, [0xB4; 16])]
#[case::w2_fold(Row::W2Fold, [0xB6; 16])]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_mid_write_never_serves_torn_state(#[case] row: Row, #[case] uuid: [u8; 16]) {
    let _s = serial().await;
    reset_levers();
    let h = make(uuid, &format!("durmatrix_b_{row:?}")).await;

    let (ino, mut img, _) = setup_row(&h, row).await;
    // Make the base image durable so "old" is a well-defined whole image.
    write_at(&h, ino, 0, &img).await;
    fsync(&h, ino).await;
    let old = img.clone();

    dev_power_cut::arm_power_cut(&h.dev_path);
    let (off, len) = row_write(&h, row, ino, &mut img, 0xDD).await;
    // NO fsync — the cut lands mid-write.
    dev_power_cut::power_cut(&h.dev_path);

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, off, len).await;
    assert_eq!(got.len(), len, "{row:?}: short read after a mid-write cut");
    let new_window = &img[off as usize..off as usize + len];
    let old_window = &old[off as usize..off as usize + len];
    assert!(
        got == new_window || got == old_window,
        "{row:?}: a mid-write power cut served TORN state — neither the \
         pre-write image nor the post-write one"
    );
}

/// The IPC ring-write row, recorded rather than silently missing: the
/// §5.5.2 placed sever lands its bytes in the SAME `ActiveBlockBuf` the
/// striped rows above exercise and shares their flush/barrier path
/// verbatim, so its durability is covered by construction here; the
/// end-to-end leg needs a real mount plus the shim and lives in
/// `tests/run_preload_gate.sh` (leg 2).
#[test]
#[ignore = "IPC ring write needs a real mount + LD_PRELOAD shim: tests/run_preload_gate.sh leg 2"]
fn ipc_ring_write_durability_row() {}

/// The un-barriered publish rows (see `a_cut_mid_write_never_serves_torn_state`):
/// recorded as a known gap with its owning spec item rather than dropped.
#[test]
#[ignore = "red-today-by-design: coverage-complete publish merges the block map with no preceding data-device barrier (spec DUR-6 §3) — DUR-1 closes the fsync path only"]
fn unbarriered_publish_rows_are_a_known_gap() {}
