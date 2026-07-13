//! R3 — sub-block ranged reads (docs/design-read-path.md §5.6 / PR 6).
//!
//! Contracts pinned (the doc's PR 6 test list, red-first):
//! - Byte-exactness across odd offsets/lens/EOF-straddles vs the written
//!   ground truth (whole-block reference) — bounce-leg window padding is
//!   never served.
//! - Zero-copy-leg alignment matrix: 4 KiB-aligned request + `RangedDest`
//!   ⇒ no bounce; any unaligned edge ⇒ exactly one counted bounce, bytes
//!   still exact.
//! - Amplification contract: N disjoint cold 4 KiB reads of ONE block ⇒
//!   `get_obj` Δ = N (ranged ops count device reads by design, §5.6),
//!   `ranged_read_bytes` ≈ N × 4 KiB, tier/hot unchanged (never
//!   published).
//! - Ghost convergence: ranged fetches RECORD heat; a subsequent
//!   whole-block fetch ghost-admits per §5.3 (publish + tier residency).
//! - `SQUEEZEFS_READ_RANGED_THRESHOLD=0` kill switch.
//! - Compressed/encrypted volumes never range (decode needs the whole
//!   block) AND the sibling raw full-block dest leg refuses transform
//!   configs — the pre-existing ciphertext-serve hole (§5.6 sibling-leg
//!   hygiene): a small-block compressed volume with a payload dest must
//!   serve DECODED bytes.
//! - Rebind-under-movement: concurrent COW overwrites during ranged reads
//!   ⇒ rebind-or-current, never foreign bytes (074/075-family shape).
//!
//! Counter-asserting phases share ONE test fn (`ranged_phases`) — the
//! churn suite's counter-isolation discipline (`get_obj` and the
//! `ranged_*` counters are process-global).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{DataRouter, RangedDest};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
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

async fn make_with(block_size: &str, uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = Some(tempdir().unwrap());
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Deterministic ground-truth byte for global file offset `off`.
fn pat(off: u64) -> u8 {
    ((off % 251) as u8) ^ (((off / 4096) % 7) as u8)
}

/// Write `len` bytes of the `pat` pattern starting at file offset 0, in
/// 512 KiB chunks (whole-block writes where possible).
async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        write_at(h, ino, off, &data).await;
        off += chunk as u64;
    }
}

async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

/// ALL counter-asserting phases in one fn (process-global counters).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranged_phases() {
    // ---- Phase A: the amplification contract. N disjoint cold 4 KiB
    // reads of ONE block ⇒ N ranged device ops of ~4 KiB each; the block
    // is never fetched whole, never published to any tier.
    let h = make_with("524288", *b"ranged-a-pr6-v30", "rng_ns_a").await;
    let ino = create(&h, "rng_a").await;
    write_pattern(&h, ino, 12 * BS).await;
    let map = make_cold(&h, ino).await;
    let k2 = map.get(&2).expect("block 2 mapped").clone();

    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    let rb0 = METRICS.ranged_read_bytes.load(Ordering::Relaxed);
    let n = 8u64;
    for i in 0..n {
        let off = 2 * BS + i * 32_768; // disjoint, 4 KiB-aligned, one block
        let d = read_at(&h, ino, off, 4096).await;
        assert_eq!(d.len(), 4096);
        assert!(
            d.iter().enumerate().all(|(j, &x)| x == pat(off + j as u64)),
            "ranged read content at off {off}"
        );
    }
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        n,
        "N disjoint cold 4 KiB reads of one block must issue exactly N \
         device read ops (ranged ops count in get_obj by design, §5.6) — \
         a whole-block fetch here is the 1024x amplification this PR kills"
    );
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        n,
        "all N served via the ranged primitive"
    );
    assert_eq!(
        METRICS.ranged_read_bytes.load(Ordering::Relaxed) - rb0,
        n * 4096,
        "device bytes ≈ N x 4 KiB (aligned requests: window == request)"
    );
    assert!(
        !tier_has(&h, &k2),
        "ranged fills are NEVER published (whole-block tier-entry contract)"
    );
    assert!(
        h.fs.router.cache.hot_block.get_no_promote(&k2).is_none(),
        "ranged fills never land in the hot tier either"
    );

    // ---- Phase B: ghost convergence. The ranged touches above RECORDED
    // heat for k2; a whole-block-shaped read (> threshold ⇒ whole-block
    // path) now ghost-hits and publishes per §5.3 — genuinely hot ranges
    // converge to cached whole blocks.
    let adm0 = METRICS.read_tier_admissions.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(d.len(), BS as usize);
    assert!(
        d.iter()
            .enumerate()
            .all(|(j, &x)| x == pat(2 * BS + j as u64)),
        "whole-block convergence read content"
    );
    assert!(
        METRICS.read_tier_admissions.load(Ordering::Relaxed) > adm0,
        "the whole-block fetch after ranged heat must ghost-admit (§5.3)"
    );
    assert!(
        tier_has(&h, &k2),
        "converged block is tier-resident (published whole block)"
    );

    // ---- Phase C: threshold=0 kill switch — ranged never fires.
    drop(h);
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let h2 = make_with("524288", *b"ranged-c-pr6-v30", "rng_ns_c").await;
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
    let ino_c = create(&h2, "rng_c").await;
    write_pattern(&h2, ino_c, 6 * BS).await;
    make_cold(&h2, ino_c).await;
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    for i in 0..4u64 {
        let off = 3 * BS + i * 8192;
        let d = read_at(&h2, ino_c, off, 4096).await;
        assert!(
            d.iter().enumerate().all(|(j, &x)| x == pat(off + j as u64)),
            "kill-switch read content at off {off}"
        );
    }
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        0,
        "SQUEEZEFS_READ_RANGED_THRESHOLD=0 must disable ranged dispatch"
    );

    // ---- Phase D: compressed volume never ranges — decode needs the
    // whole physical block; the dispatch collapses at is_passthrough().
    drop(h2);
    let h3 = make_with("524288", *b"ranged-d-pr6-v30", "rng_ns_d").await;
    h3.fs.router.set_crypto(CryptoCompressState::new(
        "lz4".to_string(),
        "none".to_string(),
        None,
    ));
    let ino_d = create(&h3, "rng_d").await;
    // Compressible content, still position-dependent enough to catch
    // wrong-window serves.
    let mut data = vec![0u8; (6 * BS) as usize];
    for (i, b) in data.iter_mut().enumerate() {
        *b = ((i as u64) / 65536) as u8;
    }
    let mut off = 0u64;
    while off < 6 * BS {
        write_at(&h3, ino_d, off, &data[off as usize..(off + BS) as usize]).await;
        off += BS;
    }
    make_cold(&h3, ino_d).await;
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    for i in 0..4u64 {
        let off = 2 * BS + i * 8192;
        let d = read_at(&h3, ino_d, off, 4096).await;
        assert!(
            d.iter()
                .enumerate()
                .all(|(j, &x)| x == data[(off + j as u64) as usize]),
            "compressed-volume read must serve DECODED bytes at off {off}"
        );
    }
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        0,
        "compressed/encrypted volumes must never take the ranged path"
    );

    // ---- Phase E: zero-copy-leg alignment matrix (direct primitive
    // calls). Aligned request + RangedDest ⇒ no bounce, DMA into the
    // dest; any unaligned edge ⇒ exactly one counted bounce, bytes exact,
    // window padding never served.
    drop(h3);
    let h4 = make_with("524288", *b"ranged-e-pr6-v30", "rng_ns_e").await;
    let ino_e = create(&h4, "rng_e").await;
    write_pattern(&h4, ino_e, 6 * BS).await;
    let map_e = make_cold(&h4, ino_e).await;
    let k1 = map_e.get(&1).expect("block 1 mapped").clone();
    let path_e = squeezefs::keys::inode_path(ino_e);

    let layout = std::alloc::Layout::from_size_align(16384, 4096).unwrap();
    let dest_ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!dest_ptr.is_null());

    // (aligned offset, aligned len, dest) — the zero-copy leg.
    let b0 = METRICS
        .ranged_read_unaligned_bounces
        .load(Ordering::Relaxed);
    let val = h4
        .fs
        .router
        .get_block_range_for_index(
            &path_e,
            1,
            8192..8192 + 8192,
            Some(&k1),
            Some(RangedDest {
                ptr: dest_ptr,
                cap: 16384,
            }),
        )
        .await
        .unwrap()
        .expect("mapped block serves");
    assert_eq!(val.len(), 8192);
    assert!(
        val.iter()
            .enumerate()
            .all(|(j, &x)| x == pat(BS + 8192 + j as u64)),
        "zero-copy leg content"
    );
    let dest_slice = unsafe { std::slice::from_raw_parts(dest_ptr, 8192) };
    assert!(
        dest_slice
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(BS + 8192 + j as u64)),
        "zero-copy leg DMAs into the offered dest"
    );
    assert_eq!(
        METRICS
            .ranged_read_unaligned_bounces
            .load(Ordering::Relaxed)
            - b0,
        0,
        "aligned request + dest must take the zero-copy leg (no bounce)"
    );

    // Unaligned offset ⇒ bounce; exact bytes; padding never served.
    for (rel_start, len) in [(4097u64, 4096usize), (8192, 4095), (12_289, 5000)] {
        let b0 = METRICS
            .ranged_read_unaligned_bounces
            .load(Ordering::Relaxed);
        let val = h4
            .fs
            .router
            .get_block_range_for_index(
                &path_e,
                1,
                rel_start..rel_start + len as u64,
                Some(&k1),
                None,
            )
            .await
            .unwrap()
            .expect("mapped block serves");
        assert_eq!(val.len(), len, "exact requested length, never the window");
        assert!(
            val.iter()
                .enumerate()
                .all(|(j, &x)| x == pat(BS + rel_start + j as u64)),
            "bounce-leg content (rel_start={rel_start} len={len})"
        );
        assert_eq!(
            METRICS
                .ranged_read_unaligned_bounces
                .load(Ordering::Relaxed)
                - b0,
            1,
            "unaligned edges take exactly one counted bounce"
        );
    }
    unsafe { std::alloc::dealloc(dest_ptr, layout) };
}

/// Byte-exactness across odd offsets/lens/EOF straddles — every ranged
/// serve must byte-match the written ground truth (the whole-block
/// reference the file was written from).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn byte_exactness_odd_offsets_and_eof() {
    let h = make_with("524288", *b"ranged-x-pr6-v30", "rng_ns_x").await;
    let ino = create(&h, "rng_x").await;
    let file_len = 5 * BS + 1234; // EOF mid-block, unaligned
    write_pattern(&h, ino, file_len).await;
    make_cold(&h, ino).await;

    let cases: &[(u64, u32)] = &[
        (0, 1),
        (1, 3),
        (4095, 2),               // straddles a 4 KiB boundary
        (4096, 4096),            // aligned
        (4097, 4096),            // off-by-one
        (BS - 3, 7),             // straddles a block boundary (multi-block arm)
        (2 * BS + 8191, 12_345), // odd everything
        (3 * BS + 65_537, 131_072),
        (file_len - 1, 1),      // last byte
        (file_len - 999, 999),  // tail run
        (file_len - 100, 4096), // straddles EOF: clamped short read
        (5 * BS, 1234),         // the whole partial tail block
    ];
    for &(off, len) in cases {
        let d = read_at(&h, ino, off, len).await;
        let expect_len = std::cmp::min(off + len as u64, file_len).saturating_sub(off) as usize;
        assert_eq!(d.len(), expect_len, "length at off={off} len={len}");
        assert!(
            d.iter().enumerate().all(|(j, &x)| x == pat(off + j as u64)),
            "content at off={off} len={len} — window padding or foreign \
             bytes served"
        );
    }

    // Whole-file reference read (multi-block whole path) agrees.
    let mut whole = Vec::new();
    let mut off = 0u64;
    while off < file_len {
        let chunk = std::cmp::min(BS, file_len - off) as u32;
        whole.extend_from_slice(&read_at(&h, ino, off, chunk).await);
        off += chunk as u64;
    }
    assert_eq!(whole.len() as u64, file_len);
    assert!(
        whole.iter().enumerate().all(|(j, &x)| x == pat(j as u64)),
        "whole-file reference"
    );
}

/// The pre-existing ciphertext-serve hole (§5.6 sibling-leg hygiene): the
/// raw full-block dest leg DMAs device bytes into the payload dest
/// without process_read. On a small-block COMPRESSED volume with a dest,
/// it would serve lz4 frames as file content. The leg must refuse
/// transform configs and fall through to the validated whole-block loop,
/// which decodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_dest_leg_refuses_transform_configs() {
    let block: u64 = 65_536;
    let h = make_with("65536", *b"ranged-h-pr6-v30", "rng_ns_h").await;
    h.fs.router.set_crypto(CryptoCompressState::new(
        "lz4".to_string(),
        "none".to_string(),
        None,
    ));
    let ino = create(&h, "rng_h").await;
    // Highly compressible plaintext: the stored block is an lz4 frame,
    // decisively different from the plaintext at every prefix.
    let plaintext = vec![0x42u8; block as usize];
    // > 4 MiB total so the layout promotes to striped blocks.
    for b in 0..80u64 {
        write_at(&h, ino, b * block, &plaintext).await;
    }
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);

    let layout = std::alloc::Layout::from_size_align(block as usize, 4096).unwrap();
    let dest_ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!dest_ptr.is_null());

    // Full-block read WITH a payload dest — the raw dest leg's exact
    // trigger shape (slice_start == 0 && slice_len == block_size && dest).
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy(&path, 2 * block, block as u32, Some(dest_ptr as u64))
            .await
            .unwrap();
    assert_eq!(data.len(), block as usize);
    assert!(
        data.iter().all(|&x| x == 0x42),
        "full-block dest read on a COMPRESSED volume must serve DECODED \
         bytes — raw device bytes here are the ciphertext/compressed-frame \
         serve hole (first bytes: {:02x?})",
        &data[..8]
    );
    unsafe { std::alloc::dealloc(dest_ptr, layout) };
}

/// Rebind-under-movement (the 074/075-family shape): ranged reads racing
/// COW overwrites must serve SOME complete version of their own block —
/// rebind-or-current, never another block's bytes, never a freed key's
/// stale incarnation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebind_under_movement_never_foreign_bytes() {
    let h = Arc::new(make_with("524288", *b"ranged-m-pr6-v30", "rng_ns_m").await);
    let blocks = 6u64;
    let ino = create(&h, "rng_m").await;
    // Version 1: block b uniformly (100 + b).
    for b in 0..blocks {
        write_at(&h, ino, b * BS, &vec![100 + b as u8; BS as usize]).await;
    }
    make_cold(&h, ino).await;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Readers: ranged 4 KiB reads at fixed offsets; every byte must
    // belong to THIS block's version set.
    let mut readers = Vec::new();
    for r in 0..3u64 {
        let h = h.clone();
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let b = (i + r) % blocks;
                let off = b * BS + (((i * 7919 + r * 4096) % (BS - 4096)) & !4095);
                let d = h.fs.read(h.req, ino, 0, off, 4096, 0).await.unwrap().data;
                let allowed = [100 + b as u8, 150 + b as u8, 200 + b as u8];
                for (j, &x) in d.iter().enumerate() {
                    assert!(
                        allowed.contains(&x),
                        "foreign bytes served: block {b} off {off} byte {j} = {x:#04x} \
                         (allowed {allowed:?}) — a stale key incarnation or another \
                         block's content leaked through a ranged serve"
                    );
                }
                i += 1;
                if i.is_multiple_of(16) {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    // Writer: two full COW overwrite passes (displace→free→reallocate
    // churn), fsync between versions.
    for (pass, base) in [(2u64, 150u8), (3u64, 200u8)] {
        for b in 0..blocks {
            write_at(&h, ino, b * BS, &vec![base + b as u8; BS as usize]).await;
        }
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        let _ = pass;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    stop.store(true, Ordering::Relaxed);
    for t in readers {
        t.await.unwrap();
    }
}
