//! Staging generation-binding contracts: STALE STAGING FROM A DEAD
//! FILESYSTEM GENERATION MUST NEVER POISON A FRESH MOUNT.
//!
//! The field bug (2026-07-10, user-hit): `squeezefs format` wipes
//! `--disk-cache-paths` staging dirs when given them — but when format is
//! invoked WITHOUT `--disk-cache-paths` (staging only passed at mount
//! time), the staging dir of the PREVIOUS filesystem generation survives
//! the reformat. The next mount then recovers/admits staging segments +
//! `active_block:`/mapping ring state belonging to a DEAD generation:
//! the admission budget starts pre-charged with dead bytes, dead
//! `active_block:inode_N:block_K` entries collide with the new
//! generation's freshly allocated (identical) ino numbers, and dead
//! offset-keyed read-cache blocks alias recycled device offsets.
//! Observed failure modes on dev@b571950: every bench write EIO'd through
//! the (correct) binding-rebind refusal ("did not settle after 8 binding
//! rebinds"), or the daemon exited early during mount.
//!
//! Contracts pinned here:
//! 1. **Reformat over stale staging (the exact repro), v2 AND v3**:
//!    format → stage writes (staged file + active blocks) → "crash" →
//!    REFORMAT the metadata volume set (no staging wipe) → mount over the
//!    old staging dir. The mount must come up with ZERO dead-generation
//!    ring state admitted (budget 0, staging index empty), the stale
//!    content must be discarded, and bench-shaped writes (staged +
//!    striped-with-active-tail, colliding with the dead generation's ino
//!    numbers) must succeed and read back byte-exact.
//! 2. **Same-generation warm remount keeps staging**: the legitimate
//!    warm-restart path (staging-budget contract 5) must NOT be wiped —
//!    recovered staged entries stay budgeted and readable.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{
    storage::MetaLvStorage, MetaLvBackend, RoutedMetaBackend, VolumeBackend,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB blocks: inline <= 4 KiB, staged 4 KiB..64 KiB, striped beyond.
const BS: u64 = 65536;
/// Staged payload size: within the staged window.
const STAGED_LEN: usize = 60 * 1024;
/// Per-entry metadata overhead allowance (staged header + serialized meta).
const SLACK: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    V2,
    V3,
}

/// Format (or REformat, `force`) one metadata volume at `path` exactly the
/// way the CLI arm does it for that format generation.
async fn format_meta(fmt: Fmt, path: &Path, force: bool) {
    match fmt {
        Fmt::V3 => {
            format_v3(
                path,
                128 * 1024 * 1024,
                &FormatV3Options {
                    node_size: DEFAULT_NODE_SIZE,
                    journal_len_override: None,
                    force,
                    full_wipe: false,
                    format_config_xattr: None,
                },
            )
            .await
            .expect("format v3 failed");
        }
        Fmt::V2 => {
            let ms = MetaLvStorage::open(path, 128 * 1024 * 1024).expect("open v2 storage");
            MetaLvBackend::format_v2_for_tests(&ms, true, force, None)
                .await
                .expect("format v2 failed");
        }
    }
}

/// Open the (already formatted) volume as the mount's metadata backend.
async fn open_meta(fmt: Fmt, path: &Path) -> Arc<RoutedMetaBackend> {
    match fmt {
        Fmt::V2 => {
            let ms = MetaLvStorage::open(path, 128 * 1024 * 1024).expect("open v2 storage");
            ms.validate_superblock().await.expect("v2 superblock");
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(
                Arc::new(MetaLvBackend::new(ms)),
            )]))
        }
        Fmt::V3 => {
            let be = KvMetaBackend::open(path).await.expect("open v3 backend");
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(be)]))
        }
    }
}

/// A bare staging cache over `staging`/`data` — the dead generation's
/// residue factory (the staging-budget contract-5 shape: entries staged
/// through it survive the drop exactly like a crashed daemon's).
async fn bare_cache(staging: &Path, data: &Path, alloc_name: &str) -> TieredCache {
    let dlm = DlmClient::new("local").unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_name)
            .await
            .unwrap(),
    );
    TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("8MB"),
        dlm.meta_client().clone(),
        ba,
        nvme_dev,
    )
    .unwrap()
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    nvme: squeezefs::cache::nvme::NvmeStaging,
}

/// The full mount-shaped stack over an existing staging dir + formatted
/// metadata volume (what `squeezefs mount` builds after the bootstrap).
async fn mount_stack(fmt: Fmt, meta: &Path, staging: &Path, data: &Path, alloc_name: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_name)
            .await
            .unwrap(),
    );
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("8MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme_dev.clone(),
    )
    .unwrap();
    let nvme = cache.nvme.clone();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let routed = open_meta(fmt, meta).await;
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H { fs, req, nvme }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap_or_else(|e| panic!("create {name} failed: {e:?}"))
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
        .unwrap_or_else(|e| {
            panic!("write ino {ino} off {off} failed (dead-generation staging poison?): {e:?}")
        });
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size)
        .await
        .unwrap_or_else(|e| {
            panic!("read ino {ino} failed (dead-generation staging poison?): {e:?}")
        })
        .data
        .to_vec()
}

/// Leave a dead filesystem generation's residue in `staging`: one staged
/// file (`inode_2`) plus active blocks blanketing the ino numbers a fresh
/// volume will hand out first — exactly the ring state a crashed/dismounted
/// daemon of the OLD generation leaves for `recover_index` to resurrect.
async fn poison_staging(staging: &Path, data: &Path, alloc_name: &str) {
    let dead = bare_cache(staging, data, alloc_name).await;
    dead.nvme
        .stage_write(
            "inode_2",
            "dead-generation-file-id",
            &vec![0xAAu8; STAGED_LEN],
            7,
        )
        .await
        .expect("dead-generation stage_write failed");
    for ino in 2u64..6 {
        assert!(
            dead.nvme.put_active_block(
                &format!("active_block:inode_{ino}:block_0"),
                &vec![0xAAu8; BS as usize],
                7,
            ),
            "dead-generation active block put refused with an empty pool"
        );
    }
    drop(dead);
}

/// Contract 1 — the exact field repro: format → write → crash → REFORMAT
/// (no staging wipe) → mount over the old staging. The mount must discard
/// the dead generation's staging and serve bench-shaped writes cleanly.
async fn reformat_over_stale_staging(fmt: Fmt) {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = format!("staging_gen_reformat_{fmt:?}");

    // Generation 1: format, leave staged residue, "crash".
    format_meta(fmt, meta.path(), false).await;
    poison_staging(staging.path(), data.path(), &alloc).await;

    // Generation 2: REFORMAT the metadata volume — staging dir untouched
    // (format was not given --disk-cache-paths).
    format_meta(fmt, meta.path(), true).await;

    // Mount generation 2 over generation 1's staging dir.
    let h = mount_stack(fmt, meta.path(), staging.path(), data.path(), &alloc).await;

    // The dead generation's ring state must NOT have been admitted.
    assert_eq!(
        h.nvme.current_staged_write_bytes(),
        0,
        "dead-generation staged bytes were seeded into the admission budget"
    );
    assert!(
        h.nvme.staging_nvme_cache.list_keys().is_empty(),
        "dead-generation ring entries were recovered into the staging index: {:?}",
        h.nvme
            .staging_nvme_cache
            .list_keys()
            .iter()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect::<Vec<_>>()
    );

    // Bench-shaped writes on the fresh generation: the first files created
    // reuse the dead generation's ino numbers (2, 3, ...) — the collision
    // that produced the rebind-EIO storm. They must succeed and read back
    // byte-exact.
    let small = create(&h, "bench_small.bin").await;
    let small_payload = vec![0xBBu8; STAGED_LEN];
    write_at(&h, small, 0, &small_payload).await;
    assert_eq!(
        read_at(&h, small, 0, STAGED_LEN as u32).await,
        small_payload,
        "staged read on the fresh generation returned dead-generation bytes"
    );

    let big = create(&h, "bench_big.bin").await;
    write_at(&h, big, 0, &vec![0xCCu8; BS as usize + 1]).await;
    // Overwrite routes through the staged active-block path — the same
    // `active_block:inode_N:block_0` key the dead generation left behind.
    write_at(&h, big, 0, &vec![0xCDu8; BS as usize + 1]).await;
    assert_eq!(
        read_at(&h, big, 0, BS as u32 + 1).await,
        vec![0xCDu8; BS as usize + 1],
        "striped read served dead-generation active-block bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reformat_over_stale_staging_discards_dead_generation_v3() {
    reformat_over_stale_staging(Fmt::V3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reformat_over_stale_staging_discards_dead_generation_v2() {
    reformat_over_stale_staging(Fmt::V2).await;
}

/// Contract 2 — the keep direction: a warm remount of the SAME filesystem
/// generation must keep recovered staging (budget seeded, entries
/// creditable). This is the staging-budget remount-seeding path; the
/// generation gate must sit in front of it without regressing it.
async fn same_generation_warm_remount_keeps_staging(fmt: Fmt) {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = format!("staging_gen_warm_{fmt:?}");

    format_meta(fmt, meta.path(), false).await;

    // Session A of THIS generation stages one file, then "crashes".
    let a = bare_cache(staging.path(), data.path(), &alloc).await;
    a.nvme
        .stage_write("inode_2", "warm-file-id", &vec![0x11u8; STAGED_LEN], 3)
        .await
        .expect("stage_write failed");
    drop(a);

    // Warm remount of the SAME generation: staging must survive.
    let h = mount_stack(fmt, meta.path(), staging.path(), data.path(), &alloc).await;
    let seeded = h.nvme.current_staged_write_bytes();
    assert!(
        seeded >= STAGED_LEN as u64,
        "same-generation warm remount lost the recovered staged entry: budget {seeded}"
    );
    assert!(
        seeded <= STAGED_LEN as u64 + SLACK,
        "warm remount seeded more than the live staged entry: {seeded}"
    );
    assert!(
        h.nvme.read_staged("warm-file-id").is_some(),
        "same-generation warm remount discarded a recovered staged entry"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_generation_warm_remount_keeps_staging_v3() {
    same_generation_warm_remount_keeps_staging(Fmt::V3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_generation_warm_remount_keeps_staging_v2() {
    same_generation_warm_remount_keeps_staging(Fmt::V2).await;
}
