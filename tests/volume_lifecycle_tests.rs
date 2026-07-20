//! PR VL3 — durable volume identity + online data-volume add, red-first
//! (docs/design-volume-lifecycle.md §5.3, KD-5, KD-14, gate G-VL-2):
//!
//! - **Durable never-reused volume ids** (KD-5): `DataVolumeRecord`
//!   (`vol-{16 hex}`), additive `FormatConfig.data_volumes` (old configs
//!   decode fine), and **legacy grandfathering byte-identity** — a set
//!   with only `data_lv` resolves records whose ids are EXACTLY the
//!   device-path basenames, so every existing `name://offset` block key
//!   keeps resolving to the same backend.
//! - **`KV_VOLUME_LIFECYCLE` = incompat bit 3** (KD-14): non-intersection
//!   with bits 0/1/2 pinned; a binary whose `FEATURES_INCOMPAT_KNOWN`
//!   lacks it (the pre-VL3 mask, simulated by masking) must REFUSE the
//!   mount loud; the bit is set durably BEFORE the first non-legacy
//!   volume record commits (bit-before-durable-record ordering) and never
//!   on untouched sets.
//! - **`volume add-data`** (§5.3): offline guarded verb — live-client
//!   refusal, device validation (exists, not already a member, probe
//!   write+readback), record append + `data_lv` mirror, duplicate-add
//!   refusal.
//! - **`volume list`** offline probe; add→write→remount(reopen)→read at
//!   the library level with real `DataRouter` backends; the new backend
//!   receives allocations (G-VL-2 minus the VL4 auto-rebalance clause).
//! - **Health-override re-home**: the `/dev/shm` runtime-config file is
//!   gone; live overrides ride `BackendRouter::set_health_override`
//!   (unknown ids refused — the phantom-backend_0 protection
//!   re-expressed); offline enable/disable is durable volume state in
//!   the records.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, FEATURE_INCOMPAT_KV_V3, FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
    FEATURE_INCOMPAT_NODE_SEQ_WATERMARK, FEATURES_INCOMPAT_KNOWN,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_ACTIVE, VOL_STATE_DISABLED};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::TempDir;

const BLOCK: usize = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
    }
}

/// Format one v3 meta volume carrying a legacy-shaped format config
/// (`data_lv` only — the pre-VL3 on-disk state).
async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// A mount-shaped fixture built EXACTLY the way `main.rs` registers data
/// backends: resolved volume records drive `register_backend`, the first
/// record's device/allocator are the router's default slot.
struct Fx {
    fs: SqueezefsFilesystem,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    _staging: TempDir,
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), &first.id)
            .await
            .unwrap(),
    );
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }

    let staging = tempfile::tempdir().unwrap();
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

    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router
        .backend_router
        .set_volume_records(records.to_vec());
    router
        .backend_router
        .active_write_backend
        .store(Arc::new(first.id.clone()));

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    Fx {
        fs,
        meta: routed,
        _staging: staging,
    }
}

impl Fx {
    /// Clean "unmount": release the D0 claims so a guarded offline verb
    /// (or a reopen) can take them.
    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

async fn create_file(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Striped burst (the phantom-suite shape): force the striped layout with
/// a >4096-byte initial write, then write `nblocks` distinct blocks and
/// fsync. Returns the expected content.
async fn striped_burst(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let dummy = vec![0u8; BLOCK + 1];
    fx.fs
        .write(req(), ino, 0, 0, bytes::Bytes::copy_from_slice(&dummy), 0, 0)
        .await
        .unwrap();
    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = vec![(b as u8) ^ 0xA7; BLOCK];
        expected.extend_from_slice(&data);
        fx.fs
            .write(
                req(),
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
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    expected
}

fn is_vol_id(id: &str) -> bool {
    id.strip_prefix("vol-")
        .map(|h| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// KD-5: record shape, serde compat, legacy grandfathering byte-identity
// ---------------------------------------------------------------------------

#[test]
fn test_format_config_data_volumes_round_trip_and_old_configs_decode() {
    // Old config JSON (no data_volumes) must decode with None — additive
    // serde field, forward-only law.
    let old = serde_json::json!({
        "name": "squeezefs",
        "block_size": 4194304u64,
        "capacity": 1073741824u64,
        "inodes": 1000000u64,
        "compression": "none",
        "encrypt_algo": "none",
        "data_lv": ["/dev/nvme1n2"],
    });
    let cfg: FormatConfig = serde_json::from_value(old).expect("old config decodes");
    assert!(cfg.data_volumes.is_none(), "additive field defaults to None");

    // And a None field must not serialize (byte-identity for untouched
    // sets: the config JSON of a set that never used lifecycle verbs
    // carries no new key).
    let out = serde_json::to_value(&cfg).unwrap();
    assert!(
        out.get("data_volumes").is_none(),
        "None data_volumes must be skipped when serializing: {out}"
    );

    // Round-trip with records.
    let mut cfg2 = cfg.clone();
    cfg2.data_volumes = Some(vec![DataVolumeRecord {
        id: "vol-00deadbeef001122".to_string(),
        backing_dev: "/dev/nvme2n1".to_string(),
        state: VOL_STATE_ACTIVE.to_string(),
        added_ts: 1234,
    }]);
    let json = serde_json::to_vec(&cfg2).unwrap();
    let back: FormatConfig = serde_json::from_slice(&json).unwrap();
    let recs = back.data_volumes.expect("records survive");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].id, "vol-00deadbeef001122");
    assert_eq!(recs[0].backing_dev, "/dev/nvme2n1");
    assert_eq!(recs[0].state, VOL_STATE_ACTIVE);
    assert_eq!(recs[0].added_ts, 1234);
}

#[test]
fn test_legacy_data_lv_resolves_basename_ids_byte_identically() {
    let cfg = base_format_config(&[
        Path::new("/dev/mapper/xai-oss1"),
        Path::new("/dev/nvme1n2"),
        Path::new("/dev/shm/squeezefs_il_backend"),
    ]);
    let recs = cfg.resolved_data_volumes();
    let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
    // EXACTLY today's basename ids — every existing `name://offset` block
    // key resolves through these names (grandfathering, KD-5).
    assert_eq!(ids, vec!["xai-oss1", "nvme1n2", "squeezefs_il_backend"]);
    for rec in &recs {
        assert_eq!(rec.state, VOL_STATE_ACTIVE);
    }
    assert_eq!(recs[1].backing_dev, "/dev/nvme1n2");

    // Durable records win over the legacy field when both exist.
    let mut cfg2 = cfg;
    cfg2.data_volumes = Some(vec![DataVolumeRecord {
        id: "vol-0011223344556677".to_string(),
        backing_dev: "/dev/nvme9n9".to_string(),
        state: VOL_STATE_ACTIVE.to_string(),
        added_ts: 0,
    }]);
    let recs2 = cfg2.resolved_data_volumes();
    assert_eq!(recs2.len(), 1);
    assert_eq!(recs2[0].id, "vol-0011223344556677");
}

#[test]
fn test_new_volume_ids_are_vol_hex16_and_unique() {
    let a = squeezefs::new_data_volume_id();
    let b = squeezefs::new_data_volume_id();
    assert!(is_vol_id(&a), "id {a:?} must be vol-{{16 hex}}");
    assert!(is_vol_id(&b), "id {b:?} must be vol-{{16 hex}}");
    assert_ne!(a, b, "ids are random, never reused");
}

// ---------------------------------------------------------------------------
// KD-14: KV_VOLUME_LIFECYCLE = bit 3
// ---------------------------------------------------------------------------

#[test]
fn test_lifecycle_bit_is_bit3_and_does_not_intersect_bits_0_1_2() {
    assert_eq!(FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE, 1 << 3, "KD-14: bit 3");
    // Bits 0/1 are KV_V3 / NODE_SEQ_WATERMARK; bit 2 is reserved for
    // KV_GUEST_SLOTS (PR VL5a) — the new constant must intersect none.
    let taken = FEATURE_INCOMPAT_KV_V3 | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK | (1u64 << 2);
    assert_eq!(
        FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE & taken,
        0,
        "bit 3 must not intersect bits 0/1/2"
    );
    // This binary understands the bit (mounts stamped sets).
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        0,
        "FEATURES_INCOMPAT_KNOWN must include the lifecycle bit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_old_binary_mask_refuses_bit3_and_set_is_durable_idempotent() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let data = make_file(dir.path(), "oss1", 64 * 1024 * 1024);
    format_meta(&meta, &[&data]).await;

    // An untouched set carries NO lifecycle bit (zero-cost grandfathering).
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        0,
        "format alone must never set the lifecycle bit"
    );

    // First lifecycle use sets it durably; the second call is a no-op.
    assert!(
        squeezefs::meta_backend::kv::superblock::set_volume_lifecycle_bit(&meta)
            .await
            .expect("set bit"),
        "first set reports newly-set"
    );
    assert!(
        !squeezefs::meta_backend::kv::superblock::set_volume_lifecycle_bit(&meta)
            .await
            .expect("idempotent set"),
        "second set is a no-op"
    );
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    assert_ne!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        0,
        "the bit must be durable on disk"
    );

    // The REAL old-mask gate: decode sector 0 with the pre-VL3
    // FEATURES_INCOMPAT_KNOWN (bits 0|1) — the refusal must fire and name
    // bit 3 (never silently mount a lifecycle-stamped set on an old
    // binary).
    let mut sector = vec![0u8; 4096];
    use std::io::Read;
    std::fs::File::open(&meta)
        .unwrap()
        .read_exact(&mut sector)
        .unwrap();
    let old_mask = FEATURE_INCOMPAT_KV_V3 | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK;
    let err = squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector, old_mask,
    )
    .expect_err("a pre-VL3 known-mask must refuse a bit-3 superblock");
    let msg = format!("{err}");
    assert!(
        msg.contains("bit 3"),
        "refusal must name the unknown bit: {msg}"
    );
    // The current binary mounts it fine.
    squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector,
        FEATURES_INCOMPAT_KNOWN,
    )
    .expect("this binary understands bit 3");
}

// ---------------------------------------------------------------------------
// §5.3: offline guarded add — validation, ordering, duplicate refusal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_offline_add_refuses_under_live_client_and_bad_devices() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1]).await;
    let meta_lvs = vec![meta.display().to_string()];

    // Live-client gate: while a writer holds the D0 claim the guarded
    // verb refuses (set-cache-paths template).
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let err = squeezefs::config_ops::add_data_volume(&meta_lvs, oss2.to_str().unwrap())
        .await
        .expect_err("add under a live client must refuse");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("mount") || msg.contains("live") || msg.contains("claim"),
        "refusal must name the live client: {msg}"
    );
    fx.close().await;

    // Nonexistent device refuses.
    let err = squeezefs::config_ops::add_data_volume(&meta_lvs, "/nonexistent/nowhere")
        .await
        .expect_err("missing device must refuse");
    assert!(!format!("{err}").is_empty());

    // Already-a-member device refuses (legacy member by path).
    let err = squeezefs::config_ops::add_data_volume(&meta_lvs, oss1.to_str().unwrap())
        .await
        .expect_err("adding an existing member must refuse");
    let msg = format!("{err}").to_lowercase();
    assert!(msg.contains("member") || msg.contains("already"), "{msg}");

    // A meta volume can never be a data volume.
    let err = squeezefs::config_ops::add_data_volume(&meta_lvs, meta.to_str().unwrap())
        .await
        .expect_err("adding a meta volume as data must refuse");
    let msg = format!("{err}").to_lowercase();
    assert!(msg.contains("meta"), "{msg}");

    // Nothing above may have stamped the lifecycle bit (refusals are
    // side-effect-free; bit-before-record means bit only on success).
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        0,
        "refused adds must not stamp the lifecycle bit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_write_remount_read_and_new_backend_receives_allocations() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1]).await;
    let meta_lvs = vec![meta.display().to_string()];

    // Mount the legacy single-volume set, write + fsync a striped burst.
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    assert_eq!(recs[0].id, "oss1", "legacy id grandfathered byte-identically");
    let fx = open_fixture(&meta, &recs).await;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, 16).await;
    fx.close().await;

    // Offline add (guarded open; the set is unmounted now).
    let rec = squeezefs::config_ops::add_data_volume(&meta_lvs, oss2.to_str().unwrap())
        .await
        .expect("offline add-data");
    assert!(is_vol_id(&rec.id), "new volumes get vol- ids: {}", rec.id);
    assert_eq!(rec.state, VOL_STATE_ACTIVE);
    assert_eq!(rec.backing_dev, oss2.display().to_string());

    // Bit-before-durable-record: the lifecycle bit is now on the member
    // superblock (write ordering pinned by construction: record commit
    // follows the bit write).
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    assert_ne!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        0,
        "add-data must stamp KV_VOLUME_LIFECYCLE"
    );

    // Duplicate add refuses.
    let err = squeezefs::config_ops::add_data_volume(&meta_lvs, oss2.to_str().unwrap())
        .await
        .expect_err("duplicate add must refuse");
    let msg = format!("{err}").to_lowercase();
    assert!(msg.contains("member") || msg.contains("already"), "{msg}");

    // The offline probe sees both records: the grandfathered legacy id
    // and the new vol- id, in order.
    let listed = squeezefs::config_ops::resolved_volume_records(&meta_lvs)
        .await
        .expect("volume list probe");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "oss1");
    assert_eq!(listed[1].id, rec.id);
    // data_lv mirrors the backing set (forward-compat mirroring §7).
    let cfg = squeezefs::config_ops::read_volume_format_config(&meta_lvs)
        .await
        .expect("config probe");
    assert_eq!(
        cfg.data_lv.as_deref().unwrap_or_default().len(),
        2,
        "data_lv mirrors the volume set"
    );

    // Remount: registration comes from the records; the manifest reads
    // back byte-identical; NEW writes reach the added backend (G-VL-2's
    // allocation-participation clause).
    let fx = open_fixture(&meta, &listed).await;
    {
        let keys: Vec<String> = fx
            .fs
            .router
            .backend_router
            .backends
            .iter()
            .map(|e| e.key().clone())
            .collect();
        assert!(keys.contains(&"oss1".to_string()), "legacy id registered");
        assert!(keys.contains(&rec.id), "new vol- id registered");
    }
    for b in 0..16usize {
        let reply = fx
            .fs
            .read(req(), ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("post-remount read of block {b} failed: {e:?}"));
        assert_eq!(
            &reply.data[..],
            &expected[b * BLOCK..(b + 1) * BLOCK],
            "block {b} must read back byte-identical after the add + remount"
        );
    }

    // New allocations land on the added volume (health round-robin admits
    // the empty volume immediately).
    let new_backend = fx.fs.router.backend_router.backends.get(&rec.id).unwrap();
    let new_alloc = new_backend.value().block_allocator.clone();
    drop(new_backend);
    for f in 0..4 {
        let ino2 = create_file(&fx, &format!("spread_{f}.bin")).await;
        striped_burst(&fx, ino2, 8).await;
    }
    assert!(
        new_alloc.get_used_blocks() > 0,
        "the added backend must receive allocations from new writes"
    );

    // volume_states rows (§10): every volume reports id/state/capacity.
    let states = fx.fs.router.backend_router.volume_states();
    assert_eq!(states.len(), 2);
    let row = states
        .iter()
        .find(|s| s.id == rec.id)
        .expect("added volume in volume_states");
    assert_eq!(row.state, VOL_STATE_ACTIVE);
    assert!(row.capacity_bytes > 0, "capacity from the device size");
    assert!(row.used_bytes > 0, "used tracks the allocator");
    assert!(row.healthy, "fresh volume is healthy");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Health-override re-home (the /dev/shm mechanism is deleted)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_live_health_override_rehomed_and_phantom_alias_protected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1, &oss2]).await;

    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let br = &fx.fs.router.backend_router;

    // Disable routes placement away from the volume.
    br.set_health_override("oss2", true)
        .expect("disable a registered volume");
    for _ in 0..16 {
        let (be_id, _, _) = br.get_active_backend().unwrap();
        assert_eq!(be_id, "oss1", "disabled volume must not take placements");
    }
    br.set_health_override("oss2", false)
        .expect("re-enable a registered volume");

    // Unknown ids refuse — and the reserved `backend_0` alias can never
    // be poisoned through the override (the phantom-backend_0 protection
    // re-expressed on the new mechanism).
    assert!(
        br.set_health_override("nope", true).is_err(),
        "unknown volume ids must refuse"
    );
    assert!(
        br.set_health_override("backend_0", true).is_err(),
        "the reserved backend_0 alias must refuse health overrides"
    );
    // Legacy alias reads keep working after the refused override attempt.
    let alloc = &br.default_allocator;
    let offset = alloc.allocate_block().await.unwrap();
    let payload = vec![0xC3u8; BLOCK];
    br.default_device
        .write_block(offset, bytes::Bytes::copy_from_slice(&payload))
        .await
        .unwrap();
    alloc.publish_block(offset);
    let got = br
        .read_block(&format!("backend_0://{offset}"), BLOCK)
        .await
        .expect("legacy alias read must survive a refused phantom override");
    assert_eq!(&got[..], &payload[..]);
    br.free_block(&offset.to_string()).await.unwrap();

    fx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_offline_disable_is_durable_volume_state() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let meta_lvs = vec![meta.display().to_string()];

    // Offline disable writes durable state into the records (guarded).
    squeezefs::config_ops::set_data_volume_state(&meta_lvs, "oss2", VOL_STATE_DISABLED)
        .await
        .expect("offline disable");
    let listed = squeezefs::config_ops::resolved_volume_records(&meta_lvs)
        .await
        .unwrap();
    assert_eq!(listed[1].id, "oss2");
    assert_eq!(listed[1].state, VOL_STATE_DISABLED, "state is durable");

    // Unknown ids refuse.
    assert!(
        squeezefs::config_ops::set_data_volume_state(&meta_lvs, "nope", VOL_STATE_DISABLED)
            .await
            .is_err(),
        "unknown volume ids must refuse"
    );

    // A mount honors the durable state: the disabled volume is excluded
    // from placement until re-enabled.
    let fx = open_fixture(&meta, &listed).await;
    for _ in 0..16 {
        let (be_id, _, _) = fx.fs.router.backend_router.get_active_backend().unwrap();
        assert_eq!(be_id, "oss1", "durably disabled volume must not place");
    }
    fx.close().await;

    squeezefs::config_ops::set_data_volume_state(&meta_lvs, "oss2", VOL_STATE_ACTIVE)
        .await
        .expect("offline re-enable");
    let listed = squeezefs::config_ops::resolved_volume_records(&meta_lvs)
        .await
        .unwrap();
    assert_eq!(listed[1].state, VOL_STATE_ACTIVE);
}
