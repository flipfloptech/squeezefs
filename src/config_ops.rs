//! Offline (guarded) admin verbs over the DURABLE on-volume state: the
//! cache-path policy verbs, the volume-lifecycle add/state verbs
//! (design-volume-lifecycle §5.3, PR VL3), and the format-grade
//! ownership/preflight helpers they share.
//!
//! The historical `/dev/shm/squeezefs_runtime_config.json` mechanism —
//! an ephemeral host-local file the daemon ingested for enable/disable
//! health overrides — was DELETED in PR VL3: live overrides now ride the
//! admin lane (`volume-disable`/`volume-enable` →
//! `BackendRouter::set_health_override`), and offline enable/disable is
//! durable volume state in the `FormatConfig.data_volumes` records
//! ([`set_data_volume_state`]).

use crate::error::{Result, SqueezefsError};
use std::path::{Path, PathBuf};

/// Read the format-recorded volume-set config off the FIRST metadata
/// volume via a read-only probe mount (nothing written, safe against a
/// volume another process has live-mounted). Fails loud on blank /
/// legacy-v2 / unformatted volumes.
async fn read_format_config(first_meta: &str) -> Result<crate::FormatConfig> {
    let vol = crate::meta_backend::open_volume_probe(first_meta).await?;
    // PR VL5b: after a slot-0 migration the config record lives in the
    // host's GUEST slot-0 keyspace, not at local ino 1.
    let root = vol.slot0_root_ino();
    let val = vol
        .getxattr(root, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
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

/// The volume the format config lives on: the host of routing slot 0 in
/// CANONICAL set order (ino 1 routes to slot 0 — PR VL5a §5.5.1a; for
/// legacy sets this is exactly the first URI-listed volume, today's
/// behavior). Every config read/write below resolves through this, so a
/// reordered stamped URI still finds the config.
async fn config_home_volume(meta_lvs: &[String]) -> Result<String> {
    if meta_lvs.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "at least one metadata volume is required".to_string(),
        ));
    }
    let disc = crate::meta_backend::discover_meta_set(meta_lvs).await?;
    Ok(disc.ordered_paths[disc.slot_to_volume[0]].clone())
}

/// `squeezefs config get-cache-paths`: the staging/cache directories the
/// filesystem was formatted with (`None`/empty ⇒ permanently cache-less).
pub async fn get_cache_paths(meta_lvs: &[String]) -> Result<Option<Vec<PathBuf>>> {
    let home = config_home_volume(meta_lvs).await?;
    Ok(read_format_config(&home).await?.disk_cache_paths)
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
    let first = config_home_volume(meta_lvs).await?;
    let first = &first;
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

/// Read the format config off the config-home metadata volume (the host
/// of routing slot 0 — the first volume for legacy sets) via a read-only
/// probe (the `volume list` / `df` access pattern — safe beside a live
/// mount, nothing written).
pub async fn read_volume_format_config(meta_lvs: &[String]) -> Result<crate::FormatConfig> {
    let home = config_home_volume(meta_lvs).await?;
    read_format_config(&home).await
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
/// §5.3 step 6 (KD-12): the auto-rebalance default in its OFFLINE
/// posture — no live mount exists, so the verb commits a durable
/// **Queued** `rebalance` job record beside the volume record; the next
/// mount's fabric adopts and runs it. `no_rebalance` suppresses it.
pub async fn add_data_volume(
    meta_lvs: &[String],
    device: &str,
    no_rebalance: bool,
) -> Result<crate::DataVolumeRecord> {
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

    let extra = if no_rebalance {
        Vec::new()
    } else {
        vec![crate::jobs::durable_queued_job_xattr(
            &crate::jobs::JobType::Rebalance,
            crate::jobs::REBALANCE_DEFAULT_THROTTLE_PCT,
        )]
    };
    commit_volume_records(meta_lvs, cfg, records, extra).await?;
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
    commit_volume_records(meta_lvs, cfg, records, Vec::new()).await
}

/// `squeezefs volume undrain` — the OFFLINE guarded path (§5.4): flip a
/// `draining` record back to `active` and mark the volume's durable
/// evacuation-job records cancelled so no future mount adopts them.
/// Retired volumes refuse (terminal — ids are permanent, KD-5).
pub async fn undrain_data_volume(meta_lvs: &[String], volume_id: &str) -> Result<()> {
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume undrain refused: {e}"))
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
    if rec.state != crate::VOL_STATE_DRAINING {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume '{volume_id}' is '{}', not draining — nothing to undrain (retired \
             volumes never come back: ids are permanent, KD-5)",
            rec.state
        )));
    }
    rec.state = crate::VOL_STATE_ACTIVE.to_string();

    // Cancel the durable evacuation records for this volume in the same
    // guarded open (an adopted Queued/Running record would restart the
    // drain at the next mount). Routed open: stamped sets route with
    // their frozen width/slot map (PR VL5a).
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result: Result<()> = async {
        for mut job in crate::jobs::JobFabric::list_records(&routed).await? {
            let matches = matches!(
                &job.job_type,
                crate::jobs::JobType::EvacuateVolume { volume_id: v } if v == volume_id
            );
            if matches && !job.state.is_terminal() {
                job.state = crate::jobs::JobState::Cancelled;
                let name = format!("{}{}", crate::jobs::JOB_XATTR_PREFIX, job.job_id);
                let bytes = serde_json::to_vec(&job).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("job record encode: {e}"))
                })?;
                crate::meta_backend::Metadata::setxattr(routed.as_ref(), 1, &name, &bytes).await?;
            }
        }
        // The state flip rides the same open (bit 3 is already set —
        // draining implies a prior lifecycle commit).
        let mut cfg = cfg;
        cfg.data_lv = Some(
            records
                .iter()
                .filter(|r| r.state != crate::VOL_STATE_RETIRED)
                .map(|r| r.backing_dev.clone())
                .collect(),
        );
        cfg.data_volumes = Some(records);
        let bytes = serde_json::to_vec(&cfg).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
        })?;
        crate::meta_backend::Metadata::setxattr(
            routed.as_ref(),
            1,
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            &bytes,
        )
        .await?;
        Ok(())
    }
    .await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after undrain: {e}");
        }
    }
    result
}

/// `squeezefs volume remove-data` — the OFFLINE posture (§5.8): the
/// short-lived **D0-guarded coordinator process**. Guarded open of the
/// whole set, an in-process data router + job fabric (mover wired, no
/// FUSE surface), the §5.2 preflight, the durable `Active → Draining`
/// flip, and the evacuation run **to completion** with progress output
/// — the volume retires before this returns. Staging note (honest):
/// the offline coordinator opens NO staging dirs (they are per-mount
/// isolated); a cleanly-unmounted set carries no staged records by
/// invariant, and anything ring-resident belongs to the next mount's
/// recovery, not to this drain.
pub async fn remove_data_volume_offline(
    meta_lvs: &[String],
    volume_id: &str,
    throttle_pct: u32,
) -> Result<()> {
    use crate::error::SqueezefsError as E;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| E::InvalidOperation(format!("volume remove-data refused: {e}")))?;
    }
    let cfg = read_volume_format_config(meta_lvs).await?;
    let records = cfg.resolved_data_volumes();
    let victim = records.iter().find(|r| r.id == volume_id).ok_or_else(|| {
        E::InvalidOperation(format!(
            "unknown data volume '{volume_id}' (see `squeezefs volume list`)"
        ))
    })?;
    if victim.state != crate::VOL_STATE_ACTIVE && victim.state != crate::VOL_STATE_DRAINING {
        return Err(E::InvalidOperation(format!(
            "volume '{volume_id}' is '{}' — only an active (or already-draining) volume \
             can be removed",
            victim.state
        )));
    }

    // Guarded open (the D0 claims) + the in-process engine. Routed:
    // stamped sets route with their frozen width/slot map (PR VL5a).
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result = offline_drain_body(&routed, meta_lvs, cfg, records, volume_id, throttle_pct).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after offline remove-data: {e}");
        }
    }
    result
}

/// The offline coordinator body (split so the guard release above runs
/// on every path).
async fn offline_drain_body(
    routed: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    meta_lvs: &[String],
    cfg: crate::FormatConfig,
    records: Vec<crate::DataVolumeRecord>,
    volume_id: &str,
    throttle_pct: u32,
) -> Result<()> {
    use crate::error::SqueezefsError as E;

    // The in-process data plane: the mount-shaped router over the
    // record set (retired members skipped; the first live record's
    // device/allocator are the default slot — the bare-key invariant).
    let dlm = crate::dlm::DlmClient::new("local")?;
    let live: Vec<&crate::DataVolumeRecord> = records
        .iter()
        .filter(|r| r.state != crate::VOL_STATE_RETIRED)
        .collect();
    let first = live
        .first()
        .ok_or_else(|| E::InvalidOperation("no live data volumes".to_string()))?;
    let first_alloc = std::sync::Arc::new(
        crate::block_allocator::BlockAllocator::new(dlm.meta_client().clone(), &first.id).await?,
    );
    if let Ok(cap) = crate::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let first_dev = std::sync::Arc::new(crate::nvme_dev::NvmeBlockDev::new(&first.backing_dev));
    if cfg.block_size > 0 {
        std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", cfg.block_size.to_string());
    }
    let cache = crate::cache::TieredCache::new(
        Vec::new(), // no staging dirs: per-mount isolated, not ours (see the verb doc)
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await?;
    let router = crate::routing::DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    router.set_block_size(cfg.block_size);
    for rec in &live {
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await?;
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(routed.clone());

    // Allocator refcount recovery — the census ground truth (§5.2).
    for kv in &routed.volumes {
        for entry in router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &router.backend_router)
                .await?;
        }
    }

    // The fabric with the mover wired (router-only quiescence probe —
    // no FUSE layer exists in this process by construction).
    let fabric = crate::jobs::JobFabric::start(
        routed.clone(),
        2,
        100,
        Some(crate::jobs::MoverCtx::router_only(router.clone())),
    )
    .await?;

    // §5.2 preflight (honest refusal with the exact numbers).
    let ctx = fabric.mover_ctx().expect("mover context was just wired");
    let pf = crate::jobs::drain_preflight(routed, ctx, volume_id, fabric.worker_count()).await?;
    if !pf.admits() {
        fabric.shutdown_abrupt().await;
        return Err(E::InvalidOperation(pf.refusal()));
    }
    println!(
        "preflight OK: needed {} B, avail {} B, transient {} B, headroom {} B",
        pf.needed_bytes, pf.avail_bytes, pf.transient_bytes, pf.headroom_bytes
    );

    // Durable Active → Draining (bit 3 first, §7 ordering), one tx.
    for path in meta_lvs {
        crate::meta_backend::kv::superblock::set_volume_lifecycle_bit(Path::new(path)).await?;
    }
    if records
        .iter()
        .find(|r| r.id == volume_id)
        .is_some_and(|r| r.state == crate::VOL_STATE_ACTIVE)
    {
        let mut cfg = cfg;
        let mut recs = records.clone();
        recs.iter_mut()
            .find(|r| r.id == volume_id)
            .expect("victim exists")
            .state = crate::VOL_STATE_DRAINING.to_string();
        cfg.data_volumes = Some(recs.clone());
        let bytes = serde_json::to_vec(&cfg).map_err(|e| {
            E::InvalidOperation(format!("failed to serialize the format config: {e}"))
        })?;
        crate::meta_backend::Metadata::setxattr(
            routed.as_ref(),
            1,
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            &bytes,
        )
        .await?;
        router.backend_router.set_volume_records(recs);
    }

    // Run the drain to completion in-process (§5.8), with progress.
    let job_id = fabric
        .submit(crate::jobs::JobSpec {
            job_type: crate::jobs::JobType::EvacuateVolume {
                volume_id: volume_id.to_string(),
            },
            throttle_pct,
        })
        .await?;
    println!("draining '{volume_id}' (job {job_id}, throttle {throttle_pct} %)...");
    let end = loop {
        match fabric
            .wait_terminal(&job_id, std::time::Duration::from_secs(10))
            .await
        {
            Ok(state) => break state,
            Err(_) => {
                if let Some(st) = fabric.status(&job_id).await? {
                    println!(
                        "  ...{}/{} tasks ({:?})",
                        st.tasks_done, st.tasks_total, st.state
                    );
                    if st.state == crate::jobs::JobState::PausedCapacity {
                        fabric.shutdown_abrupt().await;
                        return Err(E::InvalidOperation(
                            "drain self-paused (paused-capacity): survivors ran out of \
                             slack — free space and re-run `volume remove-data` (the job \
                             resumes by re-planning)"
                                .to_string(),
                        ));
                    }
                }
            }
        }
    };
    fabric.shutdown_abrupt().await;
    match end {
        crate::jobs::JobState::Completed => {
            println!("volume '{volume_id}' evacuated and retired.");
            Ok(())
        }
        other => Err(E::InvalidOperation(format!(
            "offline drain of '{volume_id}' ended {other:?} — re-run to resume (the plan \
             regenerates idempotently, KD-6)"
        ))),
    }
}

/// The shared durable tail of every offline lifecycle commit: guarded
/// open of the whole set (D0 claims — excludes racing mounts for the
/// commit's duration), **bit 3 on every member superblock first**, then
/// one setxattr tx on volume 0 with the updated records + `data_lv`
/// mirror (+ any `extra_xattrs` riders, e.g. the KD-12 durable Queued
/// rebalance record), then clean shutdown.
async fn commit_volume_records(
    meta_lvs: &[String],
    mut cfg: crate::FormatConfig,
    records: Vec<crate::DataVolumeRecord>,
    extra_xattrs: Vec<(String, Vec<u8>)>,
) -> Result<()> {
    // Routed open (PR VL5a): the config xattr on ino 1 routes to the
    // slot-0 host on stamped sets; legacy sets keep volume 0.
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;

    // Bit-before-durable-record (design-volume-lifecycle §7): stamp every
    // member superblock before the record exists. Sector 0 is never
    // rewritten by the open backends (checkpoints flip the root ledger),
    // so this is race-free under the held claims.
    for path in meta_lvs {
        if let Err(e) =
            crate::meta_backend::kv::superblock::set_volume_lifecycle_bit(Path::new(path)).await
        {
            for be in &routed.volumes {
                let _ = be.shutdown().await;
            }
            return Err(e.into());
        }
    }

    cfg.data_lv = Some(
        records
            .iter()
            .filter(|r| r.state != crate::VOL_STATE_RETIRED)
            .map(|r| r.backing_dev.clone())
            .collect(),
    );
    cfg.data_volumes = Some(records);
    let bytes = serde_json::to_vec(&cfg).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
    })?;
    let commit = async {
        crate::meta_backend::Metadata::setxattr(
            routed.as_ref(),
            1,
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            &bytes,
        )
        .await?;
        for (name, value) in &extra_xattrs {
            crate::meta_backend::Metadata::setxattr(routed.as_ref(), 1, name, value).await?;
        }
        Ok::<(), crate::error::SqueezefsError>(())
    }
    .await;
    for be in &routed.volumes {
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

/// `squeezefs volume repair-set` — the VL5a posture (design-volume-
/// lifecycle §5.5.1a): print the observed per-volume membership-stamp
/// state, then **refuse to auto-fix anything beyond re-stamping a
/// COHERENT observed state**:
///
/// - legacy set (no stamps anywhere): a no-op — never stamps, never sets
///   a bit (KD-14's untouched-sets law);
/// - every member stamped and mutually coherent (one `set_uuid`, one
///   `set_epoch`, one geometry, unique complete positions, every slot in
///   `[0, W)` hosted exactly once): idempotent re-stamp of exactly that
///   state — closes a torn-newest-ledger-slot fallback that surfaced an
///   older (but identical) stamp;
/// - exactly ONE stampless member beside an otherwise-coherent set whose
///   stamps leave exactly one position and one slot-complement free (the
///   kill-9 window repair-set itself can leave): the missing stamp is
///   inferable — write it (bit 2 barriered first, §5.5.1a ordering);
/// - **PR VL5b**: an EPOCH SPREAD over otherwise-coherent stamps (same
///   uuid/width/count, unique positions — the §5.5.2b slot-flip crash
///   windows) RESOLVES: per-slot highest-epoch-wins decides every dual
///   claim, and all members re-stamp at the max epoch with their
///   resolved hosted sets (converging writes 1/2 of an interrupted
///   flip; keyspace residue is cleaned by re-running the migration);
/// - anything else (foreign uuids, duplicate positions, count
///   disagreements — interrupted membership changes re-run their own
///   verb — multiple stampless members…): REFUSE loud with the
///   observed state.
///
/// Returns the paths whose ledger was re-stamped. Guarded like the other
/// offline lifecycle verbs: live clients refuse; each stamp write rides a
/// D0-guarded open + clean shutdown (the final checkpoint makes it
/// durable).
pub async fn repair_meta_set(meta_lvs: &[String]) -> Result<Vec<String>> {
    use crate::meta_backend::kv::checkpoint::{MembershipStamp, MEMBERSHIP_MAX_HOSTED_SLOTS};
    if meta_lvs.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "at least one metadata volume is required".to_string(),
        ));
    }
    // Live-client gate on EVERY volume before anything is touched.
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume repair-set refused: {e}"))
            })?;
    }

    let obs = crate::meta_backend::observe_meta_set(meta_lvs).await?;
    println!("observed membership state ({} volume(s)):", obs.len());
    for o in &obs {
        match &o.stamp {
            Some(st) => println!(
                "  {}: set {:02x?}.. epoch {} position {}/{} width {} slots {:?}",
                o.path,
                &st.set_uuid[..4],
                st.set_epoch,
                st.member_position,
                st.member_count,
                st.routing_width,
                st.slots_hosted
            ),
            None => println!("  {}: NO membership stamp", o.path),
        }
    }

    let stamped: Vec<&crate::meta_backend::MetaVolumeObservation> =
        obs.iter().filter(|o| o.stamp.is_some()).collect();
    if stamped.is_empty() {
        println!("legacy set (no stamps) — nothing to repair.");
        return Ok(Vec::new());
    }
    let refuse = |msg: String| {
        Err(SqueezefsError::InvalidOperation(format!(
            "volume repair-set refused: {msg} — VL5a re-stamps only a coherent observed \
             state (the §5.5.2b epoch resolution lands with the migration engine)"
        )))
    };

    // Coherence of the stamped subset (PR VL5b: EPOCHS may spread — the
    // §5.5.2b per-slot resolution below converges them; uuid, geometry
    // and membership count must agree).
    let first = stamped[0].stamp.as_ref().expect("filtered Some");
    for o in &stamped[1..] {
        let st = o.stamp.as_ref().expect("filtered Some");
        if st.set_uuid != first.set_uuid
            || st.member_count != first.member_count
            || st.routing_width != first.routing_width
        {
            return refuse(format!(
                "stamps on {} and {} disagree (uuid/count/geometry)",
                stamped[0].path, o.path
            ));
        }
    }
    let max_epoch = stamped
        .iter()
        .map(|o| o.stamp.as_ref().expect("filtered Some").set_epoch)
        .max()
        .unwrap_or(1);
    let member_count = usize::from(first.member_count);
    let width = first.routing_width as usize;
    if member_count != meta_lvs.len() {
        return refuse(format!(
            "stamps declare {member_count} members but the URI lists {}",
            meta_lvs.len()
        ));
    }

    // Positions and hosted slots of the stamped members (PR VL5b: a
    // dual claim across epochs resolves highest-epoch-wins; a SAME-epoch
    // dual claim stays a refusal — corruption by construction).
    let mut pos_holder: Vec<Option<&str>> = vec![None; member_count];
    let mut slot_claim: Vec<Option<(usize, u64)>> = vec![None; width];
    for (oi, o) in stamped.iter().enumerate() {
        let st = o.stamp.as_ref().expect("filtered Some");
        let pos = usize::from(st.member_position);
        if pos >= member_count {
            return refuse(format!(
                "{} stamps out-of-range position {pos} of {member_count}",
                o.path
            ));
        }
        if let Some(prev) = pos_holder[pos] {
            return refuse(format!("{prev} and {} both stamp position {pos}", o.path));
        }
        pos_holder[pos] = Some(&o.path);
        for &s in &st.slots_hosted {
            let s = usize::from(s);
            if s >= width {
                return refuse(format!("{} hosts slot {s} out of range", o.path));
            }
            match slot_claim[s] {
                None => slot_claim[s] = Some((oi, st.set_epoch)),
                Some((pi, pe)) if pe == st.set_epoch => {
                    return refuse(format!(
                        "{} and {} both host slot {s} at the SAME epoch {pe} — corruption",
                        stamped[pi].path, o.path
                    ));
                }
                Some((_, pe)) if st.set_epoch > pe => {
                    slot_claim[s] = Some((oi, st.set_epoch));
                }
                Some(_) => {}
            }
        }
    }
    let slot_hosted: Vec<bool> = slot_claim.iter().map(|c| c.is_some()).collect();

    let unstamped: Vec<&crate::meta_backend::MetaVolumeObservation> =
        obs.iter().filter(|o| o.stamp.is_none()).collect();
    let mut to_write: Vec<(String, MembershipStamp)> = Vec::new();
    match unstamped.len() {
        0 => {
            // Fully coherent (or an epoch-spread slot-flip state): every
            // slot must be hosted after resolution; re-stamp the
            // RESOLVED state at the max epoch (idempotent for coherent
            // sets; converges §5.5.2b writes 1/2 for interrupted flips —
            // keyspace residue is cleaned by re-running the migration).
            if let Some(missing) = slot_hosted.iter().position(|&h| !h) {
                return refuse(format!(
                    "slot {missing} of width {width} is hosted by no member"
                ));
            }
            for (oi, o) in stamped.iter().enumerate() {
                let mut st = o.stamp.clone().expect("filtered Some");
                st.native_slot = st.resolved_native_slot();
                st.set_epoch = max_epoch;
                st.slots_hosted
                    .retain(|&s| matches!(slot_claim[usize::from(s)], Some((ci, _)) if ci == oi));
                st.slot_cursors.retain(|(s, _)| st.slots_hosted.contains(s));
                to_write.push((o.path.clone(), st));
            }
        }
        1 => {
            // The kill-9 window: exactly one free position + the slot
            // complement makes the missing stamp inferable.
            let free_positions: Vec<usize> = pos_holder
                .iter()
                .enumerate()
                .filter(|(_, h)| h.is_none())
                .map(|(p, _)| p)
                .collect();
            let [pos] = free_positions[..] else {
                return refuse(format!(
                    "one stampless member ({}) but {} free positions — not inferable",
                    unstamped[0].path,
                    free_positions.len()
                ));
            };
            let missing_slots: Vec<u16> = slot_hosted
                .iter()
                .enumerate()
                .filter(|(_, &h)| !h)
                .map(|(s, _)| s as u16)
                .collect();
            if missing_slots.is_empty() || missing_slots.len() > MEMBERSHIP_MAX_HOSTED_SLOTS {
                return refuse(format!(
                    "the unhosted slot complement ({} slots) cannot belong to one member",
                    missing_slots.len()
                ));
            }
            let inferred = MembershipStamp {
                set_uuid: first.set_uuid,
                set_epoch: first.set_epoch,
                member_position: pos as u16,
                member_count: first.member_count,
                routing_width: first.routing_width,
                slots_hosted: missing_slots,
                // VL5a-shaped inference (native = position; no guests).
                native_slot: None,
                slot_cursors: Vec::new(),
            };
            println!(
                "inferring the missing stamp for {}: position {pos}, slots {:?}",
                unstamped[0].path, inferred.slots_hosted
            );
            for o in &stamped {
                to_write.push((o.path.clone(), o.stamp.clone().expect("stamped")));
            }
            to_write.push((unstamped[0].path.clone(), inferred));
        }
        n => {
            return refuse(format!(
                "{n} stampless members beside a stamped set — positions are not inferable"
            ));
        }
    }

    // Execute: bit 2 barriered durably FIRST on every volume that will
    // carry a stamp (§5.5.1a ordering invariant — a crash here leaves
    // bit-set stampless volumes this very verb repairs on re-run), then
    // each stamp rides a guarded open's shutdown checkpoint.
    for (path, _) in &to_write {
        crate::meta_backend::kv::superblock::set_guest_slots_bit(Path::new(path)).await?;
    }
    let mut restamped = Vec::with_capacity(to_write.len());
    for (path, stamp) in to_write {
        let be = crate::meta_backend::kv::backend::KvMetaBackend::open(Path::new(&path)).await?;
        be.set_membership_stamp(stamp);
        be.shutdown().await?;
        restamped.push(path);
    }
    println!("re-stamped {} member volume(s).", restamped.len());
    Ok(restamped)
}

// ---------------------------------------------------------------------------
// PR VL5b — the KD-8 staging drain barrier + offline meta membership verbs
// (design-volume-lifecycle §5.5.2 add/remove flows, §5.5.3, §5.2 meta side).
// ---------------------------------------------------------------------------

/// How `volume add-meta` picks the slots the new member takes.
#[derive(Debug, Clone)]
pub enum TakeSlots {
    /// The k most-loaded eligible slots (record-count census).
    Count(u32),
    /// An explicit slot list.
    List(Vec<u16>),
}

/// The KD-8 staging drain barrier over ISOLATED staging roots (the
/// per-mount dirs carrying a generation marker + `staging_segment/`):
/// verify every root carries NO live staged write custody (per-unit
/// diagnostics on refusal — R7: loud, abortable, never a drop), then
/// RESTAMP the old-generation roots with the new generation so the
/// discard-on-mismatch law is a provable no-op. Roots bound to a
/// FOREIGN generation are left untouched — their content was condemned
/// before this barrier and the next mount's discard loses nothing.
pub async fn staging_drain_barrier(
    dirs: &[PathBuf],
    old_generation: &str,
    new_generation: &str,
) -> Result<()> {
    staging_rebind_prepare(dirs, old_generation, new_generation).await?;
    // Phase 2: restamp the roots bound to the OLD generation.
    for dir in dirs {
        match crate::cache::read_staging_generation_marker(dir).await? {
            Some(g) if g == old_generation || g == new_generation => {
                crate::cache::write_staging_generation_marker(dir, new_generation).await?;
            }
            _ => {
                log::info!(
                    "staging drain barrier: {} carries a foreign/missing generation \
                     binding — left untouched (the next mount's discard law owns it)",
                    dir.display()
                );
            }
        }
    }
    crate::fuse_client::METRICS
        .staging_drain_barriers
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// KD-8 phase 1 (run BEFORE any membership stamp flips): refuse loud on
/// PENDING write custody (`active_block:` / `active_block_ext:` records
/// — acked writes a crash left un-uploaded; their recovery semantics
/// must not straddle a set change: mount + sync + clean unmount drains
/// them), then write the TWO-PHASE dual marker on every old-generation
/// root so durable staged-layout payloads REBIND instead of being
/// discarded — every crash prefix leaves the root adoptable by
/// whichever set is mountable.
pub async fn staging_rebind_prepare(
    dirs: &[PathBuf],
    old_generation: &str,
    new_generation: &str,
) -> Result<()> {
    let mut custody: Vec<String> = Vec::new();
    for dir in dirs {
        for key in crate::cache::scan_live_staged_custody(dir, 64).await? {
            // Durable staged-layout payloads (uuid file ids) rebind;
            // pending block custody refuses.
            if key.starts_with("active_block:") || key.starts_with("active_block_ext:") {
                custody.push(format!("{}: {key}", dir.display()));
            }
        }
    }
    if !custody.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "staging drain barrier refused (KD-8): {} pending write-custody unit(s) \
             remain — mount the filesystem, let writeback drain (sync + clean unmount), \
             then re-run; nothing was changed. Units: {:?}",
            custody.len(),
            custody
        )));
    }
    for dir in dirs {
        match crate::cache::read_staging_generation_marker(dir).await? {
            Some(g) if g == old_generation || g == new_generation => {
                crate::cache::write_staging_generation_prepare_marker(
                    dir,
                    old_generation,
                    new_generation,
                )
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// KD-8 phase 2 (run AFTER the membership flip completed): finalize
/// every dual-marked root to the single new-generation marker.
pub async fn staging_rebind_finalize(dirs: &[PathBuf], new_generation: &str) -> Result<()> {
    for dir in dirs {
        // read_staging_generation_marker reports the FIRST bound
        // generation; the dual marker's membership check is inside the
        // writer path — finalize unconditionally on dual/old bindings.
        if crate::cache::read_staging_generation_marker(dir)
            .await?
            .is_some()
        {
            crate::cache::write_staging_generation_marker(dir, new_generation).await?;
        }
    }
    crate::fuse_client::METRICS
        .staging_drain_barriers
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Expand the format-config staging paths into the ISOLATED per-mount
/// staging roots the daemon actually populates
/// (`<config-dir>/<fs-name>/<sanitized-mountpoint>/` — the mount wiring
/// in `src/main.rs`; the shared `cache_segment/` sibling is read cache,
/// not custody, and is skipped).
fn staging_isolated_roots(cfg: Option<&crate::FormatConfig>) -> Vec<PathBuf> {
    let Some(cfg) = cfg else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in cfg.disk_cache_paths.clone().unwrap_or_default() {
        let base = dir.join(&cfg.name);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            if entry.file_name() == "cache_segment" {
                continue; // shared read cache — discardable by design
            }
            out.push(entry.path());
        }
    }
    out
}

/// One member's observation for the membership verbs: path + stamp.
struct MemberObs {
    path: String,
    stamp: crate::meta_backend::kv::checkpoint::MembershipStamp,
}

/// Observe an all-stamped coherent-uuid set (membership verbs tolerate
/// epoch/count spread — THEY are the §5.5.2b re-run that converges it).
async fn observe_stamped_members(meta_lvs: &[String]) -> Result<Vec<MemberObs>> {
    let obs = crate::meta_backend::observe_meta_set(meta_lvs).await?;
    let mut out = Vec::with_capacity(obs.len());
    for o in obs {
        let Some(stamp) = o.stamp else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "metadata volume {} carries no §5.5.1a membership stamp — meta membership \
                 changes need a `format --meta-slots` set (a legacy set's routing width \
                 equals its volume count: every slot is its host's only slot, so there is \
                 nothing a new member could take — the W-granularity law, \
                 design-volume-lifecycle §5.5.1/operations.md)",
                o.path
            )));
        };
        out.push(MemberObs {
            path: o.path,
            stamp,
        });
    }
    let first_uuid = out[0].stamp.set_uuid;
    let first_width = out[0].stamp.routing_width;
    for m in &out[1..] {
        if m.stamp.set_uuid != first_uuid || m.stamp.routing_width != first_width {
            return Err(SqueezefsError::InvalidOperation(format!(
                "metadata volumes {} and {} disagree on set identity/width — not one set",
                out[0].path, m.path
            )));
        }
    }
    Ok(out)
}

/// Record-count census of one slot's keyspace on its host (the
/// "most-loaded" ranking for `--take-slots k`).
async fn slot_record_count(path: &str, slot: u16, native: Option<u16>) -> Result<u64> {
    let be = crate::meta_backend::kv::backend::KvMetaBackend::open_probe(Path::new(path)).await?;
    let ks = crate::meta_backend::slot_migration::SlotKeyspace::of(slot, native);
    let mut n = 0u64;
    crate::meta_backend::slot_migration::scan_slot_keyspace(&be, &ks, |_, _, _| {
        n += 1;
        Ok(())
    })
    .await?;
    Ok(n)
}

/// `squeezefs volume add-meta` — the OFFLINE D0-guarded coordinator
/// (design-volume-lifecycle §5.5.2 Add, the VL4 `remove_data_volume_
/// offline` posture): KD-8 staging barrier → format the new member →
/// bulk-copy the taken slots into its guest keyspaces (write 0) → the
/// new member's claim stamp @E (target-first) → every old member's
/// stamp @E → source-keyspace teardown → generation restamp + config
/// mirror. Idempotent: a crash anywhere re-runs to convergence (counts
/// disagree ⇒ mounts refuse loud until then). Returns the taken slots.
pub async fn add_meta_volume(
    meta_lvs: &[String],
    device: &str,
    take: &TakeSlots,
) -> Result<Vec<u16>> {
    use crate::meta_backend::kv::backend::KvMetaBackend;
    use crate::meta_backend::slot_migration::{
        bulk_copy_slot, teardown_slot_keyspace, SlotKeyspace,
    };
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume add-meta refused: {e}"))
            })?;
    }
    if meta_lvs.iter().any(|p| canon(p) == canon(device)) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "device {device} is already a member of the metadata set"
        )));
    }
    let members = observe_stamped_members(meta_lvs).await?;
    let width = members[0].stamp.routing_width;
    let old_count = members.len() as u16;
    // Config-less sets (library/test harnesses format volumes without
    // the bootstrap xattr) simply have no staging dirs to barrier.
    let cfg = read_volume_format_config(meta_lvs).await.ok();

    // KD-8 phase 1 BEFORE anything flips: refuse pending write custody,
    // dual-mark the isolated staging roots so durable staged payloads
    // REBIND across the membership change (never a discard window).
    let staging_dirs = staging_isolated_roots(cfg.as_ref());
    let old_gen = crate::meta_backend::volume_set_generation(meta_lvs).await?;

    // Resume detection: is the device already a stamped member of THIS
    // set (a crashed prior attempt)?
    let dev_stamp =
        match crate::meta_backend::kv::superblock::classify_volume(Path::new(device)).await {
            Ok(crate::meta_backend::kv::superblock::VolumeFormat::V3(sb)) => {
                crate::meta_backend::kv::checkpoint::read_newest_ledger(
                    Path::new(device),
                    sb.root_ledger.start,
                )
                .await?
                .and_then(|r| r.membership_stamp)
                .filter(|st| st.set_uuid == members[0].stamp.set_uuid)
            }
            _ => None,
        };
    let (epoch, new_position, resume) = match &dev_stamp {
        Some(st) => (st.set_epoch, st.member_position, true),
        None => (
            members.iter().map(|m| m.stamp.set_epoch).max().unwrap_or(1) + 1,
            members
                .iter()
                .map(|m| m.stamp.member_position)
                .max()
                .unwrap_or(0)
                + 1,
            false,
        ),
    };

    // Slot selection: explicit list, resumed claim, or the k-most-loaded
    // census — a source may never lose its LAST slot.
    let hosted_by = |m: &MemberObs| m.stamp.slots_hosted.clone();
    let taken: Vec<u16> = match (&dev_stamp, take) {
        (Some(st), _) if !st.slots_hosted.is_empty() => st.slots_hosted.clone(),
        (_, TakeSlots::List(slots)) => {
            let mut slots = slots.clone();
            slots.sort_unstable();
            slots.dedup();
            for &s in &slots {
                if u32::from(s) >= width {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "slot {s} outside the frozen routing width {width}"
                    )));
                }
            }
            for m in &members {
                let keeps = hosted_by(m).iter().filter(|s| !slots.contains(s)).count();
                if keeps == 0 && !hosted_by(m).is_empty() {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "taking {slots:?} would leave {} hosting nothing — every member \
                         keeps at least one slot (use `volume remove-meta` to retire a \
                         member)",
                        m.path
                    )));
                }
            }
            slots
        }
        (_, TakeSlots::Count(k)) => {
            if *k == 0 {
                return Err(SqueezefsError::InvalidOperation(
                    "--take-slots 0 takes nothing — an added meta volume is only useful \
                     WITH slot migration (design-volume-lifecycle §5.5.2)"
                        .to_string(),
                ));
            }
            // Candidates: every slot whose host keeps another one.
            let mut cands: Vec<(u64, u16)> = Vec::new();
            for m in &members {
                let hosted = hosted_by(m);
                if hosted.len() < 2 {
                    continue;
                }
                // A member may lose all but one slot.
                let native = m.stamp.resolved_native_slot();
                let mut loads = Vec::new();
                for &s in &hosted {
                    loads.push((slot_record_count(&m.path, s, native).await?, s));
                }
                loads.sort_by(|a, b| b.0.cmp(&a.0));
                loads.pop(); // the host keeps its least-loaded slot
                cands.extend(loads);
            }
            cands.sort_by(|a, b| b.0.cmp(&a.0));
            if (cands.len() as u32) < *k {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "--take-slots {k}: only {} slot(s) are takeable (every member keeps \
                     at least one) — the W-granularity limit (operations.md)",
                    cands.len()
                )));
            }
            cands.truncate(*k as usize);
            cands.into_iter().map(|(_, s)| s).collect()
        }
    };

    // Format the new member (fresh runs only): a HOSTLESS transient
    // stamp at epoch E — the claim (slots + cursors) lands after the
    // copy, per the §5.5.2b write-0-before-claim order.
    let node_size = {
        match crate::meta_backend::kv::superblock::classify_volume(Path::new(&members[0].path))
            .await?
        {
            crate::meta_backend::kv::superblock::VolumeFormat::V3(sb) => sb.node_size as usize,
            _ => unreachable!("observed members are v3"),
        }
    };
    if !resume {
        let volume_len = crate::nvme_dev::device_capacity_bytes(device)
            .or_else(|_| std::fs::metadata(device).map(|m| m.len()))
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("cannot size device {device}: {e}"))
            })?;
        let opts = crate::meta_backend::kv::builder::FormatV3Options {
            node_size,
            journal_len_override: None,
            force: false, // a formatted device refuses — never silently destroy
            full_wipe: false,
            format_config_xattr: None,
        };
        crate::meta_backend::kv::builder::format_v3_stamped(
            Path::new(device),
            volume_len,
            &opts,
            crate::meta_backend::kv::checkpoint::MembershipStamp {
                set_uuid: members[0].stamp.set_uuid,
                set_epoch: epoch,
                member_position: new_position,
                member_count: old_count + 1,
                routing_width: width,
                slots_hosted: Vec::new(),
                native_slot: None,
                slot_cursors: Vec::new(),
            },
        )
        .await?;
    }

    // KD-8 phase 1 (the §5.5.2b write-0 class: durable, flips nothing):
    // the new set's generation is derivable now that the member is
    // formatted — dual-mark the staging roots.
    let new_gen = {
        let mut uris: Vec<String> = meta_lvs.to_vec();
        uris.push(device.to_string());
        crate::meta_backend::volume_set_generation(&uris)
            .await
            .unwrap_or_else(|_| old_gen.clone())
    };
    staging_rebind_prepare(&staging_dirs, &old_gen, &new_gen).await?;

    // Bit 4 on every participant BEFORE any extended stamp/guest record.
    for path in meta_lvs.iter().map(String::as_str).chain([device]) {
        crate::meta_backend::kv::superblock::set_slot_migration_bit(Path::new(path)).await?;
    }

    // Open the whole working set with D0 claims: sources + the target.
    let target = KvMetaBackend::open(Path::new(device)).await?;
    let mut sources: Vec<(String, std::sync::Arc<KvMetaBackend>)> = Vec::new();
    for m in &members {
        let be = KvMetaBackend::open(Path::new(&m.path)).await?;
        sources.push((m.path.clone(), be));
    }
    let body = async {
        // Record keys embed the seeded hashes: the whole working set
        // must share one seed (VL5b set-wide-seed formats).
        for (_, be) in &sources {
            crate::meta_backend::slot_migration::check_hash_seed_uniform(be, &target)?;
        }
        // Copy each taken slot (source = the member whose stamp claims
        // it at the highest epoch; a released source skips the copy).
        let mut cursors: Vec<(u16, u64)> = Vec::new();
        for &slot in &taken {
            let src = sources
                .iter()
                .filter_map(|(p, be)| {
                    be.membership_stamp()
                        .filter(|st| st.slots_hosted.contains(&slot))
                        .map(|st| (p.clone(), be.clone(), st))
                })
                .max_by_key(|(_, _, st)| st.set_epoch);
            let Some((_, src_be, src_stamp)) = src else {
                // Resume path: the source already released — the copy +
                // claim landed durably before the crash.
                let cur = target.guest_cursor_snapshot(slot).unwrap_or(2);
                cursors.push((slot, cur));
                continue;
            };
            let src_ks = SlotKeyspace::of(slot, src_stamp.resolved_native_slot());
            let dst_ks = SlotKeyspace::of(slot, None);
            teardown_slot_keyspace(&target, &dst_ks).await?; // re-run wipe
            bulk_copy_slot(&src_be, &src_ks, &target, &dst_ks).await?;
            let cur = if src_ks.legacy {
                src_be.next_ino()
            } else {
                src_be.guest_cursor_snapshot(slot).unwrap_or(2)
            };
            target.install_guest_cursor(slot, cur);
            cursors.push((slot, cur));
        }

        // Write 1 (target-first): the new member's claim @E, durable.
        let mut claim = target.membership_stamp().ok_or_else(|| {
            SqueezefsError::InvalidOperation("new member lost its stamp".to_string())
        })?;
        claim.set_epoch = epoch;
        claim.member_count = old_count + 1;
        claim.member_position = new_position;
        claim.slots_hosted = taken.clone();
        claim.native_slot = None;
        claim.slot_cursors = cursors;
        target.set_membership_stamp(claim);
        target
            .checkpoint_now()
            .await
            .map_err(|e| SqueezefsError::InvalidOperation(format!("claim write failed: {e}")))?;

        // Write 2..n: every OLD member's stamp @E (count n+1, minus the
        // slots it lost).
        for (_, be) in &sources {
            let Some(mut st) = be.membership_stamp() else {
                continue;
            };
            st.native_slot = st.resolved_native_slot();
            st.set_epoch = epoch;
            st.member_count = old_count + 1;
            st.slots_hosted.retain(|s| !taken.contains(s));
            st.slot_cursors.retain(|(s, _)| !taken.contains(s));
            for s in &taken {
                be.remove_guest_cursor(*s);
            }
            be.set_membership_stamp(st);
            be.checkpoint_now().await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("member re-stamp failed: {e}"))
            })?;
        }

        // Teardown: the sources' now-guest-hosted keyspaces.
        for &slot in &taken {
            for (_, be) in &sources {
                let Some(st) = be.membership_stamp() else {
                    continue;
                };
                let ks = SlotKeyspace::of(slot, st.resolved_native_slot());
                teardown_slot_keyspace(be, &ks).await?;
            }
        }
        Ok::<(), SqueezefsError>(())
    }
    .await;
    // Release every claim on all paths.
    for (_, be) in &sources {
        if let Err(e) = be.shutdown().await {
            log::warn!("releasing guard after add-meta: {e}");
        }
    }
    if let Err(e) = target.shutdown().await {
        log::warn!("releasing target guard after add-meta: {e}");
    }
    body?;

    // KD-8 phase 2 + mirror: finalize the staging rebind to the new
    // generation; the FormatConfig mirror records the new membership.
    let mut new_uris: Vec<String> = meta_lvs.to_vec();
    new_uris.push(device.to_string());
    let new_gen = crate::meta_backend::volume_set_generation(&new_uris).await?;
    staging_rebind_finalize(&staging_dirs, &new_gen).await?;
    update_meta_config_mirror(&new_uris).await?;
    Ok(taken)
}

/// `squeezefs volume remove-meta` — the OFFLINE D0-guarded coordinator:
/// §5.2 meta-side capacity preflight → KD-8 barrier → migrate every
/// victim-hosted slot to survivors → SURVIVORS-FIRST stamps @E →
/// the victim's retirement TOMBSTONE last (§5.5.2b epoch completeness)
/// → generation restamp + config mirror. `victim` is the member's
/// device path as listed in the URI.
pub async fn remove_meta_volume(meta_lvs: &[String], victim: &str) -> Result<()> {
    use crate::meta_backend::kv::backend::KvMetaBackend;
    use crate::meta_backend::slot_migration::{
        bulk_copy_slot, teardown_slot_keyspace, SlotKeyspace,
    };
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume remove-meta refused: {e}"))
            })?;
    }
    let members = observe_stamped_members(meta_lvs).await?;
    if members.len() < 2 {
        return Err(SqueezefsError::InvalidOperation(
            "cannot remove the last metadata volume".to_string(),
        ));
    }
    let victim_idx = members
        .iter()
        .position(|m| canon(&m.path) == canon(victim))
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "{victim} is not a member of the metadata set"
            ))
        })?;
    let cfg = read_volume_format_config(meta_lvs).await.ok();
    let staging_dirs = staging_isolated_roots(cfg.as_ref());
    let old_gen = crate::meta_backend::volume_set_generation(meta_lvs).await?;
    let survivors_paths: Vec<String> = members
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != victim_idx)
        .map(|(_, m)| m.path.clone())
        .collect();
    let old_count = members.len() as u16;
    let epoch = members.iter().map(|m| m.stamp.set_epoch).max().unwrap_or(1) + 1;
    let victim_slots = members[victim_idx].stamp.slots_hosted.clone();
    let victim_native = members[victim_idx].stamp.resolved_native_slot();

    // §5.2 meta-side capacity preflight (honest refusal with numbers):
    // victim used extents must fit the survivors' free extents minus the
    // checkpoint headroom (clamped to a quarter of the smallest
    // survivor's heap — the working set can never exceed the volume).
    {
        let victim_be =
            crate::meta_backend::kv::backend::KvMetaBackend::open_probe(Path::new(victim)).await?;
        let victim_used = victim_be
            .superblock()
            .total_extents()
            .saturating_sub(victim_be.free_extents());
        let node_size = u64::from(victim_be.superblock().node_size);
        drop(victim_be);
        let mut avail = 0u64;
        let mut headroom = 0u64;
        for p in &survivors_paths {
            let be = KvMetaBackend::open_probe(Path::new(p)).await?;
            avail = avail.saturating_add(be.free_extents());
            let max_dirty = std::env::var("SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(4096);
            headroom = headroom.max((2 * max_dirty).min(be.superblock().total_extents() / 4));
        }
        if avail < victim_used.saturating_add(headroom) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume remove-meta preflight refused (§5.2 meta side): survivors' free \
                 extents {avail} < victim used {victim_used} + checkpoint headroom \
                 {headroom} (extents of {node_size} B) — grow the survivors first; \
                 nothing was changed"
            )));
        }
    }

    // KD-8 phase 1: refuse pending write custody + dual-mark the roots
    // (durable staged payloads rebind across the change).
    let new_gen_planned = crate::meta_backend::volume_set_generation(&survivors_paths)
        .await
        .unwrap_or_else(|_| old_gen.clone());
    staging_rebind_prepare(&staging_dirs, &old_gen, &new_gen_planned).await?;

    // Bit 4 everywhere (extended stamps + guest records ahead).
    for m in &members {
        crate::meta_backend::kv::superblock::set_slot_migration_bit(Path::new(&m.path)).await?;
    }

    // Open the working set (D0 claims).
    let victim_be = KvMetaBackend::open(Path::new(victim)).await?;
    let mut survivors: Vec<(String, std::sync::Arc<KvMetaBackend>)> = Vec::new();
    for p in &survivors_paths {
        survivors.push((p.clone(), KvMetaBackend::open(Path::new(p)).await?));
    }
    let body = async {
        // Migrate every victim-hosted slot to the emptiest survivor.
        let mut placed: Vec<(u16, usize, u64)> = Vec::new(); // (slot, survivor idx, cursor)
        for &slot in &victim_slots {
            let (ti, target) = survivors
                .iter()
                .enumerate()
                .filter(|(_, (_, be))| {
                    // v1: never migrate a slot back onto its origin's
                    // legacy keyspace.
                    be.membership_stamp()
                        .and_then(|st| st.resolved_native_slot())
                        != Some(slot)
                })
                .max_by_key(|(_, (_, be))| be.free_extents())
                .map(|(i, (_, be))| (i, be.clone()))
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation(format!(
                        "no survivor can host slot {slot} (v1 refuses migrating a slot \
                         back onto its origin volume)"
                    ))
                })?;
            crate::meta_backend::slot_migration::check_hash_seed_uniform(&victim_be, &target)?;
            let src_ks = SlotKeyspace::of(slot, victim_native);
            let dst_ks = SlotKeyspace::of(slot, None);
            teardown_slot_keyspace(&target, &dst_ks).await?; // re-run wipe
            bulk_copy_slot(&victim_be, &src_ks, &target, &dst_ks).await?;
            let cur = if src_ks.legacy {
                victim_be.next_ino()
            } else {
                victim_be.guest_cursor_snapshot(slot).unwrap_or(2)
            };
            target.install_guest_cursor(slot, cur);
            placed.push((slot, ti, cur));
        }

        // SURVIVORS-FIRST stamps @E (count n−1, positions kept, plus the
        // slots they gained) — the victim's disappearance is only ever
        // expressed after the surviving set is fully self-describing.
        for (i, (_, be)) in survivors.iter().enumerate() {
            let Some(mut st) = be.membership_stamp() else {
                continue;
            };
            st.native_slot = st.resolved_native_slot();
            st.set_epoch = epoch;
            st.member_count = old_count - 1;
            for (slot, ti, cur) in &placed {
                if *ti == i {
                    if !st.slots_hosted.contains(slot) {
                        st.slots_hosted.push(*slot);
                        st.slots_hosted.sort_unstable();
                    }
                    st.slot_cursors.retain(|(s, _)| s != slot);
                    st.slot_cursors.push((*slot, *cur));
                }
            }
            st.slot_cursors.sort_unstable_by_key(|(s, _)| *s);
            be.set_membership_stamp(st);
            be.checkpoint_now().await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("survivor re-stamp failed: {e}"))
            })?;
        }

        // VICTIM-LAST: the retirement tombstone (member_count = 0 —
        // discovery refuses it loud by name if ever listed again).
        let mut tomb = members[victim_idx].stamp.clone();
        tomb.set_epoch = epoch;
        tomb.member_count = 0;
        tomb.slots_hosted = Vec::new();
        tomb.slot_cursors = Vec::new();
        tomb.native_slot = victim_native;
        victim_be.set_membership_stamp(tomb);
        victim_be.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!("victim tombstone failed: {e}"))
        })?;
        Ok::<(), SqueezefsError>(())
    }
    .await;
    for (_, be) in &survivors {
        if let Err(e) = be.shutdown().await {
            log::warn!("releasing survivor guard after remove-meta: {e}");
        }
    }
    if let Err(e) = victim_be.shutdown().await {
        log::warn!("releasing victim guard after remove-meta: {e}");
    }
    body?;

    // KD-8 phase 2 + mirror on the survivor set.
    let new_gen = crate::meta_backend::volume_set_generation(&survivors_paths).await?;
    staging_rebind_finalize(&staging_dirs, &new_gen).await?;
    update_meta_config_mirror(&survivors_paths).await?;
    Ok(())
}

/// Rewrite the informational FormatConfig meta mirror (width / slot map
/// / member records) from the authoritative stamps — shared tail of the
/// membership verbs. Best-effort mirror content, but committed through
/// the guarded routed open (the config xattr is real state).
async fn update_meta_config_mirror(meta_lvs: &[String]) -> Result<()> {
    use crate::meta_backend::Metadata;
    let disc = crate::meta_backend::discover_meta_set(meta_lvs).await?;
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let out = async {
        let Some(bytes) = routed
            .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
            .await?
        else {
            return Ok(());
        };
        let Ok(mut cfg) = serde_json::from_slice::<crate::FormatConfig>(&bytes) else {
            return Ok(());
        };
        cfg.meta_routing_width = Some(disc.routing_width as u32);
        let positions: Vec<u16> = {
            let obs = crate::meta_backend::observe_meta_set(&disc.ordered_paths).await?;
            obs.iter()
                .map(|o| o.stamp.as_ref().map(|s| s.member_position).unwrap_or(0))
                .collect()
        };
        cfg.meta_slot_map = Some(disc.slot_to_volume.iter().map(|&v| positions[v]).collect());
        cfg.meta_volumes = Some(
            disc.ordered_paths
                .iter()
                .zip(&positions)
                .map(|(p, &pos)| crate::MetaVolumeRecord {
                    id: format!("meta-pos-{pos}"),
                    backing_dev: p.clone(),
                    member_position: pos,
                    added_ts: std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                })
                .collect(),
        );
        let bytes = serde_json::to_vec(&cfg).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("config mirror serialize: {e}"))
        })?;
        routed
            .setxattr(
                1,
                crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
                &bytes,
            )
            .await?;
        Ok::<(), SqueezefsError>(())
    }
    .await;
    for be in &routed.volumes {
        if let Err(e) = be.shutdown().await {
            log::warn!("releasing guard after the meta config mirror update: {e}");
        }
    }
    out
}
