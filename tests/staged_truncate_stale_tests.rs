//! Staged truncate-shrink must be effective on EVERY tier — the aged-fsx
//! stale-resurrection corruption (found by the follow-up C aged-daemon
//! repro: fsx `READ BAD DATA` at op 64, bad bytes = op-24 stamps that a
//! TRUNCATE DOWN → TRUNCATE UP cycle should have zeroed).
//!
//! Mechanism (pre-existing in `truncate_layout`'s staged arm):
//!
//! * **Leg A — swallowed ring refusal.** The shrink re-staged the clipped
//!   image through `stage_write`, whose same-key replace needs a FRESH
//!   segment extent (crash-torn protection: the old copy stays live until
//!   the new one is written). Under a full/fragmented ring the segment
//!   REFUSES, the truncate swallowed the error (`let _ =`), and the
//!   full-length stale blob survived. The next truncate-UP then re-raised
//!   `meta.size` over the stale tail — every consumer clamps to
//!   `meta.size`, so the dead bytes became servable content.
//!
//! * **Leg B — promoted no-op.** If the blob had been promoted (ring entry
//!   removed, durable `block_map[0]` whole image), the shrink silently
//!   no-oped: the durable image kept the pre-truncate tail (block 0 starts
//!   at 0 — the `block_start >= new_size` prune never touches it), and the
//!   truncate-up codified it the same way.
//!
//! Contract pinned here: after `truncate(down)` → `truncate(up)`, the
//! range `[down, up)` reads ZEROS — regardless of ring pressure, ring
//! fragmentation, or promotion state — and a later sub-block RMW write
//! neither resurrects stale bytes nor regrows the file size.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// Default 4 MiB blocks (files below stay STAGED); `staging_write_budget`
/// sizes the ring so tests can construct refusal/promotion deterministically.
async fn make(uuid: [u8; 16], alloc_ns: &str, staging_write_budget: &str) -> H {
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(staging_write_budget),
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

async fn read_all(h: &H, ino: u64, len: usize) -> Vec<u8> {
    let r = h.fs.read(h.req, ino, 0, 0, len as u32, 0).await.unwrap();
    r.data.to_vec()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 | 1).collect()
}

/// Assert `got == want` and report the FIRST divergent offset (not a
/// half-megabyte dump) so a stale-resurrection failure is legible.
fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length mismatch");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at offset {i}: got 0x{:02x}, want 0x{:02x} \
             (stale pre-truncate bytes resurrected through the truncate cycle)",
            got[i], want[i]
        );
    }
}

/// Wait (bounded) for an observable condition — promotion visibility, never
/// a sleep-for-sync.
async fn await_condition<F>(mut cond: F, what: &str)
where
    F: FnMut() -> bool,
{
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// Leg A: ring-resident blob, shrink re-stage REFUSED by segment
/// fragmentation. Geometry (1 MiB single-shard ring): A = 600 KiB at the
/// head, B = 300 KiB behind it — every candidate extent for a ≥ 400 KiB
/// replacement (tail ~120 KiB, B-hole ≤ 300 KiB if B promotes) is too
/// small, so the old code's `stage_write` shrink deterministically refused
/// regardless of B's promotion timing, and the swallowed error left the
/// 600 KiB stale blob to be codified by the truncate-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trunc_cycle_reads_zeros_under_ring_fragmentation() {
    const A_LEN: usize = 600 * 1024;
    const B_LEN: usize = 300 * 1024;
    const DOWN: u64 = 400 * 1024;
    const UP: u64 = 550 * 1024;

    let h = make(*b"stagtrunc-lega01", "sttr_ns_a", "1MB").await;
    let a = create(&h, "frag_a").await;
    let b = create(&h, "frag_b").await;

    let pat = pattern(A_LEN);
    write_at(&h, a, 0, &pat).await;
    write_at(&h, b, 0, &vec![0xBBu8; B_LEN]).await;

    let path = squeezefs::keys::inode_path(a);
    assert_eq!(
        h.fs.router.fetch_metadata(&path).await.unwrap().file_type,
        "staged",
        "fixture must exercise the STAGED truncate path"
    );

    truncate_to(&h, a, DOWN).await;
    truncate_to(&h, a, UP).await;

    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.size, UP, "logical size must track the truncate-up");

    let got = read_all(&h, a, UP as usize).await;
    let mut want = pat[..DOWN as usize].to_vec();
    want.resize(UP as usize, 0);
    assert_bytes(&got, &want, "leg A (ring fragmentation) truncate cycle");
}

/// Leg B: the blob was PROMOTED (ring entry gone, durable whole image in
/// `block_map[0]`) before the truncate — the shrink must clip the durable
/// image too, or the truncate-up serves its stale tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trunc_cycle_reads_zeros_after_promotion() {
    const A_LEN: usize = 800 * 1024; // > high water (768 KiB of 1 MiB) => promoted
    const DOWN: u64 = 300 * 1024;
    const UP: u64 = 600 * 1024;

    let h = make(*b"stagtrunc-legb01", "sttr_ns_b", "1MB").await;
    let a = create(&h, "promo_a").await;
    let pat = pattern(A_LEN);
    write_at(&h, a, 0, &pat).await;

    let path = squeezefs::keys::inode_path(a);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.file_type, "staged");
    let file_id = meta.file_id.clone().expect("staged file has a file_id");

    // The 800 KiB stage crosses the ring's high-water mark, so the merge
    // worker promotes it; wait until the durable mapping is published AND
    // the ring entry is released (the ring-miss truncate shape).
    await_condition(
        || {
            let promoted =
                h.fs.router
                    .metadata_cache
                    .get(&path)
                    .and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned()))
                    .is_some();
            promoted && h.fs.router.cache.nvme.read_staged(&file_id).is_none()
        },
        "staged blob promoted to a durable block and released from the ring",
    )
    .await;

    truncate_to(&h, a, DOWN).await;
    truncate_to(&h, a, UP).await;

    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.size, UP, "logical size must track the truncate-up");

    let got = read_all(&h, a, UP as usize).await;
    let mut want = pat[..DOWN as usize].to_vec();
    want.resize(UP as usize, 0);
    assert_bytes(&got, &want, "leg B (promoted durable image) truncate cycle");
}

/// The aged-storm interleave: a staged file under CONSTANT background
/// promotion churn (tiny ring — every stage crosses the high-water mark and
/// self-enqueues promotion; the merge worker races every op) while the
/// foreground runs fsx-shaped truncate-down / extend / RMW cycles. Every
/// cycle verifies the re-exposed range reads zeros and the prefix survives —
/// the daemon-side corruption reproduced by the aged fsx storm entered
/// through exactly this promote-vs-truncate window.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn trunc_cycles_under_promotion_churn_never_resurrect() {
    const FULL: usize = 800 * 1024; // > high water (768 KiB of 1 MiB budget)
    const ROUNDS: usize = 60;

    let h = make(*b"stagtrunc-churn1", "sttr_ns_e", "1MB").await;
    let a = create(&h, "churn_a").await;
    let path = squeezefs::keys::inode_path(a);

    let mut x = 0x243F_6A88_85A3_08D3u64; // deterministic LCG offsets
    for round in 0..ROUNDS {
        // Full-length rewrite with a round-tagged pattern (over_cap stalls
        // are fine — they exercise the promotion kick harder).
        let mut pat = pattern(FULL);
        for b in pat.iter_mut() {
            *b = b.wrapping_add(round as u8);
        }
        write_at(&h, a, 0, &pat).await;

        // fsx-shaped cycle: truncate down into the body, then re-expose via
        // an extending sub-block RMW write (the copy_file_range dest shape).
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let down = 128 * 1024 + (x % (256 * 1024)); // [128K, 384K)
        let woff = FULL as u64 - 32 * 1024 + (x % 8192); // extend near old EOF
        truncate_to(&h, a, down).await;
        write_at(&h, a, woff, &[0xEE; 4096]).await;

        let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
        assert_eq!(meta.size, woff + 4096, "round {round}: size after cycle");

        let got = read_all(&h, a, (woff + 4096) as usize).await;
        let mut want = pat[..down as usize].to_vec();
        want.resize((woff + 4096) as usize, 0);
        want[woff as usize..].fill(0xEE);
        if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
            panic!(
                "round {round}: first mismatch at offset {i} (down={down}, woff={woff}): \
                 got 0x{:02x}, want 0x{:02x} — stale bytes resurrected through the \
                 promote-vs-truncate window",
                got[i], want[i]
            );
        }
    }
}

/// The in-place shrink primitive itself: patches `original_size` under the
/// shard write lock without moving the extent (readers decode the clipped
/// length; payload prefix intact), refuses growth and absent keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ring_in_place_shrink_clips_reads_and_refuses_growth() {
    use squeezefs::tiering::nvme::NvmeCache;

    let dir = tempdir().unwrap();
    let cache = Arc::new(NvmeCache::new(&[dir.path()], &[4 * 1024 * 1024], 1).unwrap());
    let key = bytes::Bytes::from_static(b"staged-shrink-target");

    // Staged frame: value = [meta_len u64 BE][fencing u64][original_size u64]
    // [path_len u32][path][payload].
    let payload = pattern(128 * 1024);
    let path = b"inode_42";
    let mut meta = Vec::new();
    meta.extend_from_slice(&7u64.to_be_bytes()); // fencing token
    meta.extend_from_slice(&(payload.len() as u64).to_be_bytes()); // original_size
    meta.extend_from_slice(&(path.len() as u32).to_be_bytes());
    meta.extend_from_slice(path);
    assert!(cache.reserve_and_write(key.clone(), meta.len() as u64, &meta, &payload, None));

    let read_original_size = |cache: &Arc<NvmeCache>, key: &bytes::Bytes| -> u64 {
        let guard = cache.get(key).expect("entry resident");
        let val = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        u64::from_be_bytes(val[16..24].try_into().unwrap())
    };
    assert_eq!(read_original_size(&cache, &key), payload.len() as u64);

    // Shrink: readers decode the clipped length; prefix bytes intact.
    assert!(cache.shrink_staged_value(&key, 64 * 1024));
    assert_eq!(read_original_size(&cache, &key), 64 * 1024);
    {
        let guard = cache.get(&key).expect("entry resident");
        let val = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        let data_start = 8 + meta.len();
        assert_eq!(
            &val[data_start..data_start + 64 * 1024],
            &payload[..64 * 1024],
            "clip must not disturb the surviving payload prefix"
        );
    }

    // Equal = benign no-op; growth = refused; absent key = refused.
    assert!(cache.shrink_staged_value(&key, 64 * 1024));
    assert!(
        !cache.shrink_staged_value(&key, 128 * 1024),
        "growing would expose bytes beyond the written payload"
    );
    assert!(!cache.shrink_staged_value(&bytes::Bytes::from_static(b"absent"), 0));
}

/// The fsx shape end-to-end: a sub-block RMW write AFTER the truncate cycle
/// must seed from the clipped image — the old code seeded from the stale
/// full-length blob, resurrecting the dead bytes AND regrowing the file to
/// its pre-truncate length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rmw_after_trunc_cycle_stays_clipped_and_sized() {
    const A_LEN: usize = 600 * 1024;
    const B_LEN: usize = 300 * 1024;
    const DOWN: u64 = 400 * 1024;
    const UP: u64 = 550 * 1024;
    const WOFF: u64 = 500 * 1024;
    const WLEN: usize = 4096;

    let h = make(*b"stagtrunc-rmw001", "sttr_ns_c", "1MB").await;
    let a = create(&h, "rmw_a").await;
    let b = create(&h, "rmw_b").await;

    let pat = pattern(A_LEN);
    write_at(&h, a, 0, &pat).await;
    write_at(&h, b, 0, &vec![0xBBu8; B_LEN]).await;

    truncate_to(&h, a, DOWN).await;
    truncate_to(&h, a, UP).await;

    // Sub-block RMW into the post-truncate hole.
    write_at(&h, a, WOFF, &[0xEEu8; WLEN]).await;

    let path = squeezefs::keys::inode_path(a);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        meta.size, UP,
        "an in-bounds RMW write must not regrow the file to its \
         pre-truncate length (stale seed = stale size)"
    );

    let got = read_all(&h, a, UP as usize).await;
    let mut want = pat[..DOWN as usize].to_vec();
    want.resize(UP as usize, 0);
    want[WOFF as usize..WOFF as usize + WLEN].fill(0xEE);
    assert_bytes(&got, &want, "RMW after truncate cycle");
}
