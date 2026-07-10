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
//! The fix binds staging to the mounted volume set's **filesystem
//! generation** (`meta_backend::volume_set_generation` — v3 superblock
//! uuid / v2 config `fs_uuid`): mount stamps it into a marker at every
//! staging-dir root; a mismatching (or missing/unreadable) marker over
//! NONEMPTY staging discards the content loudly BEFORE any recovery or
//! budget seeding runs.
//!
//! Contracts pinned here:
//! 1. **Reformat over stale staging (the exact repro), v2 AND v3**: mount
//!    over a dead generation's staging comes up with ZERO dead ring state
//!    admitted, the marker replaced, the discard counter bumped, and
//!    bench-shaped writes (colliding with the dead generation's ino
//!    numbers) succeed byte-exact.
//! 2. **Same-generation warm remount keeps staging** (the staging-budget
//!    remount-seeding path behind a MATCHING gate): no discard.
//! 3. **Identity derivation**: v3 identity tracks the superblock uuid
//!    across reformats; v2 identity prefers config `fs_uuid`, falls back
//!    to a config content hash, then the unstamped sentinel; the set
//!    identity is order-dependent.
//! 4. **Upgrade + corruption paths**: unstamped (pre-fix) nonempty
//!    staging is discarded once; a garbage marker over nonempty staging
//!    is discarded; an empty dir is stamped without a discard.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::nvme::STAGING_GENERATION_MARKER;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options, FORMAT_CONFIG_XATTR};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::volume_set_generation;
use squeezefs::meta_backend::{
    storage::MetaLvStorage, xattr, MetaLvBackend, RoutedMetaBackend, VolumeBackend,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::Ordering;
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

fn discards() -> u64 {
    METRICS.staging_generation_discards.load(Ordering::Relaxed)
}

fn marker_content(staging: &Path) -> Option<String> {
    std::fs::read_to_string(staging.join(STAGING_GENERATION_MARKER)).ok()
}

/// The volume-set format config the CLI records on the first volume, with
/// a caller-chosen generation `fs_uuid` (random per real format run).
fn format_config_json(fs_uuid: Option<&str>) -> Vec<u8> {
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        fs_uuid: fs_uuid.map(str::to_string),
        block_size: BS,
        capacity: 256 * 1024 * 1024,
        inodes: 1000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
    };
    serde_json::to_vec(&cfg).unwrap()
}

/// Format (or REformat, `force`) one metadata volume at `path` the way the
/// CLI arm does for that format generation — config xattr (with a fresh
/// generation `fs_uuid`) included, as `squeezefs format` always records it
/// on the first volume.
async fn format_meta(fmt: Fmt, path: &Path, force: bool) {
    let cfg = format_config_json(Some(&uuid::Uuid::new_v4().to_string()));
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
                    format_config_xattr: Some(cfg),
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
            xattr::set_xattr(&ms, 1, FORMAT_CONFIG_XATTR, &cfg)
                .await
                .expect("record v2 format config");
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

/// A bare staging cache over `staging`/`data` bound to `fs_generation`
/// (`None` = a pre-generation-stamping binary, which neither validates nor
/// stamps) — the dead generation's residue factory: entries staged through
/// it survive the drop exactly like a crashed daemon's.
async fn bare_cache(
    staging: &Path,
    data: &Path,
    alloc_name: &str,
    fs_generation: Option<&str>,
) -> TieredCache {
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
        fs_generation,
    )
    .await
    .unwrap()
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    nvme: squeezefs::cache::nvme::NvmeStaging,
}

/// The full mount-shaped stack over an existing staging dir + formatted
/// metadata volume: derives the filesystem generation from the volume set
/// and binds staging to it, exactly as `squeezefs mount` does.
async fn mount_stack(fmt: Fmt, meta: &Path, staging: &Path, data: &Path, alloc_name: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let fs_generation = volume_set_generation(&[meta.to_string_lossy().into_owned()])
        .await
        .expect("volume_set_generation failed");
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
        Some(&fs_generation),
    )
    .await
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
async fn poison_staging(
    staging: &Path,
    data: &Path,
    alloc_name: &str,
    fs_generation: Option<&str>,
) {
    let dead = bare_cache(staging, data, alloc_name, fs_generation).await;
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
/// the dead generation's staging (loud counter + marker replaced) and
/// serve bench-shaped writes cleanly.
async fn reformat_over_stale_staging(fmt: Fmt) {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = format!("staging_gen_reformat_{fmt:?}");

    // Generation 1: format, stamp staging, leave staged residue, "crash".
    format_meta(fmt, meta.path(), false).await;
    let gen1 = volume_set_generation(&[meta.path().to_string_lossy().into_owned()])
        .await
        .expect("gen1 identity");
    poison_staging(staging.path(), data.path(), &alloc, Some(&gen1)).await;
    assert!(
        marker_content(staging.path())
            .expect("generation 1 must have stamped the staging dir")
            .contains(&gen1),
        "marker does not carry generation 1's identity"
    );

    // Generation 2: REFORMAT the metadata volume — staging dir untouched
    // (format was not given --disk-cache-paths).
    format_meta(fmt, meta.path(), true).await;
    let gen2 = volume_set_generation(&[meta.path().to_string_lossy().into_owned()])
        .await
        .expect("gen2 identity");
    assert_ne!(
        gen1, gen2,
        "{fmt:?}: reformat must change the filesystem generation identity"
    );

    // Mount generation 2 over generation 1's staging dir.
    let before = discards();
    let h = mount_stack(fmt, meta.path(), staging.path(), data.path(), &alloc).await;

    // The dead generation's ring state must NOT have been admitted…
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
    // …the discard must be loud (counter), and the marker re-stamped.
    assert_eq!(
        discards(),
        before + 1,
        "stale-staging discard did not bump staging_generation_discards"
    );
    let marker = marker_content(staging.path()).expect("marker must exist after the discard");
    assert!(
        marker.contains(&gen2) && !marker.contains(&gen1),
        "marker was not replaced with generation 2's identity: {marker:?}"
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
/// creditable, NO discard). This is the staging-budget remount-seeding
/// path running behind a MATCHING generation gate.
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
    let gen = volume_set_generation(&[meta.path().to_string_lossy().into_owned()])
        .await
        .expect("generation identity");

    // Session A of THIS generation stages one file, then "crashes".
    let a = bare_cache(staging.path(), data.path(), &alloc, Some(&gen)).await;
    a.nvme
        .stage_write("inode_2", "warm-file-id", &vec![0x11u8; STAGED_LEN], 3)
        .await
        .expect("stage_write failed");
    drop(a);

    // Warm remount of the SAME generation: staging must survive, silently.
    let before = discards();
    let h = mount_stack(fmt, meta.path(), staging.path(), data.path(), &alloc).await;
    assert_eq!(
        discards(),
        before,
        "same-generation warm remount discarded staging"
    );
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
    assert!(
        marker_content(staging.path())
            .expect("marker must survive a warm remount")
            .contains(&gen),
        "warm remount rewrote the marker with a different generation"
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

/// Contract 3 — identity derivation per format and volume-set shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volume_set_generation_identity_contracts() {
    // v3: identity is the superblock uuid — stable across reads, changed
    // by a reformat.
    let v3 = NamedTempFile::new().unwrap();
    format_meta(Fmt::V3, v3.path(), false).await;
    let v3_path = v3.path().to_string_lossy().into_owned();
    let a = volume_set_generation(std::slice::from_ref(&v3_path))
        .await
        .unwrap();
    let a_again = volume_set_generation(std::slice::from_ref(&v3_path))
        .await
        .unwrap();
    assert_eq!(a, a_again, "v3 identity must be stable across reads");
    assert!(
        a.starts_with("v3:"),
        "v3 identity must be superblock-derived: {a}"
    );
    format_meta(Fmt::V3, v3.path(), true).await;
    let b = volume_set_generation(std::slice::from_ref(&v3_path))
        .await
        .unwrap();
    assert_ne!(a, b, "v3 reformat must change the identity (fresh uuid)");

    // v2 with config fs_uuid: identity is the config's generation uuid.
    let v2 = NamedTempFile::new().unwrap();
    let v2_path = v2.path().to_string_lossy().into_owned();
    let ms = MetaLvStorage::open(v2.path(), 128 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&ms, true, false, None)
        .await
        .unwrap();
    xattr::set_xattr(
        &ms,
        1,
        FORMAT_CONFIG_XATTR,
        &format_config_json(Some("gen-uuid-A")),
    )
    .await
    .unwrap();
    let ga = volume_set_generation(std::slice::from_ref(&v2_path))
        .await
        .unwrap();
    assert_eq!(
        ga, "v2:gen-uuid-A",
        "v2 identity must prefer config fs_uuid"
    );

    // v2 without fs_uuid (pre-fix config): content-hash fallback — stable
    // for identical bytes, different for different bytes.
    xattr::set_xattr(&ms, 1, FORMAT_CONFIG_XATTR, &format_config_json(None))
        .await
        .unwrap();
    let h1 = volume_set_generation(std::slice::from_ref(&v2_path))
        .await
        .unwrap();
    assert!(
        h1.starts_with("v2:cfg-"),
        "pre-fix v2 config must fall back to a content hash: {h1}"
    );
    let h1_again = volume_set_generation(std::slice::from_ref(&v2_path))
        .await
        .unwrap();
    assert_eq!(h1, h1_again, "cfg-hash identity must be stable");

    // v2 with no config at all (never CLI-formatted): the sentinel.
    let bare = NamedTempFile::new().unwrap();
    let bare_path = bare.path().to_string_lossy().into_owned();
    let bare_ms = MetaLvStorage::open(bare.path(), 128 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&bare_ms, true, false, None)
        .await
        .unwrap();
    let s = volume_set_generation(std::slice::from_ref(&bare_path))
        .await
        .unwrap();
    assert_eq!(s, "v2:unstamped", "config-less v2 volume sentinel");

    // Volume-set identity is the ORDERED join (route_ino stripes by
    // order: a reordered set is a different metadata view).
    let ab = volume_set_generation(&[v3_path.clone(), bare_path.clone()])
        .await
        .unwrap();
    let ba = volume_set_generation(&[bare_path, v3_path]).await.unwrap();
    assert_eq!(ab, format!("{b}|{s}"), "set identity joins per-volume ids");
    assert_ne!(ab, ba, "volume order is part of the identity");
}

/// Contract 4a — the upgrade path (the user's poisoned `~/.squeeze`):
/// staging populated by a PRE-generation-stamping binary (no marker at
/// all) must be discarded once, then stamped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unstamped_nonempty_staging_is_discarded_once() {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = "staging_gen_unstamped";

    format_meta(Fmt::V3, meta.path(), false).await;
    // Pre-fix binary: stages content, stamps nothing.
    poison_staging(staging.path(), data.path(), alloc, None).await;
    assert!(
        marker_content(staging.path()).is_none(),
        "pre-fix session must not have stamped a marker"
    );

    let before = discards();
    let h = mount_stack(Fmt::V3, meta.path(), staging.path(), data.path(), alloc).await;
    assert_eq!(
        h.nvme.current_staged_write_bytes(),
        0,
        "unstamped staging content was admitted"
    );
    assert_eq!(discards(), before + 1, "unstamped discard must be counted");
    assert!(
        marker_content(staging.path()).is_some(),
        "first generation-aware mount must stamp the dir"
    );
    drop(h);

    // Second mount of the same generation: marker matches — no discard.
    let before2 = discards();
    let _h2 = mount_stack(Fmt::V3, meta.path(), staging.path(), data.path(), alloc).await;
    assert_eq!(
        discards(),
        before2,
        "matching marker must not be discarded again"
    );
}

/// Contract 4b — a garbage (unreadable-as-ours) marker over nonempty
/// staging is a dead generation: discard + re-stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_marker_with_nonempty_staging_is_discarded() {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = "staging_gen_garbage_marker";

    format_meta(Fmt::V3, meta.path(), false).await;
    poison_staging(staging.path(), data.path(), alloc, None).await;
    std::fs::write(
        staging.path().join(STAGING_GENERATION_MARKER),
        b"\x00\xffnot a marker",
    )
    .unwrap();

    let before = discards();
    let h = mount_stack(Fmt::V3, meta.path(), staging.path(), data.path(), alloc).await;
    assert_eq!(
        h.nvme.current_staged_write_bytes(),
        0,
        "staging behind a garbage marker was admitted"
    );
    assert_eq!(discards(), before + 1, "garbage-marker discard not counted");
    let gen = volume_set_generation(&[meta.path().to_string_lossy().into_owned()])
        .await
        .unwrap();
    assert!(
        marker_content(staging.path())
            .expect("marker must be re-stamped")
            .contains(&gen),
        "garbage marker was not replaced with the mounted generation"
    );
}

/// Contract 4c — an empty staging dir is simply stamped: no discard, no
/// counter noise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_staging_dir_is_stamped_without_discard() {
    let meta = NamedTempFile::new().unwrap();
    let data = NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let staging: TempDir = tempdir().unwrap();
    let alloc = "staging_gen_empty_stamp";

    format_meta(Fmt::V2, meta.path(), false).await;
    let gen = volume_set_generation(&[meta.path().to_string_lossy().into_owned()])
        .await
        .unwrap();

    let before = discards();
    let _h = mount_stack(Fmt::V2, meta.path(), staging.path(), data.path(), alloc).await;
    assert_eq!(discards(), before, "empty dir must not count as a discard");
    assert!(
        marker_content(staging.path())
            .expect("empty dir must be stamped")
            .contains(&gen),
        "empty-dir stamp carries the wrong generation"
    );
}
