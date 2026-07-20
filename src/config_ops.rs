use crate::error::{Result, SqueezefsError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Host-local RUNTIME overrides (`/dev/shm` — ephemeral by design, dies
/// at reboot). This is NOT the durable volume set: that is
/// `FormatConfig.data_lv` on the meta volumes (and, post
/// design-volume-lifecycle, the `data_volumes`/`meta_slot_map` records).
/// The only surviving semantics here are the **fail-stop health
/// overrides** (`enable`/`disable` statuses a live daemon ingests at
/// `.config` generation) — everything else this file once carried
/// (add/remove/migrate bookkeeping, redirections) was fake and was
/// deleted (design-volume-lifecycle §5.0). VL3 re-homes the overrides
/// onto the daemon admin lane and deletes this file entirely.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigList {
    pub diskcaches: Vec<DiskCacheInfo>,
    pub data_volumes: HashMap<String, String>,
    pub data_volume_statuses: HashMap<String, String>,
    pub metadata_volumes: HashMap<String, String>,
    pub metadata_volume_statuses: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskCacheInfo {
    pub path: PathBuf,
    pub status: String,
}

const CONFIG_FILE_PATH: &str = "/dev/shm/squeezefs_runtime_config.json";

pub fn load_or_create_config() -> ConfigList {
    if let Ok(content) = std::fs::read_to_string(CONFIG_FILE_PATH) {
        if let Ok(cfg) = serde_json::from_str::<ConfigList>(&content) {
            return cfg;
        }
    }
    // Default fallback: EVERYTHING starts empty. Real volumes register at
    // mount from the resolved format config; the only writers here are the
    // enable/disable health-override verbs. (Two phantom seeds used to leak
    // into every runtime config from this default — a `backend_0` alias
    // entry and a `meta_volume_0 → /dev/shm/squeezefs_pjdfs_meta` test
    // path. Both deleted; G-VL-1 pins their absence.)
    ConfigList {
        diskcaches: Vec::new(),
        data_volumes: HashMap::new(),
        data_volume_statuses: HashMap::new(),
        metadata_volumes: HashMap::new(),
        metadata_volume_statuses: HashMap::new(),
    }
}

pub fn save_config(cfg: &ConfigList) {
    if let Ok(content) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(CONFIG_FILE_PATH, content);
    }
}

pub async fn list_config(_redis_url: &str, _fs_name: &str) -> Result<ConfigList> {
    Ok(load_or_create_config())
}

/// Read the format-recorded volume-set config off the FIRST metadata
/// volume via a read-only probe mount (nothing written, safe against a
/// volume another process has live-mounted). Fails loud on blank /
/// legacy-v2 / unformatted volumes.
async fn read_format_config(first_meta: &str) -> Result<crate::FormatConfig> {
    let vol = crate::meta_backend::open_volume_probe(first_meta).await?;
    let val = vol
        .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "format configuration not found on metadata volume {first_meta}; \
                 is this volume formatted?"
            ))
        })?;
    serde_json::from_slice(&val).map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "failed to parse the format configuration on {first_meta}: {e}"
        ))
    })
}

/// Ownership that format-time stamps must carry (the staging/cache roots
/// AND the filesystem root inode) — the pure resolution rule shared by
/// `format`, `config set-cache-paths`, and the v3 builder's root-inode
/// stamp (all usually run under `sudo` for the block volumes, while both
/// the staging roots and the mounted tree are used by the operator's own
/// user):
///
/// - **root via sudo** (`SUDO_UID`/`SUDO_GID` present) ⇒ the INVOKING
///   user. Stamping raw `getuid()` (= root under sudo) forced users to
///   `chown -R` by hand before a user-mode mount could use its own
///   staging (EACCES).
/// - **genuine root** (no `SUDO_*` env) ⇒ root: a deliberate root
///   deployment is never second-guessed.
/// - **non-root** ⇒ the current identity (the chown is a no-op — and a
///   non-root invoker could not chown away from itself anyway).
///
/// Unparseable `SUDO_UID`/`SUDO_GID` values fall back per-field to the
/// effective identity (never a panic on a hostile environment).
pub fn resolve_invoking_owner(
    euid: u32,
    egid: u32,
    sudo_uid: Option<&str>,
    sudo_gid: Option<&str>,
) -> (u32, u32) {
    if euid != 0 {
        return (euid, egid);
    }
    (
        sudo_uid
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(euid),
        sudo_gid
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(egid),
    )
}

/// [`resolve_invoking_owner`] applied to the live process identity and
/// environment.
pub fn invoking_owner() -> (u32, u32) {
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let sudo_gid = std::env::var("SUDO_GID").ok();
    resolve_invoking_owner(
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
        sudo_uid.as_deref(),
        sudo_gid.as_deref(),
    )
}

/// Wipe + recreate + OWNERSHIP-STAMP one staging/cache root: the shared
/// format-grade stamp used by `format --disk-cache-paths` and
/// `config set-cache-paths`. The root comes up empty (fresh staging
/// generation, no discard noise) and owned by [`invoking_owner`], so the
/// daemon identity that will actually mount can use it without a manual
/// `chown -R`. The chown only runs as root (elsewhere it is a no-op by
/// construction); failures are loud — a half-stamped root is exactly the
/// EACCES-later trap this exists to close.
pub async fn stamp_staging_dir(dir: &Path) -> Result<()> {
    let ctx = |what: &str, e: &std::io::Error| {
        SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to {what} staging/cache dir '{}': {e}",
                dir.display()
            ),
        ))
    };
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ctx("wipe", &e)),
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| ctx("create", &e))?;
    let (uid, gid) = invoking_owner();
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(dir, Some(uid), Some(gid))
            .map_err(|e| ctx(&format!("stamp ownership {uid}:{gid} on"), &e))?;
    }
    Ok(())
}

/// `W_OK | X_OK` access check with the EFFECTIVE identity (`faccessat` +
/// `AT_EACCESS`) — the daemon needs to create/traverse inside the root.
fn effective_access_wx(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the
    // call; faccessat only reads it and touches no Rust-managed memory.
    let rc = unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Mount-side staging WRITABILITY preflight. Cache-path policy: mount is
/// side-effect-free on the staging roots — it never chowns/chmods; it
/// only verifies and fails loud with the remedy.
///
/// The daemon performs ALL staging I/O as the identity that runs
/// `squeezefs mount`; `--uid`/`--gid` change FUSE presentation only and
/// grant no staging access. A root-stamped root + user daemon therefore
/// has to fail HERE — naming the directory and the chown fix — instead of
/// surfacing a raw EACCES from deep inside bootstrap (the user-hit
/// failure mode). A missing root is fine when its nearest existing
/// ancestor is writable (mount creates the chain itself).
pub fn staging_write_preflight(dirs: &[PathBuf]) -> std::result::Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    let egid = unsafe { libc::getegid() };
    for dir in dirs {
        // The root itself when it exists, else the nearest existing
        // ancestor mount would have to create the chain under.
        let mut probe: &Path = dir.as_path();
        while !probe.exists() {
            probe = match probe.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                Some(_) => Path::new("."),
                None => Path::new("/"),
            };
        }
        let Err(e) = effective_access_wx(probe) else {
            continue;
        };
        let cause = if probe == dir.as_path() {
            format!(
                "staging/cache directory '{}' is not writable by the daemon identity \
                 (uid {euid} gid {egid}): {e}",
                dir.display()
            )
        } else {
            format!(
                "staging/cache directory '{}' cannot be created by the daemon identity \
                 (uid {euid} gid {egid}): nearest existing ancestor '{}' is not writable: {e}",
                dir.display(),
                probe.display()
            )
        };
        return Err(format!(
            "{cause}. Staging I/O runs as the user that executes `squeezefs mount`; \
             `--uid`/`--gid` only change FUSE presentation and grant no staging access, \
             and mount never chowns staging roots itself (cache-path policy). Remedy: \
             `sudo mkdir -p '{0}' && sudo chown -R {euid}:{egid} '{0}'`, or re-run \
             `squeezefs format`/`squeezefs config set-cache-paths` (they stamp ownership \
             to the invoking user), then retry the mount",
            dir.display()
        ));
    }
    Ok(())
}

/// `squeezefs config get-cache-paths`: the staging/cache directories the
/// filesystem was formatted with (`None`/empty ⇒ permanently cache-less).
pub async fn get_cache_paths(meta_lvs: &[String]) -> Result<Option<Vec<PathBuf>>> {
    let first = meta_lvs.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("at least one metadata volume is required".to_string())
    })?;
    Ok(read_format_config(first).await?.disk_cache_paths)
}

/// `squeezefs config set-cache-paths`: the ONLY way to change a
/// filesystem's staging/cache directories after format (mount rejects the
/// flag — cache-path policy).
///
/// Guarded like `format` itself:
/// - every metadata volume runs the [`format_preflight`] live-client gate
///   (`force` semantics: an already-formatted volume is fine, a volume any
///   client has LIVE-mounted refuses — changing cache paths under an
///   active mount is never safe);
/// - the volume set must be formatted (the config read fails loud
///   otherwise) — checked BEFORE any directory is touched;
/// - the NEW directories are wiped + recreated + OWNERSHIP-STAMPED (the
///   same [`stamp_staging_dir`] `format --disk-cache-paths` applies —
///   invoking user under sudo, root only for genuine root), so the next
///   mount stamps a fresh staging generation into empty dirs it can
///   actually write (no discard noise, no EACCES). Content safety does
///   not depend on the wipe: staging generation-binding discards foreign
///   content at mount anyway.
///
/// The rewrite itself is one setxattr transaction on the FIRST volume's
/// root inode (where format recorded it), made durable by the v3 journal
/// and closed with a clean checkpoint shutdown.
///
/// [`format_preflight`]: crate::meta_backend::kv::builder::format_preflight
pub async fn set_cache_paths(meta_lvs: &[String], paths: &[PathBuf]) -> Result<()> {
    let first = meta_lvs.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("at least one metadata volume is required".to_string())
    })?;
    if paths.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "at least one cache path is required".to_string(),
        ));
    }

    // 1. Live-client gate on EVERY volume before anything is touched.
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true).await?;
    }

    // 2. The volume set must be formatted; read the config to rewrite.
    let mut cfg = read_format_config(first).await?;

    // 3. Wipe + recreate + ownership-stamp the NEW dirs (format-grade
    //    cleanliness AND the SUDO_UID ownership rule).
    for dir in paths {
        stamp_staging_dir(dir).await?;
    }

    // 4. Rewrite the format config on the first volume (journal-durable
    //    commit + clean checkpoint shutdown).
    cfg.disk_cache_paths = Some(paths.to_vec());
    let bytes = serde_json::to_vec(&cfg).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
    })?;
    let vol = crate::meta_backend::open_volume_for_mount(first).await?;
    crate::meta_backend::Metadata::setxattr(
        vol.as_ref(),
        1,
        crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
        &bytes,
    )
    .await?;
    vol.shutdown().await.map_err(SqueezefsError::from)?;
    Ok(())
}

/// Read the format config off the FIRST metadata volume via a read-only
/// probe (the `volume list` / `df` access pattern — safe beside a live
/// mount, nothing written).
pub async fn read_volume_format_config(meta_lvs: &[String]) -> Result<crate::FormatConfig> {
    let first = meta_lvs.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("at least one metadata volume is required".to_string())
    })?;
    read_format_config(first).await
}

/// `squeezefs volume list` offline probe: the durable data-volume set in
/// volume order (legacy sets synthesize basename-id records — KD-5
/// grandfathering).
pub async fn resolved_volume_records(meta_lvs: &[String]) -> Result<Vec<crate::DataVolumeRecord>> {
    Ok(read_volume_format_config(meta_lvs)
        .await?
        .resolved_data_volumes())
}

/// Canonical form for membership comparison (symlinked device paths must
/// not smuggle a duplicate member in); falls back to the raw string for
/// paths that do not resolve.
fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// §5.3 step-2 device preflight, shared by the offline and online add
/// paths: exists and is a usable backing device, sized, not already a
/// data member, not a meta volume. Pure checks — no writes.
pub fn validate_new_data_volume(
    device: &str,
    existing: &[crate::DataVolumeRecord],
    meta_lvs: &[String],
) -> Result<u64> {
    crate::storage::validate_backing_device(device).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("data volume '{device}' failed validation: {e}"))
    })?;
    let capacity = crate::nvme_dev::device_capacity_bytes(device).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("cannot size data volume '{device}': {e}"))
    })?;
    let dev_canon = canon(device);
    for rec in existing {
        if canon(&rec.backing_dev) == dev_canon {
            return Err(SqueezefsError::InvalidOperation(format!(
                "'{device}' is already a member of the data-volume set (id '{}', state '{}') — \
                 volume ids are never reused (KD-5)",
                rec.id, rec.state
            )));
        }
    }
    for meta in meta_lvs {
        if canon(meta) == dev_canon {
            return Err(SqueezefsError::InvalidOperation(format!(
                "'{device}' is a metadata volume of this set — a meta volume can never be \
                 added as a data volume"
            )));
        }
    }
    Ok(capacity)
}

/// §5.3 step-2 probe: write+readback of a scratch block at offset 0 (no
/// allocator involvement — the device is not a member yet), restored to
/// zeros afterwards so the volume stays blank. Proves the device path is
/// writable through the same `NvmeBlockDev` io_uring worker the mount
/// will use — a read-only or vanished device fails HERE, before any
/// durable record names it.
pub async fn probe_data_volume_rw(device: &str) -> Result<()> {
    let dev = crate::nvme_dev::NvmeBlockDev::new(device);
    let mut pattern = vec![0u8; 4096];
    fastrand::fill(&mut pattern);
    let ctx = |what: &str, e: &SqueezefsError| {
        SqueezefsError::InvalidOperation(format!(
            "scratch-block {what} probe failed on '{device}': {e}"
        ))
    };
    dev.write_block(0, bytes::Bytes::copy_from_slice(&pattern))
        .await
        .map_err(|e| ctx("write", &e))?;
    let back = dev.read_block(0, 4096).await.map_err(|e| ctx("read", &e))?;
    if back[..] != pattern[..] {
        return Err(SqueezefsError::InvalidOperation(format!(
            "scratch-block readback mismatch on '{device}' — the device does not persist \
             writes (wrong path? overlapping volume?)"
        )));
    }
    dev.write_block(0, bytes::Bytes::from(vec![0u8; 4096]))
        .await
        .map_err(|e| ctx("blank-restore", &e))?;
    Ok(())
}

/// `squeezefs volume add-data` — the OFFLINE guarded path
/// (design-volume-lifecycle §5.3; the online path is the admin-lane
/// `volume-add-data` verb against the live daemon). Guarded like
/// `set-cache-paths`:
///
/// 1. live-client gate (`format_preflight`) on every meta volume;
/// 2. device preflight ([`validate_new_data_volume`]) + write/readback
///    probe ([`probe_data_volume_rw`]) — all refusals BEFORE any durable
///    effect;
/// 3. guarded open of the whole set (takes the D0 writer claims);
/// 4. **`KV_VOLUME_LIFECYCLE` bit 3 stamped durably on every member
///    superblock BEFORE the record commit** (bit-before-durable-record,
///    §7 — a crash after the bit and before the record leaves a set old
///    binaries refuse and this binary mounts unchanged);
/// 5. one setxattr tx on volume 0: `data_volumes` += the new `vol-` record
///    (legacy members materialized with their grandfathered basename
///    ids), `data_lv` mirrored;
/// 6. clean shutdown (claims released).
///
/// The §5.3 step-6 auto-rebalance default arms with the VL4 mover —
/// callers print that honestly.
pub async fn add_data_volume(meta_lvs: &[String], device: &str) -> Result<crate::DataVolumeRecord> {
    // 1. Live-client gate on EVERY volume before anything is touched.
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume add-data refused: {e}"))
            })?;
    }

    // 2. The set must be formatted; validate + probe the device.
    let cfg = read_volume_format_config(meta_lvs).await?;
    let mut records = cfg.resolved_data_volumes();
    validate_new_data_volume(device, &records, meta_lvs)?;
    probe_data_volume_rw(device).await?;

    let record = crate::DataVolumeRecord {
        id: crate::new_data_volume_id(),
        backing_dev: device.to_string(),
        state: crate::VOL_STATE_ACTIVE.to_string(),
        added_ts: std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    records.push(record.clone());

    commit_volume_records(meta_lvs, cfg, records).await?;
    Ok(record)
}

/// Offline durable enable/disable — the health override's durable home
/// after the `/dev/shm` runtime-config deletion: flips one record's
/// `state` (`active`/`disabled`) through the same guarded
/// bit-before-record path as `add_data_volume`. Idempotent no-ops never
/// materialize legacy records (an untouched set stays bit-identical).
pub async fn set_data_volume_state(
    meta_lvs: &[String],
    volume_id: &str,
    state: &str,
) -> Result<()> {
    if state != crate::VOL_STATE_ACTIVE && state != crate::VOL_STATE_DISABLED {
        return Err(SqueezefsError::InvalidOperation(format!(
            "unsupported volume state '{state}' (VL3 states: active|disabled; drain states \
             land with `volume remove-data`, PR VL4)"
        )));
    }
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume state change refused: {e}"))
            })?;
    }
    let cfg = read_volume_format_config(meta_lvs).await?;
    let mut records = cfg.resolved_data_volumes();
    let rec = records
        .iter_mut()
        .find(|r| r.id == volume_id)
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "unknown data volume '{volume_id}' (see `squeezefs volume list`)"
            ))
        })?;
    if rec.state == state {
        return Ok(()); // idempotent — never materializes records for a no-op
    }
    rec.state = state.to_string();
    commit_volume_records(meta_lvs, cfg, records).await
}

/// The shared durable tail of every offline lifecycle commit: guarded
/// open of the whole set (D0 claims — excludes racing mounts for the
/// commit's duration), **bit 3 on every member superblock first**, then
/// one setxattr tx on volume 0 with the updated records + `data_lv`
/// mirror, then clean shutdown.
async fn commit_volume_records(
    meta_lvs: &[String],
    mut cfg: crate::FormatConfig,
    records: Vec<crate::DataVolumeRecord>,
) -> Result<()> {
    let backends = crate::meta_backend::open_meta_volume_set(meta_lvs).await?;

    // Bit-before-durable-record (design-volume-lifecycle §7): stamp every
    // member superblock before the record exists. Sector 0 is never
    // rewritten by the open backends (checkpoints flip the root ledger),
    // so this is race-free under the held claims.
    for path in meta_lvs {
        if let Err(e) =
            crate::meta_backend::kv::superblock::set_volume_lifecycle_bit(Path::new(path)).await
        {
            for be in &backends {
                let _ = be.shutdown().await;
            }
            return Err(e.into());
        }
    }

    cfg.data_lv = Some(records.iter().map(|r| r.backing_dev.clone()).collect());
    cfg.data_volumes = Some(records);
    let bytes = serde_json::to_vec(&cfg).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
    })?;
    let commit = crate::meta_backend::Metadata::setxattr(
        backends[0].as_ref(),
        1,
        crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
        &bytes,
    )
    .await;
    for be in &backends {
        if let Err(te) = be.shutdown().await {
            log::warn!(
                "releasing guard on {:?} after a lifecycle commit failed: {te}",
                be.device_path()
            );
        }
    }
    commit?;
    Ok(())
}

pub async fn enable_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn disable_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "disabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn enable_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn disable_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "disabled".to_string());
    save_config(&cfg);
    Ok(())
}
