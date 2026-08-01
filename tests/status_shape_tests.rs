//! `squeezefs status` payload shape — red-first (fix/status-stale-fields).
//!
//! The user-facing contract for `get_volume_status` (the one builder
//! behind the `status` CLI verb, live-mounted or offline probe):
//!
//! - **No vestigial fields.** `ActiveWriteBackend` was a hardcoded `""`
//!   left over from a retired setting (write placement is the KD-16
//!   `PlacementTable`, design-volume-lifecycle §5.9 — there is no single
//!   "active write backend" to report). The key must not exist.
//! - **No hardcoded empty placeholders anywhere.** Every string value in
//!   the payload is real data; unset optional format fields
//!   (`MemCacheSize` / `DiskCacheSize` / `DiskCachePaths`) are OMITTED,
//!   never rendered as `""`/defaults.
//! - **`StorageBackends` is the durable volume set** (KD-5,
//!   design-volume-lifecycle §5.3): keyed by the never-reused volume id,
//!   each row carrying `id` / `backing_dev` / `status` (the REAL
//!   lifecycle state — active/disabled/draining/retired — mirroring the
//!   `.config` display convention), not a path-keyed row with a
//!   hardcoded `"enabled"`.
//! - Legacy (`data_lv`-only) configs grandfather through
//!   `FormatConfig::resolved_data_volumes()`: basename ids, state
//!   `active`.

use std::path::{Path, PathBuf};

use squeezefs::{
    DataVolumeRecord, FormatConfig, VOL_STATE_ACTIVE, VOL_STATE_DRAINING, VOL_STATE_RETIRED,
};

const BLOCK: u64 = 4096;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config() -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: None,
        data_volumes: None,
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
    }
}

async fn format_meta(meta: &Path, cfg: &FormatConfig) {
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// Walk the whole payload: no string value anywhere may be the empty
/// string — a `""` is always a placeholder for data we do not have, and
/// the contract is omit-not-fake.
fn assert_no_empty_strings(value: &serde_json::Value, path: &str) {
    match value {
        serde_json::Value::String(s) => {
            assert!(
                !s.is_empty(),
                "status payload carries an empty-string placeholder at {path}"
            );
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                assert_no_empty_strings(item, &format!("{path}[{i}]"));
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                assert_no_empty_strings(v, &format!("{path}.{k}"));
            }
        }
        _ => {}
    }
}

/// The modern shape: durable `data_volumes` records (KD-5) with mixed
/// lifecycle states on a multi-volume set.
#[tokio::test(flavor = "multi_thread")]
async fn test_status_shape_durable_multi_volume() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta.img", 256 * 1024 * 1024);
    let d1 = make_file(dir.path(), "data1.img", 64 * 1024 * 1024);
    let d2 = make_file(dir.path(), "data2.img", 64 * 1024 * 1024);

    let rec1 = DataVolumeRecord {
        id: "vol-00000000000000a1".to_string(),
        backing_dev: d1.display().to_string(),
        state: VOL_STATE_ACTIVE.to_string(),
        added_ts: 1_700_000_000,
    };
    let rec2 = DataVolumeRecord {
        id: "vol-00000000000000b2".to_string(),
        backing_dev: d2.display().to_string(),
        state: VOL_STATE_DRAINING.to_string(),
        added_ts: 1_700_000_001,
    };
    // A retired id-tombstone (VL4 §5.4: evacuation done, path CLEARED,
    // id kept forever — KD-5). Its row must surface the permanent id +
    // state with the backing_dev key OMITTED (the cleared path is "not
    // a device anymore", never rendered as "").
    let rec3 = DataVolumeRecord {
        id: "vol-00000000000000c3".to_string(),
        backing_dev: String::new(),
        state: VOL_STATE_RETIRED.to_string(),
        added_ts: 1_700_000_002,
    };

    let mut cfg = base_format_config();
    cfg.mem_cache_size = Some("64MB".to_string());
    cfg.data_lv = Some(vec![rec1.backing_dev.clone(), rec2.backing_dev.clone()]);
    cfg.data_volumes = Some(vec![rec1.clone(), rec2.clone(), rec3.clone()]);
    format_meta(&meta, &cfg).await;

    let status = squeezefs::fuse_client::get_volume_status(&meta.display().to_string())
        .await
        .expect("status probe");

    // 1. The vestigial field is GONE.
    let setting = status["Setting"]
        .as_object()
        .expect("status carries a Setting object");
    assert!(
        !setting.contains_key("ActiveWriteBackend"),
        "vestigial ActiveWriteBackend key must not exist: {status:#}"
    );

    // 2. No hardcoded empty placeholders anywhere in the payload.
    assert_no_empty_strings(&status, "status");

    // 3. The real format fields are populated.
    assert_eq!(setting["Name"], "squeezefs");
    assert_eq!(setting["BlockSize"], BLOCK);
    assert_eq!(setting["Capacity"], 1u64 << 30);
    assert_eq!(setting["Inodes"], 1_000_000);
    assert_eq!(setting["Compression"], "none");
    assert_eq!(setting["EncryptAlgo"], "none");
    assert_eq!(setting["MemCacheSize"], "64MB");

    // 4. Unset optional format fields are OMITTED, not rendered as
    //    ""/[] defaults.
    assert!(
        !setting.contains_key("DiskCacheSize"),
        "unset DiskCacheSize must be omitted: {status:#}"
    );
    assert!(
        !setting.contains_key("DiskCachePaths"),
        "unset DiskCachePaths must be omitted: {status:#}"
    );

    // 5. StorageBackends is the durable volume set: keyed by the
    //    never-reused id, honest lifecycle state per record.
    let backends = setting["StorageBackends"]
        .as_object()
        .expect("StorageBackends object");
    assert_eq!(backends.len(), 3, "all volume records listed: {status:#}");
    for rec in [&rec1, &rec2] {
        let row = backends.get(&rec.id).unwrap_or_else(|| {
            panic!("StorageBackends keyed by durable id {}: {status:#}", rec.id)
        });
        assert_eq!(row["id"], rec.id.as_str());
        assert_eq!(row["backing_dev"], rec.backing_dev.as_str());
        assert_eq!(
            row["status"],
            rec.state.as_str(),
            "status must be the record's REAL lifecycle state, not a hardcoded 'enabled'"
        );
    }
    // The retired tombstone: permanent id + state, backing_dev OMITTED
    // (path cleared at retirement — never rendered as "").
    let tomb = backends
        .get(&rec3.id)
        .unwrap_or_else(|| panic!("retired tombstone {} listed: {status:#}", rec3.id));
    assert_eq!(tomb["id"], rec3.id.as_str());
    assert_eq!(tomb["status"], VOL_STATE_RETIRED);
    assert!(
        tomb.as_object().unwrap().get("backing_dev").is_none(),
        "retired tombstone's cleared path must be omitted: {status:#}"
    );

    // 6. Clients rides along as a real (possibly empty) array.
    assert!(status["Clients"].is_array(), "Clients array: {status:#}");
}

/// The legacy (`data_lv`-only) shape grandfathers through
/// `resolved_data_volumes()`: basename ids, state `active`.
#[tokio::test(flavor = "multi_thread")]
async fn test_status_shape_legacy_data_lv() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta.img", 256 * 1024 * 1024);
    let d1 = make_file(dir.path(), "legacy1.img", 64 * 1024 * 1024);

    let mut cfg = base_format_config();
    cfg.data_lv = Some(vec![d1.display().to_string()]);
    format_meta(&meta, &cfg).await;

    let status = squeezefs::fuse_client::get_volume_status(&meta.display().to_string())
        .await
        .expect("status probe");

    let setting = status["Setting"].as_object().unwrap();
    assert!(!setting.contains_key("ActiveWriteBackend"));
    assert_no_empty_strings(&status, "status");

    let backends = setting["StorageBackends"].as_object().unwrap();
    assert_eq!(backends.len(), 1);
    let row = backends
        .get("legacy1.img")
        .expect("legacy volume keyed by its grandfathered basename id");
    assert_eq!(row["id"], "legacy1.img");
    assert_eq!(row["backing_dev"], d1.display().to_string());
    assert_eq!(row["status"], VOL_STATE_ACTIVE);
}
