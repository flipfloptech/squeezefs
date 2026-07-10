//! Regression harness for the REUSED-KEY STALE-FILL family (block-index→key
//! BINDING ABA — the `8e3995e` follow-up; the rare generic/075.2 soak shape).
//!
//! Block keys are plain device-offset strings. The incarnation seqlock
//! (`block_allocator.rs` / `incarnation_core.rs`) validates KEY↔CONTENT for
//! cache publishes: a fill may cache bytes only when the key's incarnation was
//! stable and unchanged across its device read. It cannot protect a reader
//! whose BLOCK-INDEX→KEY resolution went stale: after a displace→free→realloc
//! cycle the key legitimately holds the NEW owner's bytes, a fill of those
//! bytes validates perfectly — and is then served for the WRONG block index.
//!
//! The soak shape this pins (observed ~3/30 of 10,000-op fsx-075 runs): a
//! striped read resolves `map[b0] = X`, parks behind the striped-I/O
//! admission semaphore; concurrent writeback COW-rewrites b0 (displacing and
//! freeing X) and then b1 (the allocator hands X to b1); the parked read
//! finally fetches X and returns **b1's bytes at b0's file offsets** — stale
//! data exactly one block over, at the same intra-block offset. The sibling
//! sub-shape: a stale binding to a freed-but-not-yet-reused key serves the
//! punched device range (zeros where written data must be).
//!
//! Contract pinned here: a striped read/RMW-seed may serve bytes for block
//! `b` only if, once the bytes are in hand, the CURRENT block map still binds
//! `b` to the key they came from (and, for device fills, the key's
//! incarnation did not move across the read). Anything else must re-resolve
//! and retry — never serve another block's content.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{
    storage::MetaLvStorage, MetaLvBackend, RoutedMetaBackend, VolumeBackend,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    V2,
    V3,
}

/// 64 KiB blocks: small enough that multi-block striped files are cheap,
/// matching the write_visibility harness geometry.
const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(fmt: Fmt) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "stalefill_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
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
    let routed: Arc<RoutedMetaBackend> = match fmt {
        Fmt::V2 => {
            let ms = MetaLvStorage::open(m.path(), 128 * 1024 * 1024).unwrap();
            MetaLvBackend::format_v2_for_tests(&ms, true, true, None)
                .await
                .unwrap();
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(
                Arc::new(MetaLvBackend::new(ms)),
            )]))
        }
        Fmt::V3 => {
            ImageBuilder::new(BuilderConfig {
                node_size: DEFAULT_NODE_SIZE,
                journal_len_override: None,
                hash_seed: 0xC0FF_EE00_1234_5678,
                uuid: *b"stalefill-regrv3",
            })
            .unwrap()
            .build(m.path(), 128 * 1024 * 1024)
            .await
            .unwrap();
            let be = KvMetaBackend::open(m.path()).await.unwrap();
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(be)]))
        }
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
    h.fs.read(h.req, ino, 0, off, size)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn truncate_to(h: &H, ino: u64, size: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// Current block map of `ino` as the write paths just published it.
async fn block_map_of(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

/// Drain every currently-available striped-I/O admission permit so multi-block
/// read tasks park between their binding resolution and their block fetch —
/// the exact suspension the kernel-writeback race produces.
fn drain_striped_io_permits() -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let sem = squeezefs::bg_admit::STRIPED_IO_SEM.clone();
    let mut held = Vec::new();
    while let Ok(p) = sem.clone().try_acquire_owned() {
        held.push(p);
    }
    held
}

// ---------------------------------------------------------------------------
// 1. THE 075.2 soak shape, deterministic: a striped read whose binding
//    resolution (b0→X, b1→W) is suspended across a COW rewrite of both blocks
//    must serve the blocks' CURRENT content — never X's new owner's bytes
//    (b1's data one block over) and never W's punched range (zeros).
// ---------------------------------------------------------------------------

async fn parked_read_never_serves_reused_or_freed_key(fmt: Fmt) {
    let tag = format!("{fmt:?}/parked-read-aba");
    let h = make(fmt).await;
    let ino = create(&h, "aba").await;

    // Striped 2-block file: b0 = 0xA0, b1 = 0xB0 (full-block write-through).
    let mut initial = vec![0xA0u8; BS as usize];
    initial.extend_from_slice(&vec![0xB0u8; BS as usize]);
    write_at(&h, ino, 0, &initial).await;

    let m0 = block_map_of(&h, ino).await;
    let x = m0.get(&0).cloned().expect("b0 mapped after striped write");
    assert!(m0.contains_key(&1), "b1 mapped after striped write");

    // Park a 2-block read between resolution and fetch: drain the striped-I/O
    // admission pool, then issue the read. Its binding snapshot (b0→X, b1→W)
    // is taken immediately (pure RAM); its per-block tasks then park on the
    // drained semaphore — the read cannot complete while we hold the permits,
    // which is the proof it parked with the pre-rewrite bindings.
    let held = drain_striped_io_permits();
    assert!(
        !held.is_empty(),
        "[{tag}] harness: no striped-I/O permits to drain"
    );
    let read_task = tokio::spawn({
        let fs = h.fs.clone();
        let req = h.req;
        async move { fs.read(req, ino, 0, 0, (BS + 16) as u32).await }
    });
    // Let the read run to its park point (resolution is RAM-only; the first
    // await that can suspend it for long is the permit acquire).
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        !read_task.is_finished(),
        "[{tag}] harness: read completed while all striped-I/O permits were held"
    );

    // COW-rewrite b0 (0xA1): displaces X from map[b0] and frees it — the
    // free list now holds exactly X.
    write_at(&h, ino, 0, &vec![0xA1u8; BS as usize]).await;
    // COW-rewrite b1 (0xB1): the allocator hands X to b1; b1's bytes are
    // DMA'd into X's device range and map[b1] = X is published.
    write_at(&h, ino, BS, &vec![0xB1u8; BS as usize]).await;

    // Premise of the repro (allocator reuse determinism): X now belongs to b1.
    let m1 = block_map_of(&h, ino).await;
    assert_eq!(
        m1.get(&1),
        Some(&x),
        "[{tag}] harness premise broke: rewrite of b1 did not reuse b0's freed key {x} \
         (allocator policy changed? map now {m1:?})"
    );
    assert_ne!(
        m1.get(&0),
        Some(&x),
        "[{tag}] harness premise broke: b0 still maps to {x}"
    );

    // Release the permits: the parked read resumes with its STALE bindings.
    drop(held);
    let got = read_task
        .await
        .unwrap()
        .expect("parked read must succeed")
        .data
        .to_vec();
    assert_eq!(got.len(), (BS + 16) as usize, "[{tag}] short read");

    // b0's range must be its current content (0xA1). On the broken build the
    // stale binding b0→X device-reads X — which now holds B1'S bytes — and
    // serves 0xB1 at b0's offsets: stale data exactly one block over, at the
    // same intra-block offset (the fsx-075.2 soak signature).
    if let Some(pos) = got[..BS as usize].iter().position(|&v| v != 0xA1) {
        panic!(
            "[{tag}] REUSED-KEY STALE FILL: byte at file offset {pos:#x} = {:#x} \
             (expected 0xA1; 0xB1 means block 1's bytes served for block 0 \
             through reused key {x})",
            got[pos]
        );
    }
    // b1's slice must be its current content (0xB1). On the broken build the
    // stale binding b1→W device-reads W — displaced, freed, hole-punched and
    // never reallocated — and serves zeros where written data must be.
    if let Some(pos) = got[BS as usize..].iter().position(|&v| v != 0xB1) {
        panic!(
            "[{tag}] FREED-KEY STALE FILL: byte at file offset {:#x} = {:#x} \
             (expected 0xB1; 0x00 means the punched dead key was served for block 1)",
            BS as usize + pos,
            got[BS as usize + pos]
        );
    }

    // The averted serve must not have poisoned anything: a fresh read is the
    // current content too.
    let fresh = read_at(&h, ino, 0, (2 * BS) as u32).await;
    assert!(
        fresh[..BS as usize].iter().all(|&v| v == 0xA1)
            && fresh[BS as usize..].iter().all(|&v| v == 0xB1),
        "[{tag}] post-race fresh read corrupted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_read_never_serves_reused_or_freed_key_v2() {
    parked_read_never_serves_reused_or_freed_key(Fmt::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_read_never_serves_reused_or_freed_key_v3() {
    parked_read_never_serves_reused_or_freed_key(Fmt::V3).await;
}

// ---------------------------------------------------------------------------
// 2. The RAM-meta REVERT that widens the stale-binding window from
//    microseconds to a full TTL second: `update_metadata_cache_size` is a
//    get→mutate→insert on the shared `metadata_cache` with no serialization
//    against merges. When its snapshot is taken (or its refill await
//    completes) just before a concurrent block-map merge publishes, its
//    insert resurrects the PRE-MERGE map — with a fresh `cached_at`, so every
//    reader for up to a second resolves block→key bindings whose keys are
//    displaced, freed, and up for reallocation. The write path calls it after
//    every staged write (`expected_new_size`), so the fsx soak exercises this
//    constantly.
//
//    Contract: after any interleaving of {size bump} × {COW merge}, the RAM
//    metadata entry's block map must equal the backend's — a size bump must
//    never revert a merged map. (The lost-update window is scheduler-
//    dependent — OS preemption between the bump's get and insert — so this
//    hammer is a contract pin more than a deterministic red; the
//    deterministic reds for the family are tests 1 and 3.)
// ---------------------------------------------------------------------------

async fn size_bump_never_reverts_merged_map(fmt: Fmt) {
    let tag = format!("{fmt:?}/size-bump-revert");
    let h = Arc::new(make(fmt).await);
    let ino = create(&h, "revert").await;
    let path = squeezefs::keys::inode_path(ino);

    // Striped 2-block file.
    let mut initial = vec![0xA0u8; BS as usize];
    initial.extend_from_slice(&vec![0xB0u8; BS as usize]);
    write_at(&h, ino, 0, &initial).await;

    for i in 0..600u64 {
        // Force the size-bump through its refill leg (get-miss → awaited
        // fetch_metadata → mutate → insert), mirroring the write handler's
        // per-write `expected_new_size` call under an eviction/miss.
        h.fs.router.metadata_cache.invalidate(&path);

        let bump = {
            let h = h.clone();
            let path = path.clone();
            // Monotonically-growing target so the `size >` gate always takes
            // the insert leg (the handler passes offset+len of a real write).
            let target = 2 * BS + i + 1;
            async move {
                h.fs.router.update_metadata_cache_size(&path, target).await;
            }
        };
        let rewrite = {
            let h = h.clone();
            let fill = vec![(i % 0xFE) as u8 + 1; BS as usize];
            async move {
                // COW-rewrite b0: displaces + frees its key and publishes a
                // new binding the reverted entry would roll back.
                write_at(&h, ino, 0, &fill).await;
            }
        };
        let (a, b) = tokio::join!(tokio::spawn(bump), tokio::spawn(rewrite));
        a.unwrap();
        b.unwrap();

        // The RAM entry (what every read resolves bindings from for the next
        // TTL second) must match the merged backend map: a lost-update insert
        // resurrects b0's DISPLACED key — already freed and first in line for
        // reallocation to the next COW write of ANY block.
        let ram_map =
            h.fs.router
                .metadata_cache
                .get(&path)
                .and_then(|m| m.block_map);
        if let Some(ram_map) = ram_map {
            h.fs.router.metadata_cache.invalidate(&path);
            let backend_map = block_map_of(&h, ino).await;
            assert_eq!(
                ram_map.get(&0),
                backend_map.get(&0),
                "[{tag}] SIZE-BUMP MAP REVERT (iter {i}): RAM metadata_cache holds {:?} \
                 for b0 while the merged map says {:?} — every read for the next TTL \
                 second resolves a stale (freed, reallocatable) binding",
                ram_map.get(&0),
                backend_map.get(&0)
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_bump_never_reverts_merged_map_v2() {
    size_bump_never_reverts_merged_map(Fmt::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_bump_never_reverts_merged_map_v3() {
    size_bump_never_reverts_merged_map(Fmt::V3).await;
}

// ---------------------------------------------------------------------------
// 3. fsx-shaped in-process stress: concurrent COW rewrites recycling block
//    offsets as fast as possible, readers validating structural block
//    identity, and a permit "valve" repeatedly draining/releasing the
//    striped-I/O admission pool so reads keep getting suspended between
//    resolution and fetch. Every 8-byte word of block `b` is stamped with
//    `b`; a reader observing another block's stamp inside `b`'s range is the
//    ABA. Zeros are legal (holes / truncate races); foreign stamps never are.
// ---------------------------------------------------------------------------

const STRESS_BLOCKS: u64 = 6;

fn stamp_block(b: u64, generation: u64) -> Vec<u8> {
    let word = (b << 32) | (generation & 0xFFFF_FFFF);
    let mut out = Vec::with_capacity(BS as usize);
    while out.len() < BS as usize {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Validate `data`, which was read from file offset `file_off`: every aligned
/// 8-byte word must be zero (hole) or carry the stamp of the block it lies in.
fn check_stamps(data: &[u8], file_off: u64, tag: &str) {
    for (i, w) in data.chunks_exact(8).enumerate() {
        let off = file_off + (i as u64) * 8;
        if !off.is_multiple_of(8) {
            continue;
        }
        let word = u64::from_le_bytes(w.try_into().unwrap());
        if word == 0 {
            continue;
        }
        let want_b = off / BS;
        let got_b = word >> 32;
        assert_eq!(
            got_b,
            want_b,
            "[{tag}] BLOCK-IDENTITY VIOLATION: word at file offset {off:#x} \
             stamps block {got_b} (gen {}) inside block {want_b}'s range — \
             a reused/freed key served another block's bytes",
            word & 0xFFFF_FFFF
        );
    }
}

async fn stress_recycled_keys(fmt: Fmt) {
    let tag = format!("{fmt:?}/stress-recycle");
    let h = Arc::new(make(fmt).await);
    let ino = create(&h, "stress").await;

    // Seed all blocks (striped, write-through).
    for b in 0..STRESS_BLOCKS {
        write_at(&h, ino, b * BS, &stamp_block(b, 0)).await;
    }

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // 3 full-block rewriters (COW: every rewrite displaces + frees + lets the
    // allocator recycle an offset to the next writer).
    for w in 0..3u64 {
        let h = h.clone();
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..200u64 {
                let b = (w + i * 3) % STRESS_BLOCKS;
                write_at(&h, ino, b * BS, &stamp_block(b, i + 1)).await;
            }
            if w == 0 {
                done.store(true, std::sync::atomic::Ordering::Release);
            }
        }));
    }

    // 1 partial-block writer (drives the RMW-seed path under recycling).
    {
        let h = h.clone();
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let b = i % STRESS_BLOCKS;
                let half = stamp_block(b, 0x8000_0000 | i)[..BS as usize / 2].to_vec();
                write_at(&h, ino, b * BS + BS / 2, &half).await;
                i += 1;
            }
        }));
    }

    // 1 truncate/re-extend cycler (recycles many offsets at once).
    {
        let h = h.clone();
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                truncate_to(&h, ino, 2 * BS).await;
                for b in 2..STRESS_BLOCKS {
                    write_at(&h, ino, b * BS, &stamp_block(b, 0x4000_0000 | i)).await;
                }
                i += 1;
            }
        }));
    }

    // 2 readers validating structural block identity.
    for r in 0..2u64 {
        let h = h.clone();
        let done = done.clone();
        let tag = tag.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let start_b = (r + i) % (STRESS_BLOCKS - 1);
                let blocks = 1 + (i % 3);
                let off = start_b * BS;
                let len = (blocks * BS).min(STRESS_BLOCKS * BS - off);
                let data = read_at(&h, ino, off, len as u32).await;
                check_stamps(&data, off, &tag);
                i += 1;
            }
        }));
    }

    // 1 admission valve: repeatedly drain + release the striped-I/O pool so
    // reads park between binding resolution and block fetch.
    {
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let held = drain_striped_io_permits();
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                drop(held);
                tokio::task::yield_now().await;
            }
        }));
    }

    for t in tasks {
        t.await.expect("stress task panicked");
    }

    // Final full verification.
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let data = read_at(&h, ino, 0, (STRESS_BLOCKS * BS) as u32).await;
    check_stamps(&data, 0, &tag);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn stress_recycled_keys_v2() {
    stress_recycled_keys(Fmt::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn stress_recycled_keys_v3() {
    stress_recycled_keys(Fmt::V3).await;
}
