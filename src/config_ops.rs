use crate::error::{Result, SqueezefsError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigList {
    pub diskcaches: Vec<DiskCacheInfo>,
    pub data_volumes: HashMap<String, String>,
    pub data_volume_statuses: HashMap<String, String>,
    pub metadata_volumes: HashMap<String, String>,
    pub metadata_volume_statuses: HashMap<String, String>,
    pub metadata_volume_redirections: HashMap<String, String>,
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
    // Default fallback. Data volumes start EMPTY: real volumes register at
    // mount from the resolved data paths, and `config data-volume add` fills
    // this map explicitly. The old hardcoded `backend_0` seed (a legacy
    // key-resolution alias, not a volume — its `backing_dev` was not even a
    // real path) leaked a phantom entry into every runtime config.
    let data_volumes = HashMap::new();
    let data_volume_statuses = HashMap::new();

    let mut metadata_volumes = HashMap::new();
    let mut metadata_volume_statuses = HashMap::new();
    metadata_volumes.insert(
        "meta_volume_0".to_string(),
        "/dev/shm/squeezefs_pjdfs_meta".to_string(),
    );
    metadata_volume_statuses.insert("meta_volume_0".to_string(), "enabled".to_string());

    ConfigList {
        diskcaches: Vec::new(),
        data_volumes,
        data_volume_statuses,
        metadata_volumes,
        metadata_volume_statuses,
        metadata_volume_redirections: HashMap::new(),
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

pub async fn set_config_quota(
    _redis_url: &str,
    _fs_name: &str,
    _key: &str,
    _value: &str,
) -> Result<()> {
    Ok(())
}

pub async fn add_disk_cache_path(
    _redis_url: &str,
    _fs_name: &str,
    _path: &Path,
    _force: bool,
) -> Result<()> {
    Ok(())
}

pub async fn remove_disk_cache_path(
    _redis_url: &str,
    _fs_name: &str,
    _path: &Path,
    _force: bool,
) -> Result<()> {
    Ok(())
}

pub async fn enable_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn disable_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn flush_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn add_data_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
    backing_dev: Option<&str>,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volumes
        .insert(volume_id.to_string(), backing_dev.unwrap_or("").to_string());
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn remove_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volumes.remove(volume_id);
    cfg.data_volume_statuses.remove(volume_id);
    save_config(&cfg);
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

pub async fn add_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
    backing_dev: Option<&str>,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volumes
        .insert(volume_id.to_string(), backing_dev.unwrap_or("").to_string());
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn remove_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volumes.remove(volume_id);
    cfg.metadata_volume_statuses.remove(volume_id);
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

pub async fn migrate_data_volume(
    _redis_url: &str,
    _fs_name: &str,
    from_volume: &str,
    to_volume: &str,
) -> Result<()> {
    println!(
        "Migrating data volume from {} to {} via LVM pvmove...",
        from_volume, to_volume
    );
    let _ = crate::storage::run_cmd("pvmove", &[from_volume, to_volume]);

    let mut cfg = load_or_create_config();
    if let Some(dev) = cfg.data_volumes.remove(from_volume) {
        cfg.data_volumes.insert(to_volume.to_string(), dev);
        cfg.data_volume_statuses
            .insert(to_volume.to_string(), "enabled".to_string());
        cfg.data_volume_statuses
            .insert(from_volume.to_string(), "disabled".to_string());
    }
    save_config(&cfg);
    Ok(())
}

pub async fn migrate_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    from_volume: &str,
    to_volume: &str,
) -> Result<()> {
    // Route-config bookkeeping only: point the ino-routing redirection at
    // the new volume and flip the statuses. (The retired v2 backend used
    // to also best-effort copy fixed-geometry inode slots here — a
    // half-measure that never carried dentries/xattrs; no data movement
    // is performed.)
    println!(
        "Redirecting metadata volume {} to {} in the runtime config (no data is moved).",
        from_volume, to_volume
    );
    let mut cfg = load_or_create_config();
    cfg.metadata_volume_redirections
        .insert(from_volume.to_string(), to_volume.to_string());
    cfg.metadata_volume_statuses
        .insert(from_volume.to_string(), "disabled".to_string());
    cfg.metadata_volume_statuses
        .insert(to_volume.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn run_metadata_fsck(_redis_url: &str, _fs_name: &str) -> Result<Vec<String>> {
    Ok(Vec::new())
}
