//! FIND-RW4-A — incompressible blocks must round-trip on compressed
//! volumes (`.benchmarks/2026-07-17-rw4-extent-overlay.md` §6, A/B-proven
//! pre-existing on base `a070836`).
//!
//! THE DEFECT: `process_write` frames every non-passthrough image as
//! `[u32 LE image_len][image]` with NO escape for expansion. Incompressible
//! payloads expand under lz4/zstd (worst case ≈ len + len/255 + 24), and
//! the AEAD envelope adds ~291–547 B unconditionally — so a full
//! `block_size` payload's stored image EXCEEDS `block_size`. Two failures
//! follow:
//!
//!  1. **Read window too small (availability)**: undecorated block reads
//!     fetch exactly `block_size` bytes, the frame claims more, and every
//!     cold read of such a block fails loud
//!     (`malformed transform frame: claims N image bytes, M available`).
//!  2. **Chunk overflow (silent neighbor corruption)**: on the default
//!     geometry (`block_size == CHUNK_SIZE == 4 MiB`) the oversized DMA
//!     lands anyway, writing past the allocator chunk into the NEXT
//!     chunk's bytes.
//!
//! THE CONTRACT PINNED HERE (the fix):
//!  - **Store-raw escape**: a block whose compressed image would not
//!    shrink is stored RAW behind a frame marker (bit 31 of the length
//!    word); `process_read` dispatches on the marker. Compression becomes
//!    best-effort per block (btrfs/zfs practice); the escape must NOT
//!    disable compression for compressible blocks (`compress_stored_raw`
//!    stays 0 there, stored image < raw).
//!  - **Encryption interplay**: the escape sits below the AEAD layer —
//!    incompressible blocks on encrypted volumes store
//!    `encrypt(raw payload)` with the marker set.
//!  - **Geometry**: transformed volumes reserve `TRANSFORM_BLOCK_HEADROOM`
//!    inside each chunk (format clamps `block_size`); non-passthrough
//!    reads widen their device window to the worst-case stored image;
//!    mounts REFUSE pre-fix transformed geometry loud (forward-only —
//!    reformat required). Stored images may NEVER exceed the allocator
//!    chunk: the write path refuses loud, never truncates or overflows.
//!  - **Decoder superset**: pre-fix frames (bit 31 always 0) decode
//!    unchanged.
//!
//! RED on dev `22d652a`: the round-trip tests fail on their cold-read
//! assertions with the exact A/B frame-error signature; the gate/marker
//! tests fail on their refusal assertions (M1 convention — assertions,
//! never compilation).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::crypto_compress::{CryptoCompressState, TRANSFORM_BLOCK_HEADROOM};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata as _, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Frame constants mirrored test-side (the on-disk contract, not the
/// implementation's private names).
const FRAME_LEN_BYTES: usize = 4;
const FRAME_RAW_FLAG: u32 = 1 << 31;

/// Counter deltas + `SQUEEZEFS_DEFAULT_BLOCK_SIZE` are process-global;
/// tests serialize (house pattern).
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
    routed: Arc<RoutedMetaBackend>,
    /// Backing DATA device file — tests read it directly to pin stored
    /// frame bytes (the device truth the read path consumes).
    backing: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

#[allow(clippy::too_many_arguments)]
async fn make(
    uuid: [u8; 16],
    alloc_ns: &str,
    staging: bool,
    comp: &str,
    enc: &str,
    volume_key: Option<&squeezefs::keyfile::VolumeKey>,
    block_size: u64,
) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size.to_string());
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
    let s = staging.then(|| tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
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
            hash_seed: 0xC0FF_EE00_1234_5678,
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
    fs.meta_backend = Some(routed.clone());
    fs.router.set_block_size(block_size);
    fs.router.set_crypto(CryptoCompressState::new(
        comp.to_string(),
        enc.to_string(),
        volume_key,
    ));
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
        routed,
        backing: b,
        _m: m,
        _s: s,
    }
}

/// Deterministic high-entropy filler — incompressible by construction
/// (the elbencho payload class that surfaced FIND-RW4-A).
fn lcg_bytes(len: usize, mut seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((seed >> 33) as u8);
    }
    v
}

/// Compressible, position-dependent (aliasing-proof) filler.
fn compressible(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i as u64) / 65536) as u8).collect()
}

async fn create_file(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_all(h: &H, ino: u64, data: &[u8], bs: u64) {
    try_write_all(h, ino, data, bs).await.unwrap();
}

async fn try_write_all(h: &H, ino: u64, data: &[u8], bs: u64) -> Result<(), fuse3::Errno> {
    let mut off = 0usize;
    while off < data.len() {
        let chunk = std::cmp::min(bs as usize, data.len() - off);
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                off as u64,
                bytes::Bytes::copy_from_slice(&data[off..off + chunk]),
                0,
                0,
            )
            .await?;
        assert_eq!(w.written as usize, chunk);
        off += chunk;
    }
    Ok(())
}

async fn read_all(h: &H, ino: u64, len: usize, bs: u64) -> Result<Vec<u8>, fuse3::Errno> {
    let mut got = Vec::new();
    let mut off = 0u64;
    while (off as usize) < len {
        let chunk = std::cmp::min(bs, len as u64 - off) as u32;
        let reply = h.fs.read(h.req, ino, 0, off, chunk, 0).await?;
        got.extend_from_slice(&reply.data);
        off += chunk as u64;
    }
    Ok(got)
}

/// Purge every cache tier holding this file's plaintext so the next read
/// is a COLD device read + decode (the failing shape).
async fn purge_cold(h: &H, ino: u64) {
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    for key in meta.block_map.clone().unwrap_or_default().values() {
        h.fs.router.cache.purge_block_key(key);
    }
}

/// Whole-cycle round-trip: write incompressible data, fsync, verify WARM,
/// purge tiers, verify COLD (the A/B failure shape). `name` prefixes
/// assertion messages.
async fn incompressible_roundtrip(h: &H, name: &str, len: usize, bs: u64) {
    let ino = create_file(h, name).await;
    let data = lcg_bytes(len, 0xF00D_5EED ^ len as u64);
    write_all(h, ino, &data, bs).await;
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .unwrap_or_else(|e| panic!("{name}: fsync must succeed on an in-contract volume: {e:?}"));

    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.file_type, "striped", "{name}: fixture must be striped");

    // (a) WARM read — RAM tiers hold plaintext; must be byte-exact.
    let warm = read_all(h, ino, len, bs)
        .await
        .unwrap_or_else(|e| panic!("{name}: warm read failed: {e:?}"));
    assert_eq!(warm, data, "{name}: warm read must be byte-exact");

    // (b) COLD read — every tier purged; device bytes + decode only.
    purge_cold(h, ino).await;
    let cold = read_all(h, ino, len, bs).await.unwrap_or_else(|e| {
        panic!(
            "{name}: COLD read of incompressible data failed loud (the FIND-RW4-A \
             availability defect): {e:?}"
        )
    });
    assert_eq!(cold, data, "{name}: cold read must be byte-exact");
}

// ---------------------------------------------------------------------
// 1. Round-trip matrix: lz4 / zstd, with and without encryption, plus
//    encrypt-only (the AEAD envelope alone overflows the read window).
//    RED: cold reads fail `malformed transform frame`.
// ---------------------------------------------------------------------

const BS_SMALL: u64 = 262_144; // 256 KiB — chunk-safe, window-defect visible

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_lz4_striped_cold_roundtrip() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-lz4-0000001",
        "rw4a_lz4",
        false,
        "lz4",
        "none",
        None,
        BS_SMALL,
    )
    .await;
    incompressible_roundtrip(&h, "lz4", 3 * BS_SMALL as usize + 8192, BS_SMALL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_zstd_striped_cold_roundtrip() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-zst-0000001",
        "rw4a_zstd",
        false,
        "zstd",
        "none",
        None,
        BS_SMALL,
    )
    .await;
    incompressible_roundtrip(&h, "zstd", 3 * BS_SMALL as usize + 8192, BS_SMALL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_lz4_aes_striped_cold_roundtrip() {
    let _g = serial().await;
    let volume_key = test_volume_key();
    let h = make(
        *b"rw4a-l4a-0000001",
        "rw4a_lz4_aes",
        false,
        "lz4",
        "aes256gcm",
        Some(&volume_key),
        BS_SMALL,
    )
    .await;
    incompressible_roundtrip(&h, "lz4+aes", 2 * BS_SMALL as usize, BS_SMALL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_zstd_chacha_striped_cold_roundtrip() {
    let _g = serial().await;
    let volume_key = test_volume_key();
    let h = make(
        *b"rw4a-zch-0000001",
        "rw4a_zstd_cha",
        false,
        "zstd",
        "chacha20",
        Some(&volume_key),
        BS_SMALL,
    )
    .await;
    incompressible_roundtrip(&h, "zstd+chacha", 2 * BS_SMALL as usize, BS_SMALL).await;
}

/// Encrypt-only: no compression to blame — the AEAD envelope alone pushes
/// a full-block image past the `block_size` read window. The escape's
/// geometry (headroom + widened window) must carry this too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_aes_only_striped_cold_roundtrip() {
    let _g = serial().await;
    let volume_key = test_volume_key();
    let h = make(
        *b"rw4a-aes-0000001",
        "rw4a_aes_only",
        false,
        "none",
        "aes256gcm",
        Some(&volume_key),
        BS_SMALL,
    )
    .await;
    incompressible_roundtrip(&h, "aes-only", 2 * BS_SMALL as usize, BS_SMALL).await;
}

// ---------------------------------------------------------------------
// 2. Fold path (RW4 machinery): sub-block overwrites on a compressed
//    volume park extents; the fsync fold SEEDS from the incompressible
//    base block (cold device read + decode) and uploads the folded image.
//    RED: the fold's seed read fails the frame parse → fsync errors.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_extent_fold_cold_roundtrip() {
    let _g = serial().await;
    const BS: u64 = 65_536;
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    squeezefs::fuse_client::set_parked_cap_buffers(256);
    let h = make(
        *b"rw4a-fld-0000001",
        "rw4a_fold",
        true,
        "lz4",
        "none",
        None,
        BS,
    )
    .await;
    let ino = create_file(&h, "fold_file").await;
    let mut data = lcg_bytes(2 * BS as usize, 0xBEEF_CAFE);
    write_all(&h, ino, &data, BS).await;
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("seed fsync must succeed");
    purge_cold(&h, ino).await;

    // Scattered 4 KiB overwrites — patch-ineligible (transform volume) →
    // extent overlay parks; fsync folds (seed read of the incompressible
    // base + one durable upload).
    for (i, off) in [4096u64, 20480, 36864, BS + 8192, BS + 24576]
        .iter()
        .enumerate()
    {
        let patch = lcg_bytes(4096, 0xA5A5_0000 + i as u64);
        data[*off as usize..*off as usize + 4096].copy_from_slice(&patch);
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                *off,
                bytes::Bytes::copy_from_slice(&patch),
                0,
                0,
            )
            .await
            .expect("sub-block overwrite must ack");
        assert_eq!(w.written, 4096);
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap_or_else(|e| {
        panic!(
            "fold fsync failed loud — the fold's SEED read of an incompressible \
             lz4 block hit the FIND-RW4-A frame defect: {e:?}"
        )
    });

    purge_cold(&h, ino).await;
    let cold = read_all(&h, ino, data.len(), BS)
        .await
        .unwrap_or_else(|e| panic!("cold post-fold read failed loud: {e:?}"));
    assert_eq!(cold, data, "post-fold cold read must be byte-exact");
}

// ---------------------------------------------------------------------
// 3. Compressible control: the escape must not disable compression.
//    Device-truth pin: the stored frame's raw flag is CLEAR and the
//    image is SMALLER than the raw payload; `compress_stored_raw` stays 0.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compressible_control_keeps_compressing() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-ctl-0000001",
        "rw4a_control",
        false,
        "lz4",
        "none",
        None,
        BS_SMALL,
    )
    .await;
    let raw_before = METRICS.compress_stored_raw.load(Ordering::Relaxed);
    let ino = create_file(&h, "ctl").await;
    let data = compressible(2 * BS_SMALL as usize);
    write_all(&h, ino, &data, BS_SMALL).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    purge_cold(&h, ino).await;
    let cold = read_all(&h, ino, data.len(), BS_SMALL).await.unwrap();
    assert_eq!(cold, data, "compressible control must round-trip cold");
    assert_eq!(
        METRICS.compress_stored_raw.load(Ordering::Relaxed),
        raw_before,
        "compressible blocks must NOT take the store-raw escape"
    );

    // Device truth: first block's stored frame at its mapped offset —
    // flag clear, image smaller than raw.
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bm = meta.block_map.clone().unwrap();
    let key0 = bm.get(&0).expect("block 0 mapped");
    let off0: u64 = key0
        .split(':')
        .next()
        .unwrap()
        .parse()
        .expect("default-volume key is a bare offset");
    let word = read_frame_word(h.backing.path(), off0);
    assert_eq!(
        word & FRAME_RAW_FLAG,
        0,
        "compressible block must store a COMPRESSED (unflagged) frame"
    );
    let image_len = (word & !FRAME_RAW_FLAG) as usize;
    assert!(
        image_len < BS_SMALL as usize,
        "compressible stored image ({image_len} B) must be smaller than raw ({BS_SMALL} B)"
    );
}

/// Incompressible counterpart of the device-truth pin: raw flag SET,
/// image length == raw payload length, counter increments.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompressible_blocks_store_raw_flagged_frames() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-flg-0000001",
        "rw4a_flagged",
        false,
        "lz4",
        "none",
        None,
        BS_SMALL,
    )
    .await;
    let raw_before = METRICS.compress_stored_raw.load(Ordering::Relaxed);
    let ino = create_file(&h, "flagged").await;
    let data = lcg_bytes(BS_SMALL as usize, 0xDEAD_10CC);
    write_all(&h, ino, &data, BS_SMALL).await;
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("fsync of one incompressible block must succeed");
    assert!(
        METRICS.compress_stored_raw.load(Ordering::Relaxed) > raw_before,
        "incompressible block must take the store-raw escape (compress_stored_raw)"
    );

    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bm = meta.block_map.clone().unwrap();
    let key0 = bm.get(&0).expect("block 0 mapped");
    let off0: u64 = key0.split(':').next().unwrap().parse().unwrap();
    let word = read_frame_word(h.backing.path(), off0);
    assert_ne!(
        word & FRAME_RAW_FLAG,
        0,
        "incompressible block must store a RAW-flagged frame"
    );
    assert_eq!(
        (word & !FRAME_RAW_FLAG) as usize,
        BS_SMALL as usize,
        "raw-escape image must be exactly the raw payload"
    );

    purge_cold(&h, ino).await;
    let cold = read_all(&h, ino, data.len(), BS_SMALL).await.unwrap();
    assert_eq!(cold, data, "raw-flagged block must decode byte-exact cold");
}

fn read_frame_word(dev: &std::path::Path, offset: u64) -> u32 {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(dev).unwrap();
    f.seek(std::io::SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; FRAME_LEN_BYTES];
    f.read_exact(&mut b).unwrap();
    u32::from_le_bytes(b)
}

// ---------------------------------------------------------------------
// 4. Chunk-overflow refusal: on out-of-contract geometry (block_size ==
//    CHUNK_SIZE, transformed) an incompressible full block CANNOT be
//    stored. The write path must refuse LOUD — at the write or its
//    durability barrier — landing it would trample the neighboring chunk
//    (the silent-corruption half of FIND-RW4-A). Dev behavior (RED): the
//    write lands overflowing, everything claims success, and the cold
//    read fails — silent corruption + unavailability.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_stored_image_refused_never_silent() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-ovr-0000001",
        "rw4a_oversize",
        false,
        "lz4",
        "none",
        None,
        CHUNK_SIZE,
    )
    .await;
    let ino = create_file(&h, "oversize").await;
    let data = lcg_bytes(CHUNK_SIZE as usize, 0x0BAD_F00D);
    let stored = async {
        try_write_all(&h, ino, &data, CHUNK_SIZE).await?;
        h.fs.fsync(h.req, ino, 0, false).await
    }
    .await;
    match stored {
        Err(_) => {
            // Post-fix contract: the oversized stored image is refused loud
            // somewhere on the write/durability path — nothing landed by
            // overflowing the chunk.
        }
        Ok(_) => {
            // If the store CLAIMS success, the data must actually be
            // readable back cold — an unreadable "success" is the defect.
            purge_cold(&h, ino).await;
            let cold = read_all(&h, ino, data.len(), CHUNK_SIZE).await.expect(
                "store claimed success for an incompressible full-chunk block, so the \
                     cold read MUST decode — instead it failed loud (FIND-RW4-A: the \
                     oversized frame landed by overflowing the allocator chunk)",
            );
            assert_eq!(
                cold, data,
                "cold read after claimed-success store must be exact"
            );
        }
    }
}

// ---------------------------------------------------------------------
// 5. Mount geometry gate (forward-only): a transformed volume whose
//    block_size leaves no room for the worst-case stored image inside the
//    allocator chunk (every pre-fix compressed/encrypted format) must
//    REFUSE to mount loud. Passthrough and clamped-geometry volumes mount.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_gate_refuses_pre_fix_transformed_geometry() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-gat-0000001",
        "rw4a_gate_refuse",
        false,
        "lz4",
        "none",
        None,
        CHUNK_SIZE,
    )
    .await;
    set_format_config(&h, CHUNK_SIZE, "lz4", "none").await;
    assert!(
        h.fs.init(h.req).await.is_err(),
        "mount must REFUSE a pre-fix transformed geometry (block_size == chunk leaves \
         no headroom for the worst-case stored image) — reformat is the remedy"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_gate_passes_clamped_and_passthrough_geometry() {
    let _g = serial().await;
    // Clamped transformed geometry (what post-fix format produces).
    let clamped = CHUNK_SIZE - TRANSFORM_BLOCK_HEADROOM;
    let h = make(
        *b"rw4a-gok-0000001",
        "rw4a_gate_ok",
        false,
        "lz4",
        "none",
        None,
        clamped,
    )
    .await;
    set_format_config(&h, clamped, "lz4", "none").await;
    assert!(
        h.fs.init(h.req).await.is_ok(),
        "clamped transformed geometry must mount"
    );

    // Passthrough at full chunk-size blocks: untouched by the gate.
    let h2 = make(
        *b"rw4a-gpt-0000001",
        "rw4a_gate_pt",
        false,
        "none",
        "none",
        None,
        CHUNK_SIZE,
    )
    .await;
    set_format_config(&h2, CHUNK_SIZE, "none", "none").await;
    assert!(
        h2.fs.init(h2.req).await.is_ok(),
        "passthrough geometry must be untouched by the FIND-RW4-A gate"
    );
}

async fn set_format_config(h: &H, block_size: u64, comp: &str, enc: &str) {
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size,
        capacity: 16 * 1024 * 1024 * 1024,
        inodes: 1000,
        compression: comp.to_string(),
        encrypt_algo: enc.to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_volumes: None,
        data_lv: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    };
    h.routed
        .setxattr(
            1,
            "user.squeezefs.format_config",
            &serde_json::to_vec(&cfg).unwrap(),
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------
// 6. Frame-encoding compatibility: the new decoder is a STRICT SUPERSET
//    of the pre-fix encoding — unflagged (pre-fix) frames decode
//    unchanged; RAW-flagged frames decode to the raw payload.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_marker_superset_of_pre_fix_encoding() {
    let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);

    // Pre-fix writer form for a COMPRESSIBLE payload (bit 31 always 0):
    // must decode unchanged (superset decoder).
    let data = compressible(96 * 1024);
    let image = lz4_flex::compress_prepend_size(&data);
    let mut old_form = Vec::with_capacity(4 + image.len());
    old_form.extend_from_slice(&(image.len() as u32).to_le_bytes());
    old_form.extend_from_slice(&image);
    assert_eq!(
        state.process_read(&old_form).unwrap().as_ref(),
        data.as_slice(),
        "pre-fix unflagged frames must keep decoding (strict superset)"
    );

    // RAW-flagged frame: the length word carries bit 31; the payload is
    // stored verbatim (uncompressed). Padded to a device window too.
    let raw = lcg_bytes(64 * 1024, 0x00DD_BA11);
    let mut flagged = Vec::with_capacity(4 + raw.len());
    flagged.extend_from_slice(&((raw.len() as u32) | FRAME_RAW_FLAG).to_le_bytes());
    flagged.extend_from_slice(&raw);
    assert_eq!(
        state
            .process_read(&flagged)
            .expect("RAW-flagged frame must decode (the store-raw escape marker)")
            .as_ref(),
        raw.as_slice(),
        "RAW-flagged frame must decode to the raw payload"
    );
    flagged.resize(256 * 1024, 0xEE);
    assert_eq!(
        state.process_read(&flagged).unwrap().as_ref(),
        raw.as_slice(),
        "padded RAW-flagged frame must decode (window tolerance)"
    );

    // A flagged frame whose claimed length exceeds the window still
    // refuses loud (corruption is never served).
    let mut torn = Vec::new();
    torn.extend_from_slice(&((1_000_000u32) | FRAME_RAW_FLAG).to_le_bytes());
    torn.extend_from_slice(&[0u8; 512]);
    assert!(
        state.process_read(&torn).is_err(),
        "over-claiming RAW frame must refuse loud"
    );
}

// ---------------------------------------------------------------------
// 7. Crash pin: a durable upload of an incompressible block that DIES
//    between the transformed device write and the meta commit leaves the
//    OLD durable data fully intact (mapping unchanged, old block
//    decodes), and the acked new data still lands once the fault clears
//    (never-lossy).
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_incompressible_upload_keeps_old_data() {
    let _g = serial().await;
    let h = make(
        *b"rw4a-crs-0000001",
        "rw4a_crash",
        true,
        "lz4",
        "none",
        None,
        BS_SMALL,
    )
    .await;
    let ino = create_file(&h, "crash_pin").await;
    // v1: compressible 2-block striped baseline, durably stored.
    let v1 = compressible(2 * BS_SMALL as usize);
    write_all(&h, ino, &v1, BS_SMALL).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let meta1 = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta1.file_type, "striped");
    let key1 = meta1.block_map.clone().unwrap().get(&0).unwrap().clone();

    // v2: incompressible full overwrite of block 0 whose durable upload
    // DMA is failed by injection (armed with margin so any retry inside
    // the window keeps failing — the mapping provably cannot move while
    // the old-data assertions run). The write itself must still ack:
    // never-lossy custody degrades to staging.
    let v2b0 = lcg_bytes(BS_SMALL as usize, 0xC4A5_11ED);
    squeezefs::nvme_dev::set_fail_next_writes(8);
    try_write_all(&h, ino, &v2b0, BS_SMALL)
        .await
        .expect("write must ack via the never-lossy staging degrade");

    // Old durable data intact inside the failure window: mapping
    // unchanged, old transformed block still decodes.
    let meta2 = h.fs.router.fetch_metadata(&path).await.unwrap();
    let key2 = meta2.block_map.clone().unwrap().get(&0).unwrap().clone();
    assert_eq!(key1, key2, "failed upload must not move the block mapping");
    let raw = h.fs.router.read_nvme_block(&key2).await.unwrap();
    let plain =
        h.fs.router
            .get_crypto()
            .process_read(&raw)
            .expect("old durable block must still decode inside the failure window");
    assert_eq!(
        &plain[..BS_SMALL as usize],
        &v1[..BS_SMALL as usize],
        "old durable bytes must be intact"
    );

    // Never-lossy: the acked v2 lands once the fault clears.
    squeezefs::nvme_dev::clear_fail_next_writes();
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("post-fault fsync must land v2 durably");
    purge_cold(&h, ino).await;
    let mut expected = v1.clone();
    expected[..BS_SMALL as usize].copy_from_slice(&v2b0);
    let cold = read_all(&h, ino, expected.len(), BS_SMALL).await.unwrap();
    assert_eq!(cold, expected, "v2 must be durable + cold-readable");
}

/// The mount-resolved volume key these tests encrypt under (KW-1: the
/// operator's key file material + the volume's KDF salt — never a PEM;
/// `docs/design-key-handling.md`).
fn test_volume_key() -> squeezefs::keyfile::VolumeKey {
    let material = squeezefs::keyfile::KeyMaterial::from_bytes(
        b"squeezefs-test-key-material-0123456789".to_vec(),
    )
    .expect("test key material");
    squeezefs::keyfile::derive_volume_key(&material, &[0x33u8; 32])
}
