//! Finding 44 — silent data loss on multi-pass sub-block rewrites under
//! the DEFAULT-ON device overlay, caught by the kvmap PR-6a live smoke
//! (2026-09-02) and corpse-decoded on the durable store: after a rewrite
//! pass of sequential quarter-block direct writes and a REMOUNT, block
//! index 0 of every file read ZEROS past its first segment — the map's
//! fresh binding named a device block that only ever received ONE
//! segment's bytes, while same-mount reads served from RAM custody and
//! fio's crc32c verify passed. The C8 accounting oracle is layout-vs-
//! ledger and structurally content-blind, so nothing tripped.
//!
//! The composition gap: every coverage-law suite pins the device overlay
//! OFF (`set_device_overlay_for_tests(false, false)`) while the field
//! default is ON — the overlay × coverage-union × multi-pass-rewrite ×
//! cold-read composition had no contract.
//!
//! The contract here (the remount law, content edition): whatever vehicle
//! a rewrite rides — accumulation write-through, W1 patch, or the device
//! overlay's ACK-early store — a COLD re-read after quiesce returns the
//! last acknowledged bytes, byte-exact, for every pass. The A/B pair
//! pins the attribution: the overlay-OFF control passes on dev; the
//! overlay-ON leg is RED on dev (the finding).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;
const SEG: u64 = BS / 4; // the field shape: 1 MiB segments on 4 MiB blocks
                         // Enough blocks that the inline map crosses to a kvmap head at the
                         // 64 KiB-node cap (the field files were kvmap-headed; the corpse's
                         // stamps were POINT2 records) — needs_indirect at ~2000 entries.
const BLOCKS: usize = 2100;
// The rewrite passes hammer the corpse zone (the field damage was
// block 0-adjacent) — rewriting all 2100 blocks x 2 passes is venue
// time without extra coverage.
const REWRITE_BLOCKS: usize = 8;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _s: TempDir,
}

async fn make(
    b: &NamedTempFile,
    m: &NamedTempFile,
    alloc_ns: &str,
    overlay_on: bool,
    format: bool,
) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // The field's 1 MiB segments are always patch-oversize on 4 MiB
    // blocks; at the downscaled BS they would be patch-eligible and
    // bypass the machinery under test (the coverage suite's own pin).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    // THE LEVER UNDER TEST: the field posture (overlay ON, Bytes
    // vehicle, ACK-early with the O_DIRECT opt-in — the shipped
    // defaults) vs the coverage suites' historical OFF.
    squeezefs::device_overlay::set_device_overlay_for_tests(overlay_on, overlay_on);
    squeezefs::device_overlay::set_ack_early_for_tests(overlay_on, overlay_on);
    let dlm = DlmClient::new().unwrap();
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
    let routed: Arc<RoutedMetaBackend> = {
        if format {
            // The FULL v3 format (the crossing suite's posture): the
            // default multi-writer bit set, so the self-arm gate sees a
            // real volume.
            format_v3(
                m.path(),
                128 * 1024 * 1024,
                &FormatV3Options {
                    node_size: 64 * 1024,
                    journal_len_override: None,
                    force: true,
                    full_wipe: false,
                    format_config_xattr: None,
                },
            )
            .await
            .unwrap();
        }
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
    H { fs, req, _s: s }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: Vec<u8>) {
    let len = data.len();
    let w =
        h.fs.write(h.req, ino, 0, off, bytes::Bytes::from(data), 0, 0)
            .await
            .unwrap_or_else(|e| panic!("write at {off} (block {}): {e:?}", off / BS));
    assert_eq!(w.written as usize, len, "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("READ at {off} (block {}): {e:?}", off / BS))
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// The remount equivalent: drop every RAM tier + cached block key so the
/// next read is device-true (the coverage suite's purge, verbatim).
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

/// One rewrite pass in the field shape: per block, four quarter-block
/// segments issued CONCURRENTLY (fio iodepth=16 keeps a whole block's
/// segments in flight together; delivery order stays sequential here —
/// the out-of-order face is the coverage suite's law and stays there).
async fn rewrite_pass(h: &H, ino: u64, want: &mut [u8], tag: u8) {
    for b in 0..REWRITE_BLOCKS as u64 {
        let mut writes = Vec::new();
        for s in 0..4u64 {
            let off = b * BS + s * SEG;
            let data = pattern(SEG as usize, tag);
            want[off as usize..(off + SEG) as usize].copy_from_slice(&data);
            writes.push(write_at(h, ino, off, data));
        }
        futures::future::join_all(writes).await;
    }
    fsync(h, ino).await;
}

async fn cold_full_read_matches(h: &H, ino: u64, want: &[u8], what: &str) {
    purge_tiers(h, ino).await;
    // Read in 8-block windows (a 131 MiB single READ is no kernel shape).
    let mut got = Vec::with_capacity(want.len());
    let mut off = 0usize;
    while off < want.len() {
        let l = (8 * BS as usize).min(want.len() - off);
        got.extend_from_slice(&read_at(h, ino, off as u64, l).await);
        off += l;
    }
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        let blk = i as u64 / BS;
        let seg = (i as u64 % BS) / SEG;
        let zeros = got[i..]
            .iter()
            .take(SEG as usize)
            .filter(|&&x| x == 0)
            .count();
        panic!(
            "{what}: first mismatch at byte {i} (block {blk}, segment {seg}): \
             got {:#04x} want {:#04x}; {zeros}/{SEG} of the segment reads \
             zero — the finding-44 corpse shape (the device block missing \
             acknowledged segments after a cold re-read)",
            got[i], want[i]
        );
    }
}

async fn run_multi_pass(overlay_on: bool, ns: &str) {
    // Backing under target/ — tmpfs refuses the O_DIRECT open the
    // zc_write_fd screen requires (the ack-early suite's rule); ONE
    // backing+meta pair spans both sessions (the remount law's venue).
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let b = tempfile::Builder::new()
        .prefix("f44-b")
        .tempfile_in(dir)
        .unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    // Meta stays on tmpfs (the coverage suites' posture); only the DATA
    // backing needs the O_DIRECT-capable fs.
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();

    let h = make(&b, &m, ns, overlay_on, true).await;
    let ino = create(&h, "f44").await;
    let len = BLOCKS * BS as usize;
    let mut want = pattern(len, 1);
    // Fresh pass in block-sized writes (one giant Bytes would be a
    // single ActiveBlockBuf shape no kernel produces).
    for b in 0..BLOCKS as u64 {
        let off = b * BS;
        write_at(
            &h,
            ino,
            off,
            want[off as usize..(off + BS) as usize].to_vec(),
        )
        .await;
    }
    fsync(&h, ino).await;
    // The crossing check: the field files were kvmap-headed.
    let head_check =
        h.fs.router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap();
    assert_eq!(
        head_check.block_map_id.as_deref(),
        Some("kvmap:1"),
        "fixture: the file must cross into the kvmap tree (got {:?})",
        head_check.block_map_id
    );
    cold_full_read_matches(&h, ino, &want, "fresh pass").await;

    rewrite_pass(&h, ino, &mut want, 2).await;
    cold_full_read_matches(&h, ino, &want, "rewrite pass 1").await;

    rewrite_pass(&h, ino, &mut want, 3).await;
    cold_full_read_matches(&h, ino, &want, "rewrite pass 2 (same session)").await;

    // SESSION 2 — the true remount (the field's umount/mount): a fresh
    // backend open + recovery walk over the same volumes; the cold read
    // must return pass-2's acknowledged bytes byte-exact.
    drop(h);
    let ns2 = format!("{ns}_s2");
    let h2 = make(&b, &m, &ns2, overlay_on, false).await;
    cold_full_read_matches(&h2, ino, &want, "rewrite pass 2 (REMOUNT)").await;
}

/// The finding-44 red: the FIELD posture (device overlay ON — the shipped
/// default since 2026-08-15) across two rewrite passes with cold re-reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_pass_rewrites_survive_cold_reads_with_the_overlay_on() {
    let _g = serial().await;
    run_multi_pass(true, "f44_overlay_on").await;
}

/// The attribution control: the identical sequence with the overlay OFF
/// (the coverage suites' historical posture) — green on dev, pinning the
/// composition, not the coverage union, as the finding's home.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_pass_rewrites_survive_cold_reads_with_the_overlay_off() {
    let _g = serial().await;
    run_multi_pass(false, "f44_overlay_off").await;
}
