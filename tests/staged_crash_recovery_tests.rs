//! Staged-mmap crash-recovery contract suite (the SIGKILL → remount → EIO
//! class observed since the 37fe5eb-era runs).
//!
//! The staging ring is the SOLE copy of acked-but-not-yet-promoted staged
//! payloads. Kill-9 (D0) semantics for it:
//!
//! - **acked+fsynced** staged data is `fsync`-durable ON THE RING: fsync
//!   syncs the ring shard and commits the staged layout, it does NOT
//!   promote (promotion happens at the mount's clean unmount or under
//!   staging-pool pressure — `.benchmarks/2026-09-09-dismount-staged-residue.md`
//!   §1.2/§7), so after a crash the same-root remount recovers it
//!   byte-exact from the ring;
//! - **acked-unfsynced** staged data MAY be lost by the crash, but a
//!   remount must never turn it into an error: recover what is intact,
//!   discard-and-log what is torn, and reads of a staged file whose
//!   payload is gone degrade to CONSISTENT ZEROS — never EIO/ENOENT.
//!
//! Contracts pinned here:
//! 1. `recover_index` rebuilds the segment index with the WRITERS' true
//!    block geometry (value 4 KiB-aligned past header+key; footprint =
//!    aligned value delta + value length). The historical scan understated
//!    every footprint by the alignment gap, so post-recovery writes were
//!    first-fit-placed INSIDE recovered values — clobbering the sole copy
//!    of staged data after every crash remount.
//! 2. The recovery scan never mis-syncs into value interiors: payload
//!    bytes that alias the block magic must not fabricate entries (the
//!    historical byte-wise crawl fabricated one and then skipped the next
//!    REAL entry — a lost staged file = EIO on read).
//! 3. A corrupted/torn entry header is discarded loudly; entries after it
//!    are still recovered.
//! 4. Router read contract: a `staged` file whose ring entry is gone (torn
//!    → discarded, or never flushed) and which has no promoted mapping
//!    reads as size-consistent ZEROS, never an error.
//! 5. End-to-end kill-9: SIGKILL a daemon with resident staged data,
//!    remount over the same staging dir (same generation), and every
//!    staged file reads back byte-exact — including AFTER new staged
//!    writes land post-recovery (contract 1's clobber is the regression).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{DataRouter, LayoutMetadata};
use std::ffi::OsStr;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Router block size (staged threshold when staging dirs exist).
const BLOCK: usize = 64 * 1024;
/// Staged payload size: > MAX_INLINE_SIZE (4 KiB), < BLOCK, and shaped so
/// the header+key+value footprint has a non-trivial alignment remainder
/// (the clobber window of contract 1).
const STAGED_LEN: usize = 16000;
/// Files staged by the kill-9 child / the post-recovery probe burst.
const KILL_FILES: usize = 40;
/// The generation string both sides of the kill-9 test bind staging to.
const GEN: &str = "staged-crash-gen";

/// Deterministic per-(salt, file, position) content whose consecutive-byte
/// deltas are constant (+7 mod 251), so the segment BLOCK magic byte
/// sequence can never occur accidentally inside a payload.
fn pattern(salt: usize, idx: usize, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (i, x) in v.iter_mut().enumerate() {
        *x = (salt
            .wrapping_mul(131)
            .wrapping_add(idx.wrapping_mul(31))
            .wrapping_add(i.wrapping_mul(7))
            % 251) as u8;
    }
    v
}

fn assert_bytes_exact(want: &[u8], got: &[u8], what: &str) {
    if got != want {
        let first_diff = got
            .iter()
            .zip(want.iter())
            .position(|(g, w)| g != w)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "{what}: wrong bytes (len got={} want={}, first diff at byte {first_diff}: \
             got=0x{:02x} want=0x{:02x})",
            got.len(),
            want.len(),
            got.get(first_diff).copied().unwrap_or(0),
            want.get(first_diff).copied().unwrap_or(0),
        );
    }
}

// ===========================================================================
// Contracts 1–3: segment-level index recovery (NvmeCache directly).
// ===========================================================================

use squeezefs::tiering::nvme::NvmeCache;

/// One-shard file-backed NvmeCache over `dir` (capacity ≥ 4096 ⇒ the
/// production 4 KiB block alignment applies).
fn one_shard_cache(dir: &std::path::Path, capacity: usize) -> NvmeCache {
    NvmeCache::new(&[dir], &[capacity], 1).expect("NvmeCache::new")
}

/// The exact value image `reserve_and_write(key, meta_len, meta, data)`
/// stores: `[meta_len: u64 BE][meta][data]` (no active-block padding).
fn value_image(meta_bytes: &[u8], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + meta_bytes.len() + data.len());
    v.extend_from_slice(&(meta_bytes.len() as u64).to_be_bytes());
    v.extend_from_slice(meta_bytes);
    v.extend_from_slice(data);
    v
}

fn cache_value(cache: &NvmeCache, key: &str) -> Option<Vec<u8>> {
    cache
        .get_static(&bytes::Bytes::copy_from_slice(key.as_bytes()))
        .map(|g| g.to_vec())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recover_index_true_geometry_survives_post_recovery_writes() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempdir().unwrap();
    let cap = 8 * 1024 * 1024;

    // Entry sizes chosen to straddle alignment remainders (contract 1's
    // clobber window exists whenever footprint % 4096 lands past the
    // header+key bytes).
    let sizes = [100usize, 4096, 5008, 16000, 12];
    let mut expected: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let cache = one_shard_cache(dir.path(), cap);
        for (i, &sz) in sizes.iter().enumerate() {
            let key = format!("staged_file_{i}");
            let data = pattern(3, i, sz);
            let admitted = cache.reserve_and_write(
                bytes::Bytes::copy_from_slice(key.as_bytes()),
                0,
                &[],
                &data,
                None,
            );
            assert!(admitted, "entry {i} must admit into an empty segment");
            expected.push((key, value_image(&[], &data)));
        }
        // Dropped WITHOUT any shutdown flush beyond the writers' own
        // MS_ASYNC msync — kill-9 semantics for the mmap (page cache
        // keeps the bytes; the index is RAM-only and dies).
    }

    // Remount: fresh cache over the same segment file, index recovered
    // from disk.
    let cache2 = one_shard_cache(dir.path(), cap);
    cache2.recover_index();

    for (key, want) in &expected {
        let got = cache_value(&cache2, key)
            .unwrap_or_else(|| panic!("recovered index lost entry {key} (pre-write)"));
        assert_bytes_exact(want, &got, &format!("recovered entry {key} (pre-write)"));
    }

    // Contract 1: post-recovery writes must respect recovered footprints.
    // With the historical understated lengths, the first-fit cursor lands
    // INSIDE the last recovered value and this burst clobbers it.
    let mut new_expected: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..3usize {
        let key = format!("post_recovery_{i}");
        let data = pattern(4, i, 6000);
        let admitted = cache2.reserve_and_write(
            bytes::Bytes::copy_from_slice(key.as_bytes()),
            0,
            &[],
            &data,
            None,
        );
        assert!(
            admitted,
            "post-recovery entry {i} must admit (8 MiB segment)"
        );
        new_expected.push((key, value_image(&[], &data)));
    }

    for (key, want) in &expected {
        let got = cache_value(&cache2, key).unwrap_or_else(|| {
            panic!("entry {key} vanished after post-recovery writes (index clobber)")
        });
        assert_bytes_exact(
            want,
            &got,
            &format!(
                "entry {key} after post-recovery writes — a diff here means recovery \
                 understated its footprint and a new entry was placed inside it"
            ),
        );
    }
    for (key, want) in &new_expected {
        let got = cache_value(&cache2, key)
            .unwrap_or_else(|| panic!("post-recovery entry {key} unreadable"));
        assert_bytes_exact(want, &got, &format!("post-recovery entry {key}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recover_index_ignores_false_magic_inside_value_interiors() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempdir().unwrap();
    let cap = 2 * 1024 * 1024;

    // Entry A: 12000-byte payload with a CRAFTED fake block header inside
    // it — magic + plausible key_len/val_len whose fake footprint would
    // skip past entry B. Payload bytes are user data: the scan must never
    // interpret them.
    let mut a_data = pattern(5, 0, 12000);
    // A's value region starts at file offset 4096 (aligned past header +
    // 2-byte key); its data starts at 4096 + 8 (meta_len framing) + 0.
    // Plant the fake header at file offset 13000 — past the historical
    // scan's understated skip (12 + 2 + 12008 = 12022) so the byte-wise
    // crawl walks straight into it.
    let fake_at = 13000 - (4096 + 8);
    a_data[fake_at..fake_at + 4].copy_from_slice(&0xCAFEBABEu32.to_le_bytes());
    a_data[fake_at + 4..fake_at + 8].copy_from_slice(&2u32.to_le_bytes()); // key_len
    a_data[fake_at + 8..fake_at + 12].copy_from_slice(&12000u32.to_le_bytes()); // val_len
                                                                                // And a second fake at an ALIGNED interior offset (8192).
    let fake_aligned = 8192 - (4096 + 8);
    a_data[fake_aligned..fake_aligned + 4].copy_from_slice(&0xCAFEBABEu32.to_le_bytes());
    a_data[fake_aligned + 4..fake_aligned + 8].copy_from_slice(&2u32.to_le_bytes());
    a_data[fake_aligned + 8..fake_aligned + 12].copy_from_slice(&500u32.to_le_bytes());

    let b_data = pattern(5, 1, 500);
    {
        let cache = one_shard_cache(dir.path(), cap);
        assert!(cache.reserve_and_write(bytes::Bytes::from_static(b"aa"), 0, &[], &a_data, None));
        assert!(cache.reserve_and_write(bytes::Bytes::from_static(b"bb"), 0, &[], &b_data, None));
    }

    let cache2 = one_shard_cache(dir.path(), cap);
    cache2.recover_index();

    // Contract 2: B must survive — the fake in-payload header must not
    // fabricate an entry whose bogus footprint skips B's real header.
    let got_b = cache_value(&cache2, "bb").unwrap_or_else(|| {
        panic!(
            "entry bb LOST after recovery — the scan fabricated an entry from payload \
             bytes and skipped the next real entry (a lost staged file = EIO on read)"
        )
    });
    assert_bytes_exact(&value_image(&[], &b_data), &got_b, "recovered entry bb");
    let got_a = cache_value(&cache2, "aa").expect("entry aa must survive recovery");
    assert_bytes_exact(&value_image(&[], &a_data), &got_a, "recovered entry aa");
    // And no fabricated keys.
    assert_eq!(
        cache2.list_keys().len(),
        2,
        "recovery fabricated phantom entries from payload bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recover_index_discards_corrupt_header_and_resyncs() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempdir().unwrap();
    let cap = 1024 * 1024;

    let e_data = pattern(6, 0, 3000);
    let f_data = pattern(6, 1, 2000);
    {
        let cache = one_shard_cache(dir.path(), cap);
        assert!(cache.reserve_and_write(bytes::Bytes::from_static(b"ee"), 0, &[], &e_data, None));
        assert!(cache.reserve_and_write(bytes::Bytes::from_static(b"ff"), 0, &[], &f_data, None));
    }

    // Corrupt E's header val_len to an impossible size (torn/garbage
    // header shape) directly in the segment file.
    let seg = dir.path().join("segment_0.bin");
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(&seg).unwrap();
        f.write_all_at(&0xFFFF_FFF0u32.to_le_bytes(), 8).unwrap(); // val_len @ header+8
        f.sync_all().unwrap();
    }

    let cache2 = one_shard_cache(dir.path(), cap);
    cache2.recover_index();

    // Contract 3: the corrupt entry is discarded (not served), the good
    // entry after it is still recovered.
    assert!(
        cache_value(&cache2, "ee").is_none(),
        "a corrupt-header entry must be DISCARDED by recovery, not served"
    );
    let got_f = cache_value(&cache2, "ff")
        .expect("entry ff after a corrupt header must still be recovered (resync)");
    assert_bytes_exact(&value_image(&[], &f_data), &got_f, "recovered entry ff");
}

// ===========================================================================
// Contract 3b: READ-CACHE segments must come up COLD after a remount.
//
// The staging ring's keys are identity-stable across mounts (uuid file_ids;
// inode-keyed active_block overlays) so recovering them is sound — but the
// read cache is keyed by bare block keys whose offsets are freed and REUSED
// across sessions, with no cross-session incarnation store to validate a
// recovered entry against (the fill-time seqlock only guards live fills).
// A resurrected read-cache entry under a reused key serves the PREVIOUS
// incarnation's bytes — the crash-recovery twin of the reused-key
// stale-fill family (fstests generic/616 zeros-read flake). A cache is
// reconstructible: discard it at mount, never recover it.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_read_cache_segments_do_not_resurrect_across_remount() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let meta = NamedTempFile::new().unwrap();
    format_v3_file(meta.path(), 128 * 1024 * 1024).await;
    let staging = tempdir().unwrap();

    let block_key = "12345678";
    let cached = pattern(9, 0, 8192);
    let staged_content = pattern(9, 1, STAGED_LEN);
    let file_id;
    {
        let h = router_h("rc_cold_vol", meta.path(), staging.path()).await;
        h.fs.router
            .cache
            .nvme
            .cache_read_block(block_key, bytes::Bytes::from(cached.clone()))
            .expect("seed read cache");
        assert!(
            h.fs.router
                .cache
                .nvme
                .read_cached_block(block_key)
                .is_some(),
            "read-cache entry must be resident before the remount"
        );
        // And one real staged file: staging MUST keep surviving remounts.
        let ino =
            h.fs.create(h.req, 1, OsStr::new("keepme.bin"), libc::S_IFREG | 0o644, 0)
                .await
                .expect("create")
                .attr
                .ino;
        h.fs.write(
            h.req,
            ino,
            0,
            0,
            bytes::Bytes::copy_from_slice(&staged_content),
            0,
            0,
        )
        .await
        .expect("staged write");
        let layout = close_and_wait_staged(&h, ino).await;
        file_id = layout.file_id.clone().expect("file_id");
        assert!(h.fs.router.cache.nvme.read_staged(&file_id).is_some());
    }

    // Remount over the same staging dir (same generation).
    let h2 = router_h("rc_cold_vol", meta.path(), staging.path()).await;

    // Staging ring: identity-stable, must recover.
    let got = h2
        .fs
        .router
        .cache
        .nvme
        .read_staged(&file_id)
        .expect("staged ring entry must survive the remount (sole copy of dirty data)");
    assert_bytes_exact(&staged_content, &got, "recovered staged entry");

    // Read cache: keys are NOT incarnation-stable across mounts — a
    // resurrected entry under a reused offset key serves the previous
    // incarnation's bytes. Must come up cold.
    assert!(
        h2.fs
            .router
            .cache
            .nvme
            .read_cached_block(block_key)
            .is_none(),
        "read-cache entry RESURRECTED across remount — under offset reuse this serves \
         another incarnation's bytes (the generic/616 zeros-read class)"
    );
}

// ===========================================================================
// Contract 4: router read degrade — staged meta, no ring entry, no mapping
// ⇒ size-consistent zeros, never an error.
// ===========================================================================

struct RouterH {
    fs: SqueezefsFilesystem,
    req: Request,
    routed: Arc<RoutedMetaBackend>,
    _backing: NamedTempFile,
}

async fn router_h(tag: &str, meta_path: &std::path::Path, staging: &std::path::Path) -> RouterH {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    // The staged-layout contracts pin the inline ceiling at one page — the
    // default since the phase-B sweep (.benchmarks/2026-09-09-inline-raise-
    // sweep-local.md) — explicitly, so a SQUEEZEFS_INLINE_MAX_BYTES override
    // in the environment cannot move this suite's sub-block files inline.
    squeezefs::routing::set_inline_max_bytes_override(Some(squeezefs::routing::INLINE_MAX_FLOOR));
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        ba.clone(),
        nvme.clone(),
        Some(GEN),
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);

    let meta_backend = KvMetaBackend::open(meta_path).await.expect("open v3 meta");
    let routed = Arc::new(RoutedMetaBackend::new(vec![meta_backend]));
    router.set_meta_backend(routed.clone());

    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.meta_backend = Some(routed.clone());

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
        ..Default::default()
    };
    RouterH {
        fs,
        req,
        routed,
        _backing: backing,
    }
}

async fn format_v3_file(path: &std::path::Path, len: u64) {
    std::fs::File::create(path).unwrap().set_len(len).unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
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

/// Close the file the way an application does (open → write → close, NO
/// fsync): `release` schedules the layout persist in the background using
/// the write's cached lease; wait for the persisted STAGED layout to land.
/// This is exactly the "acked, not fsync-promoted" crash-point state.
async fn close_and_wait_staged(h: &RouterH, ino: u64) -> LayoutMetadata {
    h.fs.release(h.req, ino, 0, 0, 0, false)
        .await
        .expect("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(Some(bytes)) = h.routed.getxattr(ino, "layout").await {
            if let Ok(layout) = bincode::deserialize::<LayoutMetadata>(&bytes) {
                if layout.file_type == "staged" && layout.file_id.is_some() {
                    return layout;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "staged layout for ino {ino} never persisted after release"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_staged_meta_with_lost_ring_entry_reads_zeros_not_error() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let meta = NamedTempFile::new().unwrap();
    format_v3_file(meta.path(), 128 * 1024 * 1024).await;
    let staging = tempdir().unwrap();
    let h = router_h("degrade_vol", meta.path(), staging.path()).await;

    let ino =
        h.fs.create(
            h.req,
            1,
            OsStr::new("lost_payload.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    let content = pattern(7, 0, STAGED_LEN);
    h.fs.write(
        h.req,
        ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&content),
        0,
        0,
    )
    .await
    .expect("staged write");

    let path = squeezefs::keys::inode_path(ino).to_string();
    let layout = close_and_wait_staged(&h, ino).await;
    assert_eq!(layout.file_type, "staged", "file must persist as staged");
    let file_id = layout
        .file_id
        .clone()
        .expect("staged layout carries file_id");
    assert!(
        h.fs.router.cache.nvme.read_staged(&file_id).is_some(),
        "payload must be resident in the staging ring before the loss is simulated"
    );

    // Simulate the crash outcome: the ring entry is gone (torn → discarded
    // by recovery, or lost before landing). No promoted mapping exists.
    h.fs.router.cache.nvme.remove_staged(&file_id);
    h.fs.router.metadata_cache.invalidate(&ino);
    h.fs.router.cache.read_lru.remove(&path);
    h.fs.router.cache.write_lru.remove(&path);

    // Contract 4: reads degrade to size-consistent zeros — never an error.
    let reply =
        h.fs.read(h.req, ino, 0, 0, STAGED_LEN as u32, 0)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "read of a staged file whose payload was lost by a crash must degrade \
                 to zeros, got an ERROR (the observed post-kill-9 EIO class): {e:?}"
                )
            });
    assert_eq!(
        reply.data.len(),
        STAGED_LEN,
        "degraded read must be size-consistent with the inode"
    );
    assert!(
        reply.data.iter().all(|&b| b == 0),
        "degraded read must be ZEROS (consistent), not stale/garbage bytes"
    );

    // Consistency: a second read (and a sub-range read) see the same zeros.
    let again =
        h.fs.read(h.req, ino, 0, 4096, 1024, 0)
            .await
            .expect("second degraded read must also succeed");
    assert_eq!(again.data.len(), 1024);
    assert!(again.data.iter().all(|&b| b == 0));

    // getattr still reports the inode's size (metadata intact).
    let attr = h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");
    assert_eq!(attr.attr.size, STAGED_LEN as u64);
}

// ===========================================================================
// Contract 5: the deterministic kill-9 repro (re-exec child, crash_kill
// pattern). Child stages KILL_FILES files (no fsync-promotion), persists
// their staged layouts durably, signals READY, and hangs; the parent
// SIGKILLs it, remounts over the SAME staging dir + meta volume, and
// asserts byte-exact reads — before AND after a post-recovery staged
// burst.
// ===========================================================================

fn manifest_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("SQUEEZEFS_STAGED_CRASH_MANIFEST").unwrap())
}

/// Child branch: gated on SQUEEZEFS_STAGED_CRASH_CHILD.
#[test]
fn staged_crash_child_entry() {
    if std::env::var("SQUEEZEFS_STAGED_CRASH_CHILD").is_err() {
        return;
    }
    let meta_vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_STAGED_CRASH_META").unwrap());
    let staging =
        std::path::PathBuf::from(std::env::var("SQUEEZEFS_STAGED_CRASH_STAGING").unwrap());
    let manifest = manifest_path();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let h = router_h("kill9_vol", &meta_vol, &staging).await;

        let mut lines = Vec::new();
        for i in 0..KILL_FILES {
            let name = format!("staged_{i}.bin");
            let ino =
                h.fs.create(h.req, 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                    .await
                    .expect("create")
                    .attr
                    .ino;
            let content = pattern(7, i, STAGED_LEN);
            h.fs.write(
                h.req,
                ino,
                0,
                0,
                bytes::Bytes::copy_from_slice(&content),
                0,
                0,
            )
            .await
            .expect("staged write");
            let layout = close_and_wait_staged(&h, ino).await;
            assert_eq!(
                layout.file_type, "staged",
                "child file {i} must stay staged"
            );
            let file_id = layout.file_id.clone().expect("file_id");
            assert!(
                h.fs.router.cache.nvme.read_staged(&file_id).is_some(),
                "child file {i} payload must be resident in the ring"
            );
            lines.push(format!("FILE {name} {ino} {file_id} {STAGED_LEN}"));
        }
        // Meta durability barrier: the staged layouts are ACKED (create +
        // close/persist); the PAYLOADS are deliberately not fsynced (no
        // promotion) — they live only in the staging ring.
        h.routed.volumes[0].sync_device().await.expect("sync meta");

        let mut f = std::fs::File::create(&manifest).expect("manifest");
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
        writeln!(f, "READY").unwrap();
        f.sync_all().unwrap();

        // Hang until SIGKILL.
        std::future::pending::<()>().await
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staged_ring_survives_kill9_remount_and_post_recovery_writes() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let exe = std::env::current_exe().expect("test binary path");

    let dir = tempdir().unwrap();
    let meta_vol = dir.path().join("staged_crash.v3.meta");
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let manifest = dir.path().join("manifest.txt");

    format_v3_file(&meta_vol, 128 * 1024 * 1024).await;

    let mut child = Command::new(&exe)
        .args([
            "--exact",
            "staged_crash_child_entry",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("SQUEEZEFS_STAGED_CRASH_CHILD", "1")
        .env("SQUEEZEFS_STAGED_CRASH_META", &meta_vol)
        .env("SQUEEZEFS_STAGED_CRASH_STAGING", &staging)
        .env("SQUEEZEFS_STAGED_CRASH_MANIFEST", &manifest)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn staged crash child");

    // Wait for READY (the child's setup is deterministic; the deadline is
    // generous for debug builds).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut manifest_text = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if text.lines().any(|l| l == "READY") {
                manifest_text = text;
                break;
            }
        }
        if let Some(status) = child.try_wait().expect("child wait") {
            panic!("staged crash child exited early ({status:?}) — setup failed");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        manifest_text.lines().any(|l| l == "READY"),
        "child never reached READY"
    );

    // SIGKILL with resident staged data (D0 crash point).
    child.kill().expect("SIGKILL staged crash child");
    let _ = child.wait();

    let files: Vec<(String, u64, String, usize)> = manifest_text
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            match p.as_slice() {
                ["FILE", name, ino, file_id, len] => Some((
                    name.to_string(),
                    ino.parse().unwrap(),
                    file_id.to_string(),
                    len.parse().unwrap(),
                )),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        files.len(),
        KILL_FILES,
        "manifest must list every staged file"
    );

    // Remount over the SAME staging dir + meta volume (same generation —
    // the binding keeps the segments; recover_index rebuilds the ring).
    let h = router_h("kill9_vol", &meta_vol, &staging).await;
    eprintln!(
        "[kill9] remount: ledger_seq={} replay_entries={} dropped_torn={}",
        h.routed.volumes[0].mounted_ledger().seq,
        h.routed.volumes[0].replay_stats().entries,
        h.routed.volumes[0].replay_stats().dropped_torn,
    );

    // Phase 1: every staged file reads back byte-exact after the crash
    // remount. (The ring bytes survive kill-9 via the page cache; an
    // error here is the observed EIO class, a zeros read here means the
    // recovery scan LOST a recoverable entry.)
    for (name, ino, _fid, len) in &files {
        let entry =
            h.fs.lookup(h.req, 1, OsStr::new(name))
                .await
                .unwrap_or_else(|e| panic!("lookup of {name} after kill-9 remount failed: {e:?}"));
        assert_eq!(entry.attr.ino, *ino, "{name}: ino identity across remount");
        let idx: usize = name
            .trim_start_matches("staged_")
            .trim_end_matches(".bin")
            .parse()
            .unwrap();
        let want = pattern(7, idx, *len);
        let reply =
            h.fs.read(h.req, *ino, 0, 0, *len as u32, 0)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "read of {name} after kill-9 remount ERRORED (the observed EIO class; \
                     resident acked staged data must recover, a lost payload must read \
                     zeros — never an error): {e:?}"
                    )
                });
        assert_bytes_exact(&want, &reply.data, &format!("{name} after kill-9 remount"));
    }

    // Phase 2: a post-recovery staged burst must not clobber recovered
    // entries (contract 1 end-to-end — the recovered index must carry
    // TRUE footprints).
    for i in 0..KILL_FILES {
        let name = format!("post_{i}.bin");
        let ino =
            h.fs.create(h.req, 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                .await
                .expect("create post-recovery file")
                .attr
                .ino;
        let content = pattern(8, i, STAGED_LEN);
        h.fs.write(
            h.req,
            ino,
            0,
            0,
            bytes::Bytes::copy_from_slice(&content),
            0,
            0,
        )
        .await
        .expect("post-recovery staged write");
        close_and_wait_staged(&h, ino).await;
    }

    for (name, ino, _fid, len) in &files {
        let idx: usize = name
            .trim_start_matches("staged_")
            .trim_end_matches(".bin")
            .parse()
            .unwrap();
        let want = pattern(7, idx, *len);
        let reply =
            h.fs.read(h.req, *ino, 0, 0, *len as u32, 0)
                .await
                .unwrap_or_else(|e| panic!("re-read of {name} after post-recovery burst: {e:?}"));
        assert_bytes_exact(
            &want,
            &reply.data,
            &format!(
                "{name} after a post-recovery staged burst — a diff means the recovered \
                 index understated footprints and a new entry clobbered this one"
            ),
        );
    }
}
