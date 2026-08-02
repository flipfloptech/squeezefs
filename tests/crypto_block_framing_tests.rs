//! Transform-image FRAMING (PR 6 prerequisite, found by the §5.6
//! decoded-bytes pin): every non-passthrough `process_write` image must be
//! SELF-DELIMITING, because block reads return the full `block_size`
//! window — the stored image plus whatever trailing bytes the device
//! holds. Unframed images made cold device reads of EVERY compressed/
//! encrypted striped block fail loud:
//!   - lz4: `decompress_size_prepended` rejects trailing bytes
//!     (`OffsetZero` — reproduced standalone and through the full FS on
//!     the parent commit);
//!   - AEAD: `decrypt` seals/opens `data[header..]`, so block padding
//!     lands inside the tag check and every padded open fails.
//!
//! The fix frames every non-passthrough image as `[u32 LE image_len]`
//! plus the image. FORWARD-ONLY (standing directive 2026-07-12):
//! unframed legacy blobs refuse loud — no sniffing shim; cold reads of
//! such volumes never worked, so there is nothing behavioral to
//! preserve.
//!
//! Passthrough volumes are byte-identical (no frame — the passthrough
//! contract is zero transform, and R3 ranged reads depend on it).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make(
    uuid: [u8; 16],
    alloc_ns: &str,
    staging: bool,
    comp: &str,
    enc: &str,
    pem: Option<&str>,
) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
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
    fs.meta_backend = Some(routed);
    fs.router.set_crypto(CryptoCompressState::new(
        comp.to_string(),
        enc.to_string(),
        pem,
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
        _b: b,
        _m: m,
        _s: s,
    }
}

fn payload(len: usize) -> Vec<u8> {
    // Compressible but position-dependent: wrong-window/wrong-frame serves
    // cannot alias.
    (0..len).map(|i| ((i as u64) / 65536) as u8).collect()
}

async fn write_all(h: &H, ino: u64, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let chunk = std::cmp::min(BS as usize, data.len() - off);
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
            .await
            .unwrap();
        assert_eq!(w.written as usize, chunk);
        off += chunk;
    }
}

/// Full-FS cold round-trip on a transform volume: write striped, purge
/// every cache, read back cold from the DEVICE (the exact shape that
/// failed with `OffsetZero`/AEAD errors before framing).
async fn roundtrip(h: &H, name: &str, len: usize) {
    let ino =
        h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let data = payload(len);
    write_all(h, ino, &data).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.file_type, "striped", "fixture must be striped");
    for key in meta.block_map.clone().unwrap_or_default().values() {
        h.fs.router.cache.purge_block_key(key);
    }
    // Cold sub-block read mid-file + cold whole-file sweep.
    let off = 2 * BS + 8192;
    let d = h.fs.read(h.req, ino, 0, off, 4096, 0).await.unwrap().data;
    assert_eq!(d.len(), 4096);
    assert!(
        d.iter()
            .enumerate()
            .all(|(j, &x)| x == data[off as usize + j]),
        "{name}: cold 4 KiB read must decode"
    );
    let mut got = Vec::new();
    let mut off = 0u64;
    while (off as usize) < len {
        let chunk = std::cmp::min(BS, len as u64 - off) as u32;
        got.extend_from_slice(&h.fs.read(h.req, ino, 0, off, chunk, 0).await.unwrap().data);
        off += chunk as u64;
    }
    assert_eq!(got.len(), len, "{name}: full sweep length");
    assert_eq!(got, data, "{name}: full cold sweep must decode byte-exact");
}

/// The bug's exact shape: cache-less lz4 volume, direct striped writes,
/// cold device reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lz4_cold_striped_reads_decode() {
    let h = make(
        *b"framing-a-pr6-00",
        "frame_ns_a",
        false,
        "lz4",
        "none",
        None,
    )
    .await;
    roundtrip(&h, "f_lz4", (6 * BS) as usize).await;
}

/// Staged-growth promotion variant (staging dirs present) — the second
/// write topology that stores transform images.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lz4_staged_growth_promotion_decodes() {
    let h = make(
        *b"framing-b-pr6-00",
        "frame_ns_b",
        true,
        "lz4",
        "none",
        None,
    )
    .await;
    roundtrip(&h, "f_lz4s", (6 * BS) as usize).await;
}

/// zstd leg (its own decoder, its own trailing-bytes behavior).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zstd_cold_striped_reads_decode() {
    let h = make(
        *b"framing-c-pr6-00",
        "frame_ns_c",
        false,
        "zstd",
        "none",
        None,
    )
    .await;
    roundtrip(&h, "f_zstd", (6 * BS) as usize).await;
}

/// Encrypted leg: AEAD open over a padded window fails the tag check
/// without framing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lz4_aes_cold_striped_reads_decode() {
    let pem = test_pem();
    let h = make(
        *b"framing-d-pr6-00",
        "frame_ns_d",
        false,
        "lz4",
        "aes256gcm-rsa",
        Some(&pem),
    )
    .await;
    roundtrip(&h, "f_aes", (6 * BS) as usize).await;
}

/// Unit-level frame contract: padded windows decode; unframed legacy
/// blobs and truncated frames refuse loud (forward-only — no sniffing);
/// passthrough stays byte-identical (R3 depends on it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_contract_padded_legacy_truncated_passthrough() {
    let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    let data = bytes::Bytes::from(payload(96 * 1024));
    let img = state.process_write(data.clone()).unwrap();

    // Padded to a block-size window (device-read shape) — must decode.
    let mut padded = img.to_vec();
    padded.resize(BS as usize, 0xEE); // worst case: NON-zero trailing bytes
    let out = state.process_read(&padded).unwrap();
    assert_eq!(out.as_ref(), data.as_ref(), "padded window must decode");

    // Exact image — must decode (writeback parity shape).
    let out = state.process_read(&img).unwrap();
    assert_eq!(out.as_ref(), data.as_ref(), "exact image must decode");

    // Unframed legacy blob — refuse loud, never garbage.
    let legacy = lz4_flex::compress_prepend_size(&data);
    assert!(
        state.process_read(&legacy).is_err(),
        "unframed legacy blobs must refuse loud (forward-only, no shim)"
    );

    // Truncated frame — refuse loud.
    assert!(
        state.process_read(&img[..img.len() - 1]).is_err(),
        "truncated frame must refuse loud"
    );
    assert!(
        state.process_read(&img[..2]).is_err(),
        "sub-header input must refuse loud"
    );

    // Passthrough: byte-identical, no frame.
    let pt = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
    let img = pt.process_write(data.clone()).unwrap();
    assert_eq!(img.as_ref(), data.as_ref(), "passthrough adds no frame");
    assert_eq!(pt.process_read(&img).unwrap().as_ref(), data.as_ref());
}

fn test_pem() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let mut rng = rand::thread_rng();
    let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .unwrap()
        .to_string()
}
