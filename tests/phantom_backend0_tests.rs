//! Phantom `backend_0` regression suite.
//!
//! Live-diagnosed pre-beta bug: with multiple `sqdata://` volumes the daemon's
//! data-volume table contained the real named volumes PLUS a hardcoded legacy
//! entry literally named `backend_0` (`.config` showed
//! `data_volumes: [backend_0, oss1, oss2, oss3, oss4]`). Write placement could
//! route a striped flush to the phantom, and — worse — every hot write path
//! persisted UNPREFIXED block keys (`offset.to_string()`) regardless of which
//! named backend the bytes were DMA'd to, so reads/frees of blocks placed on
//! any non-first volume resolved through the `backend_0` alias to the WRONG
//! device (EIO/corruption to the app, leaked refcounts → degraded health).
//!
//! Contract pinned here:
//! 1. When named volumes are registered, `backend_0` is NOT a placement
//!    candidate, NOT a `.config` data-volume entry, and NOT health-scored.
//! 2. `backend_0` REMAINS a pure key-resolution alias: legacy unprefixed and
//!    `backend_0://`-prefixed block keys resolve to the default (first) volume
//!    on both single- and multi-volume mounts.
//! 3. A bench-shaped striped write burst on a multi-volume mount reads back
//!    byte-identical and frees back to zero used blocks on EVERY volume.
//! 4. Single-volume mounts keep today's on-disk behavior: persisted block-map
//!    keys stay bare/unprefixed and keep resolving.
//! 5. A phantom `backend_0: disabled` health override must not poison the
//!    alias health used by legacy-key reads — since PR VL3 (the `/dev/shm`
//!    runtime-config file is deleted) the override surface is
//!    `BackendRouter::set_health_override`, which refuses the reserved
//!    alias and unknown ids outright.

use fuse3::raw::Filesystem;
use fuse3::SetAttr;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, CONFIG_INODE, STATS_INODE};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{DataRouter, StorageBackend};
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
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
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const BLOCK: usize = 4096;

struct NamedVolume {
    name: String,
    _backing: NamedTempFile,
    device: Arc<NvmeBlockDev>,
    allocator: Arc<BlockAllocator>,
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    volumes: Vec<NamedVolume>,
    _staging: TempDir,
    _meta: NamedTempFile,
}

/// Build a filesystem exactly the way `main.rs` `Commands::Mount` does for a
/// set of named data volumes: the FIRST volume's device+allocator become the
/// router's default slot (the `backend_0` alias target), and EVERY volume —
/// including the first — is registered in `backends` under its real name.
async fn harness(volume_names: &[&str]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    let dlm = DlmClient::new("local").unwrap();

    let mut volumes = Vec::new();
    for name in volume_names {
        let backing = NamedTempFile::new().unwrap();
        // The allocator strides at its 4 MiB chunk size, so a burst of N
        // blocks needs N * 4 MiB of (sparse) device.
        std::fs::File::create(backing.path())
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let device = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
        let allocator = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), name)
                .await
                .unwrap(),
        );
        volumes.push(NamedVolume {
            name: name.to_string(),
            _backing: backing,
            device,
            allocator,
        });
    }

    let first_dev = volumes[0].device.clone();
    let first_alloc = volumes[0].allocator.clone();

    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, first_alloc.clone(), first_dev.clone());

    for vol in &volumes {
        router.backend_router.backends.insert(
            vol.name.clone(),
            Arc::new(StorageBackend {
                device: vol.device.clone(),
                block_allocator: vol.allocator.clone(),
            }),
        );
    }

    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_backend = open_v3_meta(meta_temp.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = fuse3::raw::Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    H {
        fs,
        req,
        volumes,
        _staging: staging,
        _meta: meta_temp,
    }
}

fn pattern_block(b: usize) -> Vec<u8> {
    vec![(b as u8) ^ 0x5A; BLOCK]
}

async fn read_config_json(h: &H) -> serde_json::Value {
    let attr = h.fs.getattr(h.req, CONFIG_INODE, None, 0).await.unwrap();
    let len = attr.attr.size;
    let reply =
        h.fs.read(h.req, CONFIG_INODE, 0, 0, len as u32, 0)
            .await
            .unwrap();
    serde_json::from_slice(&reply.data).expect(".config must be valid JSON")
}

/// Drop every RAM/NVMe read-tier entry so subsequent reads must resolve each
/// block-map key through the backend router down to a real device read —
/// the tier a wrong-device key actually corrupts.
fn purge_read_tiers(h: &H) {
    for key in h.fs.router.cache.read_lru.keys() {
        h.fs.router.cache.read_lru.remove(&key);
    }
    for key in h.fs.router.cache.nvme.list_cached_blocks() {
        h.fs.router.cache.nvme.remove_cached_read_block(&key);
    }
}

/// Write a bench-shaped striped burst of `nblocks` distinct 4 KiB blocks and
/// fsync so every block flushes through write placement. Returns the expected
/// file content.
async fn striped_burst(h: &H, ino: u64, nblocks: usize) -> Vec<u8> {
    // Force the striped layout transition first (same trick as the writeback
    // suite): a >4096-byte initial write.
    let dummy = vec![0u8; BLOCK + 1];
    h.fs.write(
        h.req,
        ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&dummy),
        0,
        0,
    )
    .await
    .unwrap();

    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = pattern_block(b);
        expected.extend_from_slice(&data);
        h.fs.write(
            h.req,
            ino,
            0,
            (b * BLOCK) as u64,
            bytes::Bytes::copy_from_slice(&data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write of block {b} failed: {e:?}"));
    }
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .unwrap_or_else(|e| panic!("fsync after burst failed: {e:?}"));
    expected
}

async fn create_file(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

// ---------------------------------------------------------------------------
// 1. Placement: the phantom must never be selected when named volumes exist.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multi_volume_placement_never_selects_phantom_backend_0() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2", "oss3", "oss4"]).await;

    let mut seen = std::collections::HashSet::new();
    for i in 0..64 {
        let (be_id, _, _) =
            h.fs.router
                .backend_router
                .get_active_backend()
                .unwrap_or_else(|e| panic!("get_active_backend #{i} failed: {e:?}"));
        assert_ne!(
            be_id, "backend_0",
            "write placement selected the phantom backend_0 with 4 real named volumes registered"
        );
        assert!(
            h.volumes.iter().any(|v| v.name == be_id),
            "placement returned unknown backend id {be_id:?}"
        );
        seen.insert(be_id);
    }
    // Round-robin over equal-health volumes must spread across all of them.
    assert_eq!(
        seen.len(),
        4,
        "placement did not distribute across all named volumes: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_volume_placement_never_selects_phantom_backend_0() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["solo1"]).await;

    for i in 0..16 {
        let (be_id, _, _) =
            h.fs.router
                .backend_router
                .get_active_backend()
                .unwrap_or_else(|e| panic!("get_active_backend #{i} failed: {e:?}"));
        assert_eq!(
            be_id, "solo1",
            "single-volume placement must name the real registered volume, never the phantom"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. `.config` data-volume table: exactly the named volumes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multi_volume_config_table_lists_exactly_named_volumes() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2", "oss3", "oss4"]).await;

    let cfg = read_config_json(&h).await;
    let table = cfg["data_volumes"]
        .as_object()
        .expect(".config data_volumes must be an object");
    let mut keys: Vec<_> = table.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["oss1", "oss2", "oss3", "oss4"],
        "the .config data-volume table must contain EXACTLY the named volumes \
         (phantom backend_0 entry observed)"
    );
}

// ---------------------------------------------------------------------------
// 3. Bench-shaped striped burst: byte-identical read-back, clean frees.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multi_volume_striped_burst_reads_back_and_frees_cleanly() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2", "oss3", "oss4"]).await;
    let nblocks = 24usize;

    let ino = create_file(&h, "bench_burst.bin").await;
    let expected = striped_burst(&h, ino, nblocks).await;

    // Placement sanity: the burst must actually have spread beyond the first
    // volume, otherwise the read-back assertion proves nothing.
    let spread = h
        .volumes
        .iter()
        .filter(|v| v.allocator.get_used_blocks() > 0)
        .count();
    assert!(
        spread >= 2,
        "burst did not spread across named volumes (used-block spread = {spread})"
    );

    // Reads must come from the devices, not the write-time RAM tiers.
    purge_read_tiers(&h);

    for b in 0..nblocks {
        let reply =
            h.fs.read(h.req, ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
                .await
                .unwrap_or_else(|e| {
                    panic!("read of block {b} errored (EIO/ENOENT surfaced to the app): {e:?}")
                });
        assert_eq!(
            &reply.data[..],
            &expected[b * BLOCK..(b + 1) * BLOCK],
            "block {b} read back wrong bytes — its key resolved to the wrong device"
        );
    }

    // Truncate to zero: every displaced key must free on the allocator that
    // owns it, on every volume — the unit-level pin of "health stays 1000".
    h.fs.setattr(
        h.req,
        ino,
        None,
        SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Async block-reclaim: truncate's displaced frees finish on the
    // background queue.
    h.fs.router.backend_router.reclaim_drain().await;

    for vol in &h.volumes {
        assert_eq!(
            vol.allocator.get_used_blocks(),
            0,
            "volume {} leaked blocks after truncate — frees routed to the wrong allocator",
            vol.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Legacy key-resolution alias compat (must stay green before AND after).
// ---------------------------------------------------------------------------

async fn assert_legacy_keys_resolve(h: &H) {
    let alloc = &h.volumes[0].allocator;
    let dev = &h.volumes[0].device;

    let offset = alloc.allocate_block().await.unwrap();
    let payload = vec![0xC3u8; BLOCK];
    dev.write_block(offset, bytes::Bytes::copy_from_slice(&payload))
        .await
        .unwrap();
    alloc.publish_block(offset);

    let bare = offset.to_string();
    let prefixed = format!("backend_0://{offset}");

    let via_bare =
        h.fs.router
            .backend_router
            .read_block(&bare, BLOCK)
            .await
            .expect("legacy UNPREFIXED block key must resolve to the first/primary volume");
    assert_eq!(&via_bare[..], &payload[..]);

    let via_prefix =
        h.fs.router
            .backend_router
            .read_block(&prefixed, BLOCK)
            .await
            .expect("legacy backend_0:// block key must resolve to the first/primary volume");
    assert_eq!(&via_prefix[..], &payload[..]);

    // The alias must also route frees/refcounts to the first volume.
    assert!(
        h.fs.router.backend_router.increment_refcount(&prefixed),
        "refcount take through the backend_0 alias failed"
    );
    h.fs.router
        .backend_router
        .free_block(&prefixed)
        .await
        .unwrap();
    h.fs.router.backend_router.free_block(&bare).await.unwrap();
    // Async block-reclaim: frees finish on the background queue.
    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(
        alloc.get_used_blocks(),
        0,
        "alias frees must land on volume 1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_legacy_keys_resolve_on_multi_volume_mount() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2", "oss3", "oss4"]).await;
    assert_legacy_keys_resolve(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_legacy_keys_resolve_on_single_volume_mount() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["solo1"]).await;
    assert_legacy_keys_resolve(&h).await;
}

// ---------------------------------------------------------------------------
// 5. Single-volume on-disk behavior byte-identical: keys stay unprefixed.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_volume_persisted_keys_stay_unprefixed_and_read_back() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["solo1"]).await;
    let nblocks = 8usize;

    let ino = create_file(&h, "solo_burst.bin").await;
    let expected = striped_burst(&h, ino, nblocks).await;

    let meta =
        h.fs.router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap();
    assert_eq!(meta.file_type, "striped");
    let map = meta
        .block_map
        .as_ref()
        .expect("striped file has a block map");
    assert_eq!(map.len(), nblocks);
    for (idx, key) in map.iter() {
        assert!(
            !key.contains("://"),
            "single-volume mounts must keep persisting UNPREFIXED block keys \
             (on-disk compat) — block {idx} persisted {key:?}"
        );
    }

    purge_read_tiers(&h);
    for b in 0..nblocks {
        let reply =
            h.fs.read(h.req, ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
                .await
                .unwrap();
        assert_eq!(
            &reply.data[..],
            &expected[b * BLOCK..(b + 1) * BLOCK],
            "single-volume block {b} read back wrong bytes"
        );
    }

    h.fs.setattr(
        h.req,
        ino,
        None,
        SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Async block-reclaim: truncate's frees finish on the background queue.
    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(h.volumes[0].allocator.get_used_blocks(), 0);
}

// ---------------------------------------------------------------------------
// 6. Phantom health overrides must not poison alias health. (The /dev/shm
//    runtime-config file this contract originally guarded against was
//    DELETED in PR VL3; the override surface is now
//    `BackendRouter::set_health_override`, which must REFUSE the reserved
//    alias outright — the same protection, expressed structurally.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_phantom_backend0_health_override_refused_and_alias_unpoisoned() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let h = harness(&["oss1", "oss2", "oss3", "oss4"]).await;

    // A `backend_0: disabled` override (the stale-runtime-config shape of
    // old) must REFUSE — the reserved alias is not a volume.
    assert!(
        h.fs.router
            .backend_router
            .set_health_override("backend_0", true)
            .is_err(),
        "the reserved backend_0 alias must refuse health overrides"
    );
    // Unknown ids refuse too (a stale name from an older generation can
    // never linger as a poison entry — there is no file to linger in).
    assert!(
        h.fs.router
            .backend_router
            .set_health_override("oss_gone", true)
            .is_err(),
        "unknown volume ids must refuse health overrides"
    );

    let cfg_json = read_config_json(&h).await;
    let table = cfg_json["data_volumes"].as_object().unwrap();
    assert!(
        !table.contains_key("backend_0"),
        "phantom backend_0 must not appear in the data-volume table"
    );
    for name in ["oss1", "oss2", "oss3", "oss4"] {
        assert_eq!(
            table[name]["status"], "enabled",
            "real volume {name} must stay enabled"
        );
    }

    // Legacy alias reads must still work: nothing above may have marked
    // the default slot unhealthy.
    assert_legacy_keys_resolve(&h).await;
}

// ---------------------------------------------------------------------------
// 7. `.config` virtual-file honesty (user report 2026-07-22): catting the
//    file must yield EXACT-size JSON (no tail-padding whitespace spam), the
//    data-volume table must carry each volume's REAL backing device path and
//    durable id (never the name recycled into `backing_dev`), and the
//    canonical `sqmeta://` / `sqdata://` URI spellings — the same strings
//    the CLI verbs accept — must appear for the set and per volume.
// ---------------------------------------------------------------------------

/// One full single-reader "cat" cycle against a virtual inode: LOOKUP →
/// OPEN (pins the generation) → fstat(GETATTR) → read-to-EOF → RELEASE.
/// Returns (the fstat-reported size, the bytes the pinned fh served).
async fn cat_virtual(h: &H, ino: u64) -> (u64, Vec<u8>) {
    let name = if ino == CONFIG_INODE {
        ".config"
    } else {
        ".stats"
    };
    let _entry =
        h.fs.lookup(h.req, 1, OsStr::new(name))
            .await
            .expect("lookup virtual inode");
    let opened =
        h.fs.open(h.req, ino, libc::O_RDONLY as u32, 0)
            .await
            .expect("open virtual inode");
    let attr =
        h.fs.getattr(h.req, ino, Some(opened.fh), 0)
            .await
            .expect("getattr virtual inode");
    let data =
        h.fs.read(h.req, ino, opened.fh, 0, 16 * 1024 * 1024, 0)
            .await
            .expect("read virtual inode");
    h.fs.release(h.req, ino, opened.fh, 0, 0, false)
        .await
        .expect("release virtual inode");
    (attr.attr.size, data.data.to_vec())
}

/// The catted `.config` bytes are exactly the JSON payload plus ONE final
/// newline — no tail padding (the user's "lot of line returns") — and the
/// fstat size the kernel copies to equals the served bytes exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_config_payload_exact_size_no_tail_padding() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2"]).await;

    let (size, bytes) = cat_virtual(&h, CONFIG_INODE).await;
    assert_eq!(
        size,
        bytes.len() as u64,
        "fstat size must equal the bytes the pinned fh serves"
    );
    let raw = String::from_utf8(bytes).expect(".config is UTF-8");
    assert_eq!(
        raw,
        format!("{}\n", raw.trim_end()),
        ".config must carry NO tail padding beyond one final newline"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect(".config parses as JSON");
    assert!(parsed.get("data_volumes").is_some());
}

/// `.stats` rides the same exact-size snapshot mechanism: regression guard
/// for the padding removal (monitoring scripts read this inode).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stats_payload_exact_size_no_tail_padding() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2"]).await;

    let (size, bytes) = cat_virtual(&h, STATS_INODE).await;
    assert_eq!(
        size,
        bytes.len() as u64,
        "fstat size must equal the bytes the pinned fh serves"
    );
    let raw = String::from_utf8(bytes).expect(".stats is UTF-8");
    assert_eq!(
        raw,
        format!("{}\n", raw.trim_end()),
        ".stats must carry NO tail padding beyond one final newline"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect(".stats parses as JSON");
    assert!(
        parsed.get("metrics").is_some(),
        ".stats still serves the complete metrics surface"
    );
}

/// The data-volume table carries each volume's REAL backing device path
/// (the registered device, like the metadata table always did), its durable
/// id, and per-volume + volume-set URIs in the product's canonical grammar
/// (`parse_block_uri`: `sqmeta://p1,p2` / `sqdata://p1,p2`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_config_lists_real_backing_devices_ids_and_uris() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["oss1", "oss2"]).await;

    // Publish the durable record snapshot exactly as mount wiring does
    // (KD-5: ids are the registered names; backing_dev the real path).
    let records: Vec<squeezefs::DataVolumeRecord> = h
        .volumes
        .iter()
        .map(|v| squeezefs::DataVolumeRecord {
            id: v.name.clone(),
            backing_dev: v.device.device_path.clone(),
            state: squeezefs::VOL_STATE_ACTIVE.to_string(),
            added_ts: 0,
        })
        .collect();
    h.fs.router.backend_router.set_volume_records(records);

    let (_size, bytes) = cat_virtual(&h, CONFIG_INODE).await;
    let cfg: serde_json::Value = serde_json::from_slice(&bytes).expect(".config parses as JSON");

    let table = cfg["data_volumes"]
        .as_object()
        .expect("data_volumes object");
    let mut data_paths = Vec::new();
    for v in &h.volumes {
        let entry = table
            .get(&v.name)
            .unwrap_or_else(|| panic!("volume {} missing from .config", v.name));
        let real = &v.device.device_path;
        assert_eq!(
            entry["backing_dev"].as_str(),
            Some(real.as_str()),
            "data volume {} must report its REAL backing device path, \
             not its name recycled into the field",
            v.name
        );
        assert_eq!(
            entry["id"].as_str(),
            Some(v.name.as_str()),
            "data volume {} must carry its durable volume id",
            v.name
        );
        assert_eq!(
            entry["uri"].as_str(),
            Some(format!("sqdata://{real}").as_str()),
            "data volume {} must carry its canonical sqdata:// URI",
            v.name
        );
        assert_eq!(entry["status"], "enabled");
        assert!(entry["health"].is_number(), "health stays a numeric score");
        data_paths.push(real.clone());
    }
    assert_eq!(
        cfg["sqdata_uri"].as_str(),
        Some(format!("sqdata://{}", data_paths.join(",")).as_str()),
        "the joined data-volume-set sqdata:// URI must appear"
    );

    // Metadata table: real path (as today) + per-volume URI + the joined
    // volume-set sqmeta:// URI — the exact string the CLI verbs accept.
    let meta_path = h._meta.path().display().to_string();
    let mtable = cfg["metadata_volumes"]
        .as_object()
        .expect("metadata_volumes object");
    let m0 = mtable
        .get("meta_volume_0")
        .expect("meta_volume_0 entry present");
    assert_eq!(m0["backing_dev"].as_str(), Some(meta_path.as_str()));
    assert_eq!(
        m0["uri"].as_str(),
        Some(format!("sqmeta://{meta_path}").as_str()),
        "per-meta-volume sqmeta:// URI"
    );
    assert_eq!(
        cfg["sqmeta_uri"].as_str(),
        Some(format!("sqmeta://{meta_path}").as_str()),
        "the volume-set sqmeta:// URI (joined member form the CLI accepts) must appear"
    );
}

/// Legacy bare-router fallback (offline tools, tests): a router with no
/// named registrations keeps its `backend_0` default-slot entry — but even
/// that entry must report the default device's real path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_config_bare_router_default_slot_reports_device_path() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = harness(&["solo1"]).await;
    h.fs.router.backend_router.backends.clear();

    let (_size, bytes) = cat_virtual(&h, CONFIG_INODE).await;
    let cfg: serde_json::Value = serde_json::from_slice(&bytes).expect(".config parses as JSON");
    let table = cfg["data_volumes"]
        .as_object()
        .expect("data_volumes object");
    let entry = table
        .get("backend_0")
        .expect("bare routers keep the legacy default-slot entry");
    assert_eq!(
        entry["backing_dev"].as_str(),
        Some(h.volumes[0].device.device_path.as_str()),
        "even the legacy default slot must report the real device path"
    );
}

// ---------------------------------------------------------------------------
// 8. Real-CLI multi-volume shape (the user's exact failure): format 4 sqmeta +
//    4 sqdata, mount, bench-shaped burst, .config exactly oss1-4, no
//    ENOENT/EIO in the daemon log.
// ---------------------------------------------------------------------------

mod cli {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn transport_supported() -> bool {
        if !std::path::Path::new("/dev/fuse").exists() {
            eprintln!("[SKIP] /dev/fuse not present");
            return false;
        }
        match std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring") {
            Ok(v)
                if matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "y" | "1" | "yes" | "true" | "on"
                ) => {}
            other => {
                eprintln!("[SKIP] kernel fuse.enable_uring not enabled ({other:?})");
                return false;
            }
        }
        if Command::new("fusermount3").arg("-V").output().is_err() {
            eprintln!("[SKIP] fusermount3 not available");
            return false;
        }
        true
    }

    struct Mount {
        child: Child,
        mnt: PathBuf,
        base: PathBuf,
        log: PathBuf,
    }

    impl Mount {
        fn unmount(&mut self) {
            let _ = Command::new("timeout")
                .arg("30")
                .arg("sync")
                .arg("-f")
                .arg(&self.mnt)
                .status();
            for _ in 0..10 {
                let st = Command::new("fusermount3")
                    .arg("-u")
                    .arg(&self.mnt)
                    .status()
                    .expect("run fusermount3 -u");
                if st.success() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if self.child.try_wait().expect("try_wait").is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = self.child.kill();
            panic!("mount daemon did not exit within 30s of unmount");
        }
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn scratch_base(tag: &str) -> PathBuf {
        let home = std::env::var("HOME").expect("HOME set");
        let base = PathBuf::from(home)
            .join("tmp")
            .join(format!("sqfs_phantom_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn mount_volumes(tag: &str, data_names: &[&str]) -> Mount {
        let bin = env!("CARGO_BIN_EXE_squeezefs");
        let base = scratch_base(tag);
        let staging = base.join("staging");
        let mnt = base.join("mnt");
        let log = base.join("mount.log");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();

        let mut meta_uris = Vec::new();
        for i in 0..data_names.len() {
            let meta = base.join(format!("meta{}", i + 1));
            std::fs::File::create(&meta)
                .unwrap()
                .set_len(256 * 1024 * 1024)
                .unwrap();
            meta_uris.push(meta.display().to_string());
        }
        let mut data_uris = Vec::new();
        for name in data_names {
            let data = base.join(name);
            std::fs::File::create(&data)
                .unwrap()
                .set_len(4 * 1024 * 1024 * 1024)
                .unwrap();
            data_uris.push(data.display().to_string());
        }

        // Cache-path policy: staging dirs are DECLARED AT FORMAT (recorded
        // in the format config); mount reads them from there and rejects
        // the flag.
        let fmt = Command::new(bin)
            .arg("format")
            .arg(format!("sqmeta://{}", meta_uris.join(",")))
            .arg(format!("sqdata://{}", data_uris.join(",")))
            .arg("--disk-cache-paths")
            .arg(&staging)
            .output()
            .expect("run squeezefs format");
        assert!(
            fmt.status.success(),
            "format failed: {}\n{}",
            String::from_utf8_lossy(&fmt.stdout),
            String::from_utf8_lossy(&fmt.stderr)
        );

        let logf = std::fs::File::create(&log).unwrap();
        let child = Command::new(bin)
            .arg("mount")
            .arg(format!("sqmeta://{}", meta_uris.join(",")))
            .arg(&mnt)
            .arg("--uid")
            .arg(unsafe { libc::getuid() }.to_string())
            .arg("--gid")
            .arg(unsafe { libc::getgid() }.to_string())
            .stdout(Stdio::from(logf.try_clone().unwrap()))
            .stderr(Stdio::from(logf))
            .spawn()
            .expect("spawn squeezefs mount");

        let mount = Mount {
            child,
            mnt,
            base,
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mount did not become ready in 90s; log:\n{}",
                std::fs::read_to_string(&mount.log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        mount
    }

    fn config_json(mount: &Mount) -> serde_json::Value {
        let raw = std::fs::read_to_string(mount.mnt.join(".config")).expect("read .config");
        serde_json::from_str(&raw).expect(".config must be valid JSON")
    }

    fn assert_daemon_log_clean(mount: &Mount) {
        let log = std::fs::read_to_string(&mount.log).unwrap_or_default();
        for needle in [
            "No such file or directory",
            "Input/output error",
            "Failed to open NVMe device",
        ] {
            assert!(
                !log.contains(needle),
                "daemon log contains {needle:?}:\n{}",
                log.lines()
                    .filter(|l| l.contains(needle))
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    /// The user's exact multi-volume shape, bench-shaped burst: a striped
    /// 32 MiB sequential write in 1 MiB chunks, fsync, full read-back
    /// verification, delete — with `.config` listing exactly oss1..oss4 and a
    /// clean daemon log throughout.
    #[test]
    fn test_cli_multi_volume_user_shape_burst_and_config() {
        if !transport_supported() {
            return;
        }
        let mut mount = mount_volumes("multi", &["oss1", "oss2", "oss3", "oss4"]);

        // .config: EXACTLY the named volumes, no phantom.
        let cfg = config_json(&mount);
        let table = cfg["data_volumes"]
            .as_object()
            .expect("data_volumes object");
        let mut keys: Vec<_> = table.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["oss1", "oss2", "oss3", "oss4"],
            ".config data_volumes must list exactly the named volumes; got {keys:?}"
        );

        // User report 2026-07-22: the catted bytes are exact-size JSON —
        // no tail padding beyond one final newline — every data volume
        // reports its REAL backing device path + sqdata:// URI, and the
        // volume-set sqmeta:// URI (the CLI-accepted spelling) appears.
        let raw = std::fs::read_to_string(mount.mnt.join(".config")).expect("read .config");
        assert_eq!(
            raw,
            format!("{}\n", raw.trim_end()),
            "catting .config must not print tail-padding whitespace"
        );
        assert!(
            cfg["sqmeta_uri"]
                .as_str()
                .unwrap_or_default()
                .starts_with("sqmeta://"),
            ".config must carry the volume-set sqmeta:// URI; got {:?}",
            cfg["sqmeta_uri"]
        );
        for name in ["oss1", "oss2", "oss3", "oss4"] {
            let real = mount.base.join(name).display().to_string();
            assert_eq!(
                table[name]["backing_dev"].as_str(),
                Some(real.as_str()),
                "data volume {name} must report its real backing device path"
            );
            assert_eq!(
                table[name]["uri"].as_str(),
                Some(format!("sqdata://{real}").as_str()),
                "data volume {name} must carry its canonical sqdata:// URI"
            );
        }

        // Bench-shaped burst (the user's first-128MB-write failure shape).
        let file_path = mount.mnt.join("bench_large_seq_0.bin");
        let chunk = {
            let mut c = vec![0u8; 1024 * 1024];
            for (i, b) in c.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            c
        };
        let nchunks = 32usize;
        {
            let mut f = std::fs::File::create(&file_path).expect("create bench file");
            for i in 0..nchunks {
                f.write_all(&chunk)
                    .unwrap_or_else(|e| panic!("bench-shaped write chunk {i} failed: {e}"));
            }
            f.sync_all().expect("fsync bench file");
        }

        // Read back and verify every chunk.
        {
            let mut f = std::fs::File::open(&file_path).expect("open bench file for read");
            let mut buf = vec![0u8; 1024 * 1024];
            for i in 0..nchunks {
                f.seek(SeekFrom::Start((i * 1024 * 1024) as u64)).unwrap();
                f.read_exact(&mut buf)
                    .unwrap_or_else(|e| panic!("read of chunk {i} failed: {e}"));
                assert_eq!(
                    buf, chunk,
                    "chunk {i} read back wrong bytes — striped key resolved to the wrong volume"
                );
            }
        }

        std::fs::remove_file(&file_path).expect("delete bench file");
        assert_daemon_log_clean(&mount);
        mount.unmount();
    }

    /// Single-volume CLI smoke: same flow, one meta + one data volume; pins
    /// that the fix leaves the single-volume shape working end-to-end.
    #[test]
    fn test_cli_single_volume_smoke_unchanged() {
        if !transport_supported() {
            return;
        }
        let mut mount = mount_volumes("single", &["solo1"]);

        let cfg = config_json(&mount);
        let table = cfg["data_volumes"]
            .as_object()
            .expect("data_volumes object");
        let keys: Vec<_> = table.keys().cloned().collect();
        assert_eq!(
            keys,
            vec!["solo1"],
            ".config data_volumes must list exactly the single named volume"
        );

        let file_path = mount.mnt.join("solo.bin");
        let payload: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 249) as u8).collect();
        std::fs::write(&file_path, &payload).expect("write solo file");
        let got = std::fs::read(&file_path).expect("read solo file");
        assert_eq!(got, payload, "single-volume read-back mismatch");
        std::fs::remove_file(&file_path).unwrap();

        assert_daemon_log_clean(&mount);
        mount.unmount();
    }
}
