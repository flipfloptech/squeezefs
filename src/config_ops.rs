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
/// Format-verb concurrent volume-format pool width, pure form
/// (tie-tested in the derivation sweep; moved out of `main.rs`'s format
/// arm as KD-MW-14 rung 3c): `cpus × 2` — format tasks are I/O-parked
/// device work, so the permit pool oversubscribes the (fleet-share-
/// DIVIDED) core root ×2; floor 2 keeps a degenerate root formatting a
/// meta+data pair concurrently.
pub fn format_pool_permits(cpus: usize) -> usize {
    cpus.saturating_mul(2).max(2)
}

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

/// System roots the staging stamp refuses to wipe under ANY flag (the
/// ENG-4 staging-wipe guard): `stamp_staging_dir` runs `remove_dir_all`
/// on an operator-supplied path — usually as root — so one mistyped
/// `--disk-cache-paths` must never become a recursive delete of a system
/// tree. A supplied path is hard-refused (consent never overrides) when,
/// after normalization AND symlink resolution, it equals one of these
/// entries or is a path-prefix of one (`/` therefore guards them all).
/// Subdirectories BELOW an entry (e.g. `/var/cache/squeezefs`) stay
/// legal — those are exactly where dedicated staging roots live; the
/// marker/consent precondition still applies to them.
pub const STAGING_WIPE_DENYLIST: &[&str] =
    &["/", "/home", "/etc", "/usr", "/var", "/boot", "/root"];

/// The per-mount isolation container mounts create under a staging root
/// (`<root>/squeezefs/<sanitized-mountpoint>/` — see the mount-side
/// staging isolation in `fuse_client.rs`). Recognition input for
/// [`staging_wipe_precheck`].
const STAGING_ISOLATION_CONTAINER: &str = "squeezefs";

/// VAL-7b (pre-RC spec §3): the mode of every staging / read-cache
/// **directory**. Owner-only.
///
/// These trees hold the segment rings — on a passthrough (untransformed)
/// volume that is literal **plaintext user file data**, plus the staged
/// object key names in every segment header. They were created with the
/// process umask (0755 in practice), so any local user could enumerate
/// and — with the 0644 segment files — read every staged byte of every
/// tenant.
pub const STAGING_DIR_MODE: u32 = 0o700;

/// VAL-7b: the mode of every staging / read-cache **segment file**.
pub const STAGING_FILE_MODE: u32 = 0o600;

/// Create `dir` (and any missing parents) and assert
/// [`STAGING_DIR_MODE`] on the leaf — the ONE policy point for staging
/// and read-cache directory creation (VAL-7b).
///
/// The leaf mode is set with an explicit `fchmod` rather than
/// `DirBuilder::mode`, for two reasons: `create_dir_all` applies the mode
/// to every intermediate it creates (an operator-supplied
/// `/mnt/nvme/staging` must not silently turn `/mnt/nvme` into a 0700
/// dir), and an ALREADY-EXISTING permissive dir — the common upgrade
/// case — has to be tightened too.
///
/// **`O_NOFOLLOW` is deliberately NOT used here.** The mount-side staging
/// layout makes `<isolated-dir>/cache_segment` a **symlink** to
/// `../cache_segment` on purpose — the read cache is shared across mounts
/// of one staging root (`src/main.rs`'s isolation container) — and
/// `O_DIRECTORY | O_NOFOLLOW` on a symlink is `ENOTDIR`, which would fail
/// every mount that has a staging root (caught by
/// `tests/cache_path_policy_tests.rs`, pinned by
/// `tests/val7_access_control_tests.rs`). These paths are
/// daemon-constructed inside a root the daemon already owns; the
/// symlink-swap surface that genuinely needs `O_NOFOLLOW` is the
/// operator-supplied root the format-grade stamp chowns as root
/// (`stamp_precleared_staging_dir`, which does use it).
pub fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let d = open_dir(dir, false)?;
    fchmod(&d, STAGING_DIR_MODE)
}

/// `open(dir, O_DIRECTORY | O_CLOEXEC [| O_NOFOLLOW])` — the anchor for
/// every `fchmod`/`fchown` on a staging root (VAL-7b: the historical
/// path-based `chown` followed a symlink planted between the create and
/// the chown).
fn open_dir(dir: &Path, nofollow: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = libc::O_DIRECTORY | libc::O_CLOEXEC;
    if nofollow {
        flags |= libc::O_NOFOLLOW;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(dir)
}

fn fchmod(f: &std::fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `f` is a live owned fd for the duration of the call;
    // fchmod touches no Rust-managed memory.
    if unsafe { libc::fchmod(f.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn fchown(f: &std::fs::File, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: as `fchmod` above.
    if unsafe { libc::fchown(f.as_raw_fd(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Absolute, lexically normalized form of `dir` (trailing slashes and
/// `.` segments dropped; relative paths anchored at the cwd). No symlink
/// resolution — [`denylist_refusal`] additionally checks the
/// canonicalized form when the path exists.
fn absolute_lexical(dir: &Path) -> PathBuf {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(dir))
            .unwrap_or_else(|_| dir.to_path_buf())
    };
    abs.components().collect()
}

/// [`STAGING_WIPE_DENYLIST`] check: `Some(refusal)` when `dir` (lexical
/// or canonical form) is a protected system path. Runs BEFORE any
/// filesystem mutation and is consent-independent.
fn denylist_refusal(dir: &Path) -> Option<SqueezefsError> {
    let mut candidates = vec![absolute_lexical(dir)];
    if let Ok(canon) = std::fs::canonicalize(dir) {
        if !candidates.contains(&canon) {
            candidates.push(canon);
        }
    }
    for cand in &candidates {
        for entry in STAGING_WIPE_DENYLIST {
            let e = Path::new(entry);
            if cand.as_path() == e || e.starts_with(cand) {
                return Some(SqueezefsError::InvalidOperation(format!(
                    "refusing to wipe '{}': it is a protected system path ('{}' matches the \
                     denylist {}); staging/cache directories must be dedicated \
                     subdirectories, never system roots",
                    dir.display(),
                    cand.display(),
                    STAGING_WIPE_DENYLIST.join(", "),
                )));
            }
        }
    }
    None
}

/// Does `dir` look like a previously used squeezefs staging root? True
/// iff its ONLY top-level entry is the `squeezefs/` isolation container
/// and at least one per-mount dir below it carries the staging
/// generation marker. Foreign top-level content beside the container
/// de-recognizes the root: those bytes are not ours to wipe without
/// consent.
fn is_recognized_staging_root(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut saw_container = false;
    for ent in rd.flatten() {
        if ent.file_name() == std::ffi::OsStr::new(STAGING_ISOLATION_CONTAINER) {
            saw_container = true;
        } else {
            return false;
        }
    }
    if !saw_container {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir.join(STAGING_ISOLATION_CONTAINER)) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.path()
            .join(crate::cache::nvme::STAGING_GENERATION_MARKER)
            .is_file()
    })
}

/// What a sanctioned wipe of one staging root is about to delete —
/// printed BEFORE acting (the ENG-4 deletion plan).
struct StagingWipePlan {
    total: usize,
    preview: Vec<String>,
    recognized: bool,
}

/// The ENG-4 wipe guard, pure of side effects: denylist hard-refusal,
/// then the marker/consent precondition. `Ok(None)` = nothing to delete
/// (missing or empty dir); `Ok(Some(plan))` = the wipe is sanctioned
/// (recognized staging root, or explicit consent) and `plan` is what
/// will be removed.
fn staging_wipe_precheck(dir: &Path, wipe_consent: bool) -> Result<Option<StagingWipePlan>> {
    if let Some(refusal) = denylist_refusal(dir) {
        return Err(refusal);
    }
    let entries: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(SqueezefsError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "failed to inspect staging/cache dir '{}': {e}",
                    dir.display()
                ),
            )))
        }
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let recognized = is_recognized_staging_root(dir);
    let total = entries.len();
    let mut preview = entries;
    preview.sort();
    preview.truncate(5);
    if !recognized && !wipe_consent {
        return Err(SqueezefsError::InvalidOperation(format!(
            "staging/cache directory '{}' is not empty ({total} top-level {}: {}{}) and \
             carries no squeezefs staging marker — refusing to wipe it. Verify the path, \
             then re-run with --force to confirm the wipe (config set-cache-paths also \
             accepts --yes), or point at an empty/new directory",
            dir.display(),
            if total == 1 { "entry" } else { "entries" },
            preview.join(", "),
            if total > preview.len() { ", …" } else { "" },
        )));
    }
    Ok(Some(StagingWipePlan {
        total,
        preview,
        recognized,
    }))
}

/// Print the deletion plan for one sanctioned wipe (ENG-4: the plan is
/// shown BEFORE anything is removed).
fn print_wipe_plan(dir: &Path, plan: &StagingWipePlan) {
    let more = plan.total.saturating_sub(plan.preview.len());
    println!(
        "Staging wipe plan for '{}': {} top-level {} will be permanently removed: {}{}{}",
        dir.display(),
        plan.total,
        if plan.total == 1 { "entry" } else { "entries" },
        plan.preview.join(", "),
        if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        },
        if plan.recognized {
            " [recognized squeezefs staging root]"
        } else {
            " [confirmed via --force]"
        },
    );
}

/// Wipe + recreate + OWNERSHIP-STAMP one staging/cache root: the shared
/// format-grade stamp used by `format --disk-cache-paths` and
/// `config set-cache-paths`. The root comes up empty (fresh staging
/// generation, no discard noise) and owned by [`invoking_owner`], so the
/// daemon identity that will actually mount can use it without a manual
/// `chown -R`. The chown only runs as root (elsewhere it is a no-op by
/// construction); failures are loud — a half-stamped root is exactly the
/// EACCES-later trap this exists to close.
///
/// Guarded (ENG-4): the wipe precheck runs first — protected system
/// paths ([`STAGING_WIPE_DENYLIST`]) are hard-refused, and a non-empty
/// dir that does not look like a previously used staging root only
/// wipes with `wipe_consent` (the caller's `--force`/`--yes`). The
/// deletion plan prints before the wipe.
pub async fn stamp_staging_dir(dir: &Path, wipe_consent: bool) -> Result<()> {
    let plan = staging_wipe_precheck(dir, wipe_consent)?;
    stamp_precleared_staging_dir(dir, plan.as_ref()).await
}

/// [`stamp_staging_dir`] for a whole declared set: prechecks EVERY dir
/// before wiping ANY (a refusal must abort with all directories intact —
/// never a partial wipe with an unchanged format config).
pub async fn stamp_staging_dirs(dirs: &[PathBuf], wipe_consent: bool) -> Result<()> {
    let mut plans = Vec::with_capacity(dirs.len());
    for dir in dirs {
        plans.push(staging_wipe_precheck(dir, wipe_consent)?);
    }
    for (dir, plan) in dirs.iter().zip(&plans) {
        log::info!("Stamping local staging/cache directory: {:?}", dir);
        stamp_precleared_staging_dir(dir, plan.as_ref()).await?;
    }
    Ok(())
}

/// The stamp itself, after [`staging_wipe_precheck`] sanctioned it.
async fn stamp_precleared_staging_dir(dir: &Path, plan: Option<&StagingWipePlan>) -> Result<()> {
    let ctx = |what: &str, e: &std::io::Error| {
        SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to {what} staging/cache dir '{}': {e}",
                dir.display()
            ),
        ))
    };
    if let Some(plan) = plan {
        print_wipe_plan(dir, plan);
    }
    {
        let dir = dir.to_path_buf();
        match squeezefs_ipc::sqz_blocking::run_blocking(move || std::fs::remove_dir_all(&dir)).await
        {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ctx("wipe", &e)),
        }
    }
    {
        let dir = dir.to_path_buf();
        squeezefs_ipc::sqz_blocking::run_blocking(move || std::fs::create_dir_all(&dir)).await
    }
    .map_err(|e| ctx("create", &e))?;
    // VAL-7b: mode + ownership are both applied through ONE
    // `O_DIRECTORY | O_NOFOLLOW` fd. The historical
    // `std::os::unix::fs::chown(dir, …)` was a PATH call: a symlink
    // planted at `dir` between the `create_dir_all` above and the chown
    // re-targeted it at any file on the box — as root, on a path the
    // operator supplied. `fchown` on a fd cannot be redirected, and the
    // same fd asserts the 0700 mode (the root holds plaintext staged
    // payloads on passthrough volumes).
    let d = open_dir(dir, true).map_err(|e| ctx("open (O_NOFOLLOW)", &e))?;
    fchmod(&d, STAGING_DIR_MODE).map_err(|e| ctx("set mode 0700 on", &e))?;
    let (uid, gid) = invoking_owner();
    if unsafe { libc::geteuid() } == 0 {
        fchown(&d, uid, gid).map_err(|e| ctx(&format!("stamp ownership {uid}:{gid} on"), &e))?;
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
///   content at mount anyway;
/// - the wipe itself is guarded (ENG-4): system paths are hard-refused
///   and non-empty dirs with no staging marker need `wipe_consent` (the
///   CLI's `--force`/`--yes`) — ALL dirs precheck before ANY is wiped.
///
/// The rewrite itself is one setxattr transaction on the FIRST volume's
/// root inode (where format recorded it), made durable by the v3 journal
/// and closed with a clean checkpoint shutdown.
///
/// [`format_preflight`]: crate::meta_backend::kv::builder::format_preflight
pub async fn set_cache_paths(
    meta_lvs: &[String],
    paths: &[PathBuf],
    wipe_consent: bool,
) -> Result<()> {
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
    //    cleanliness AND the SUDO_UID ownership rule), behind the ENG-4
    //    guard: every dir prechecks before any is wiped, so a refusal
    //    leaves all directories AND the recorded config untouched.
    stamp_staging_dirs(paths, wipe_consent).await?;

    // 4. Rewrite the format config on the first volume (journal-durable
    //    commit + clean checkpoint shutdown).
    cfg.disk_cache_paths = Some(paths.to_vec());
    let bytes = serde_json::to_vec(&cfg).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
    })?;
    let vol = crate::meta_backend::open_volume_for_mount(first).await?;
    // VAL-2: the daemon's OWN record writer, so it rides the unscreened
    // internal entry point. The generic `Metadata::setxattr` on a
    // `KvMetaBackend` mirrors the FUSE allowlist and would refuse
    // `user.squeezefs.format_config` (EPERM) — the screen is the client
    // boundary, never a ban on the administrator of the record.
    vol.setxattr_internal(
        1,
        crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
        &bytes,
    )
    .await?;
    vol.shutdown().await.map_err(SqueezefsError::from)?;
    Ok(())
}

/// `squeezefs job rotate-enroll <sqmeta-uri>` — the ONE rotation lever
/// for the set's job-wire enrollment secret (1.3.0: the record is minted
/// once and reused by every coordinator — a per-mount re-mint broke every
/// member that outlives a coordinator's incarnation). Offline, on a
/// quiesced set: the live-client gate on EVERY volume, then the record
/// removed on the config home volume under the D0 guard (the daemon's own
/// unscreened remover — the FUSE screen keeps `job:` invisible to
/// clients). Returns whether a record was removed.
pub async fn rotate_enroll_secret(meta_lvs: &[String]) -> Result<bool> {
    let first = config_home_volume(meta_lvs).await?;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true).await?;
    }
    let vol = crate::meta_backend::open_volume_for_mount(&first).await?;
    let present = vol
        .getxattr(1, crate::job_wire::JOB_ENROLL_XATTR)
        .await?
        .is_some();
    if present {
        vol.removexattr_internal(1, crate::job_wire::JOB_ENROLL_XATTR)
            .await?;
    }
    vol.shutdown().await.map_err(SqueezefsError::from)?;
    Ok(present)
}

/// `squeezefs config set-fabric-endpoints <sqmeta-uri>
/// <vol-id>=<traddr>:<trsvcid>:<subnqn> ...` — the spec-grammar parser
/// (KD-MW-15). Splits at the FIRST `=`, then the first two `:`
/// boundaries of the remainder — NQNs carry colons, so the subnqn is
/// everything after the second separator. IPv6 transport addresses are
/// bracketed (`[::1]:4420:nqn...`); the brackets are grammar, not part
/// of the stored traddr.
pub fn parse_fabric_endpoint_spec(spec: &str) -> Result<(String, crate::FabricEndpoint)> {
    let grammar = |why: &str| {
        SqueezefsError::InvalidOperation(format!(
            "fabric-endpoint spec '{spec}' is invalid ({why}); the grammar is \
             <vol-id>=<traddr>:<trsvcid>:<subnqn> (bracket an IPv6 traddr: \
             [::1]:4420:nqn...)"
        ))
    };
    let (vol_id, rest) = spec
        .split_once('=')
        .ok_or_else(|| grammar("no '=' between the vol-id and the coordinates"))?;
    let vol_id = vol_id.trim();
    if vol_id.is_empty() {
        return Err(grammar("empty vol-id"));
    }
    let (traddr, rest) = if let Some(bracketed) = rest.strip_prefix('[') {
        let (addr, after) = bracketed
            .split_once(']')
            .ok_or_else(|| grammar("unterminated '[' in the traddr"))?;
        let after = after
            .strip_prefix(':')
            .ok_or_else(|| grammar("expected ':' after the bracketed traddr"))?;
        (addr, after)
    } else {
        rest.split_once(':')
            .ok_or_else(|| grammar("no ':' between traddr and trsvcid"))?
    };
    let (trsvcid, subnqn) = rest
        .split_once(':')
        .ok_or_else(|| grammar("no ':' between trsvcid and subnqn"))?;
    if traddr.is_empty() {
        return Err(grammar("empty traddr"));
    }
    if trsvcid.is_empty() {
        return Err(grammar("empty trsvcid"));
    }
    if subnqn.is_empty() {
        return Err(grammar("empty subnqn"));
    }
    Ok((
        vol_id.to_string(),
        crate::FabricEndpoint {
            traddr: traddr.to_string(),
            trsvcid: trsvcid.to_string(),
            subnqn: subnqn.to_string(),
        },
    ))
}

/// `squeezefs config set-fabric-endpoints`: the ONLY way to declare a
/// DATA volume's NVMe-oF connect coordinates (KD-MW-15 — mount rejects
/// the flag; the cache-path-policy precedent verbatim). Guarded exactly
/// like [`set_cache_paths`]:
///
/// - every metadata volume runs the `format_preflight` live-client gate
///   (changing the coordinate a mount would connect from under a live
///   client is never safe);
/// - the volume set must be formatted, and every named vol-id must be a
///   member of the durable data-volume set (coordinates for a volume the
///   set does not have are a typo, refused loud naming the id) —
///   all checked BEFORE anything is written;
/// - the records commit as ino-1 xattrs on the config-home volume
///   (versioned + checksummed [`crate::FabricEndpoint`] images under
///   [`crate::fabric_endpoint_record_name`], VAL-2-allowlist-invisible),
///   journal-durable, closed with a clean checkpoint shutdown.
pub async fn set_fabric_endpoints(
    meta_lvs: &[String],
    entries: &[(String, crate::FabricEndpoint)],
) -> Result<()> {
    if entries.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "at least one <vol-id>=<traddr>:<trsvcid>:<subnqn> spec is required".to_string(),
        ));
    }

    // 1. Live-client gate on EVERY volume before anything is touched.
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true).await?;
    }

    // 2. The set must be formatted; every named id must be a member.
    let cfg = read_volume_format_config(meta_lvs).await?;
    let records = cfg.resolved_data_volumes();
    for (vol_id, _) in entries {
        if !records.iter().any(|r| &r.id == vol_id) {
            let known: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
            return Err(SqueezefsError::InvalidOperation(format!(
                "'{vol_id}' is not a member of this set's durable data-volume records \
                 (known ids: {}) — fabric-endpoint coordinates name volumes by their \
                 durable id (KD-5); see `squeezefs volume list`",
                known.join(", ")
            )));
        }
    }

    // 3. Commit the records on the config-home volume (one guarded open,
    //    journal-durable, clean shutdown).
    let home = config_home_volume(meta_lvs).await?;
    let vol = crate::meta_backend::open_volume_for_mount(&home).await?;
    let outcome = async {
        for (vol_id, ep) in entries {
            // VAL-2: the daemon's OWN record writer rides the unscreened
            // internal entry point (the set_cache_paths precedent — the
            // allowlist is the client boundary, never a ban on the
            // administrator of the record).
            vol.setxattr_internal(1, &crate::fabric_endpoint_record_name(vol_id), &ep.encode())
                .await?;
        }
        Ok::<(), SqueezefsError>(())
    }
    .await;
    vol.shutdown().await.map_err(SqueezefsError::from)?;
    outcome
}

/// `squeezefs config get-fabric-endpoints` / the mount-side record read:
/// every durable data-volume record with its declared endpoint (or
/// `None`). Read-only probe — safe beside a live mount. A damaged or
/// future-version record refuses loud here (never a guessed coordinate).
pub async fn get_fabric_endpoints(
    meta_lvs: &[String],
) -> Result<Vec<(crate::DataVolumeRecord, Option<crate::FabricEndpoint>)>> {
    let home = config_home_volume(meta_lvs).await?;
    let cfg = read_format_config(&home).await?;
    let records = cfg.resolved_data_volumes();
    let vol = crate::meta_backend::open_volume_probe(&home).await?;
    let root = vol.slot0_root_ino();
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        let name = crate::fabric_endpoint_record_name(&rec.id);
        let ep = match vol.getxattr(root, &name).await? {
            Some(raw) => Some(crate::FabricEndpoint::decode(&raw).map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "fabric_endpoint record for volume '{}' is unusable: {e}",
                    rec.id
                ))
            })?),
            None => None,
        };
        out.push((rec, ep));
    }
    Ok(out)
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
    // RES-12: the probe's `NvmeBlockDev` is short-lived and its `Drop`
    // JOINS the io_uring worker thread. This function runs on a tokio
    // worker in the LIVE daemon (the admin-lane `volume-add-data` verb,
    // `SqueezefsFilesystem::admin_add_data_volume`) as well as in the
    // offline CLI, so the drop is handed to the blocking pool on EVERY
    // exit path — including the error paths, which is why the result is
    // captured first and the device dropped after.
    let outcome = async {
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
    .await;
    if let Some(h) = crate::detached::drop_off_runtime(dev) {
        // Ordered: the probe must be fully torn down before the caller
        // publishes a durable record naming the device.
        let _ = h.await;
    }
    outcome
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
    let dlm = crate::dlm::DlmClient::new()?;
    let live: Vec<&crate::DataVolumeRecord> = records
        .iter()
        .filter(|r| r.state != crate::VOL_STATE_RETIRED)
        .collect();
    let first = live
        .first()
        .ok_or_else(|| E::InvalidOperation("no live data volumes".to_string()))?;
    let first_alloc =
        std::sync::Arc::new(crate::block_allocator::BlockAllocator::new(&first.id).await?);
    if let Ok(cap) = crate::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let first_dev = std::sync::Arc::new(crate::nvme_dev::NvmeBlockDev::new(&first.backing_dev));
    let cache = crate::cache::TieredCache::new(
        Vec::new(), // no staging dirs: per-mount isolated, not ours (see the verb doc)
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await?;
    let router = crate::routing::DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    // §3d.2 (rc-manifest): the set's block size rides the router seams —
    // never a process-env write (the retired set_var-based
    // SQUEEZEFS_DEFAULT_BLOCK_SIZE runtime channel: two
    // different-block-size sets in one process fought over it). The
    // explicit passthrough state is what `get_crypto()` falls back to
    // anyway; installing it via `set_crypto` pins the DUR-8e plaintext
    // bound to THIS set's block size (`init_scratch_pool` records it) —
    // the mount path's exact discipline.
    router.set_block_size(cfg.block_size);
    router.set_crypto(crate::crypto_compress::CryptoCompressState::new(
        "none".to_string(),
        "none".to_string(),
        None,
    ));
    for rec in &live {
        router.backend_router.register_backend(rec).await?;
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(routed.clone());

    // Block-ownership recovery — the census ground truth (§5.2). Pre-RC
    // spec §6.2 item 1: durable reference records where incompat bit 8 is
    // stamped (no inode-tree walk), the derived walk otherwise.
    if router
        .backend_router
        .recover_durable_block_refs(&routed)
        .await?
        .is_none()
    {
        for kv in &routed.volumes {
            for entry in router.backend_router.backends.iter() {
                entry
                    .value()
                    .block_allocator
                    .recover_active_blocks_v3(kv, &router.backend_router)
                    .await?;
            }
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
                        router.backend_router.reclaim_drain().await;
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
    // Claim-release law (the dismount posture; the mover's own pass
    // drains already cover the Completed path — this is the belt for
    // every terminal outcome): return every queued device range BEFORE
    // the caller releases the D0 claims. A queued punch outliving the
    // claims can land on offsets a successor writer has reallocated
    // (`.benchmarks/2026-08-04-volume-drain-flake.md`).
    router.backend_router.reclaim_drain().await;
    match end {
        // The CLI arm owns the user-facing outcome line (this library
        // path reports progress only — printing it here too made the
        // offline drain announce completion twice).
        crate::jobs::JobState::Completed => Ok(()),
        other => Err(E::InvalidOperation(format!(
            "offline drain of '{volume_id}' ended {other:?} — re-run to resume (the plan \
             regenerates idempotently, KD-6)"
        ))),
    }
}

/// `squeezefs clone <src> <dest>` — the OFFLINE, D0-guarded, instant
/// copy-on-write clone (metadata duplicated, block refcounts
/// incremented, not one data byte copied).
///
/// The verb was a **silent no-op** before this function existed: the CLI
/// arm built a bare `DataRouter` with no metadata backend, so
/// `resolve_path_to_inode` short-circuited to ino 1 and `clone_path`'s
/// whole body (an `if let Some(backend) = …`) was skipped — it printed
/// success having created nothing (`tests/cli_clone_tests.rs`).
///
/// Posture, identical to the sibling offline mutating verbs
/// (`volume remove-data`, `fsck --repair`, `defrag` offline):
///
/// * the format preflight refuses loudly under a **live mount** (a
///   throwaway allocator's RAM refcounts a live daemon never sees could
///   pin nothing and alias blocks the daemon concurrently frees);
/// * [`crate::meta_backend::open_routed_meta_set`] takes the **D0
///   claims** (flock + PR + `writer_claim`) for the clone's duration —
///   unconditionally, not only when a flag happens to be passed;
/// * `defrag::build_offline_router` (private) wires the mount-shaped data
///   plane (the recorded data volumes, the block-size, allocator
///   **refcount recovery** — without which every striped pin refuses as
///   untracked and the clone would fail loud instead of sharing blocks);
/// * a **staged** source refuses loudly: its acked payload lives in the
///   mount's per-mount-isolated staging, which this coordinator
///   deliberately never opens (the `remove-data` staging note), so
///   cloning it would mint a zero-filled "successful" clone.
pub async fn clone_path_offline(meta_lvs: &[String], src: &str, dest: &str) -> Result<()> {
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "clone refused: metadata volume '{path}' is not exclusively claimable \
                     (a live writer may hold it — the single-writer guard forbids offline \
                     clones under a live write mount): {e}"
                ))
            })?;
    }
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result = clone_offline_body(&routed, meta_lvs, src, dest).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after offline clone: {e}");
        }
    }
    result
}

/// [`clone_path_offline`]'s body, split so the D0 guards are released on
/// every path.
async fn clone_offline_body(
    routed: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    meta_lvs: &[String],
    src: &str,
    dest: &str,
) -> Result<()> {
    let router = crate::defrag::build_offline_router(routed, meta_lvs).await?;
    // Resolve the source FIRST: a missing path must refuse loudly (the
    // no-op verb "succeeded" on every nonexistent source).
    let src_ino = router.resolve_path_to_inode(src).await.map_err(|e| {
        SqueezefsError::InvalidOperation(format!("clone source '{src}' cannot be resolved: {e}"))
    })?;
    if src_ino == 1 {
        return Err(SqueezefsError::InvalidOperation(format!(
            "clone source '{src}' resolves to the filesystem root — name a file"
        )));
    }
    let meta = router
        .fetch_metadata(crate::keys::inode_path(src_ino).as_str())
        .await?;
    if meta.file_type == "staged" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "clone source '{src}' carries a STAGED layout: its acked payload lives in the \
             mount's isolated staging, which the offline coordinator never opens (cloning \
             it would mint a zero-filled clone). Clone it through a live mount \
             (`cp --reflink=always`, which rides copy_file_range) or flush/promote the \
             file first"
        )));
    }
    let out = router.clone_path(src, dest).await;
    // Claim-release law (the `remove-data` tail): return every queued
    // device range BEFORE the caller releases the D0 claims.
    router.backend_router.reclaim_drain().await;
    out
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
    use crate::meta_backend::kv::checkpoint::{MembershipStamp, STAMP_MAX_RUNS};
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
        for s in st.slots_hosted.iter() {
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
                    .retain(|s| matches!(slot_claim[usize::from(s)], Some((ci, _)) if ci == oi));
                let kept = st.slots_hosted.clone();
                st.slot_cursors.retain(|(s, _)| kept.contains(*s));
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
            let complement = crate::meta_backend::kv::slot_set::SlotSet::from_slots(&missing_slots);
            if complement.is_empty() || complement.runs().len() > STAMP_MAX_RUNS {
                return refuse(format!(
                    "the unhosted slot complement ({} slots in {} runs) cannot belong to \
                     one member",
                    complement.len(),
                    complement.runs().len()
                ));
            }
            let inferred = MembershipStamp {
                set_uuid: first.set_uuid,
                set_epoch: first.set_epoch,
                member_position: pos as u16,
                member_count: first.member_count,
                routing_width: first.routing_width,
                slots_hosted: complement,
                // Position-native inference (no guests minted yet).
                native_slot: Some(pos as u16),
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

/// The staging generation of the set `meta_lvs` describes: the volume-set
/// generation, decorated with THIS node's writer scope when every member
/// carries incompat bit 10 (§6.2 item 10,
/// [`crate::writer_scope::staging_generation`]).
///
/// Both halves of a KD-8 rebind resolve their OWN set's scope — the old
/// set's and the new set's engagement can differ (adding an un-stamped
/// member makes the new set un-scoped, since engagement needs unanimity),
/// and stamping a root with the wrong side's decoration is exactly the
/// mismatch that would make the next mount discard durable staged
/// payloads.
///
/// A scope-resolution failure degrades to UNSCOPED, loudly, rather than
/// wedging a membership verb: the resulting root is bound to the
/// un-scoped generation, which the mount's `ScopeUpgrade` arm adopts and
/// re-stamps — never discards.
async fn staging_generation_for_set(meta_lvs: &[String], set_generation: &str) -> String {
    let scope = match crate::writer_scope::resolve_scope_for_set(meta_lvs).await {
        Ok(scope) => scope,
        Err(e) => {
            log::warn!(
                "writer-scope resolution failed for the metadata set ({e}); staging \
                 generation stays UN-scoped for this operation — the next mount's scope \
                 upgrade arm adopts and re-stamps the roots (nothing is discarded)"
            );
            None
        }
    };
    crate::writer_scope::staging_generation(set_generation, scope)
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
            // §6.2 item 10: the membership test is scope-aware — a root
            // still bound to the UN-scoped generation is rebindable (the
            // upgrade arm), because leaving it behind would strand it on
            // the dead set generation and the next mount would discard its
            // durable staged payloads.
            Some(g)
                if crate::writer_scope::marker_is_rebindable(&g, old_generation)
                    || crate::writer_scope::marker_is_rebindable(&g, new_generation) =>
            {
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
            // pending block custody refuses. §6.2 item 8: a FOREIGN-scoped
            // record refuses too (it is listed with its scope in the
            // refusal) — this process cannot drain custody whose payload
            // ring belongs to another node, and rebinding the root around
            // it would bind a generation to content we cannot classify.
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
            // Scope-aware membership (see `staging_drain_barrier`).
            Some(g)
                if crate::writer_scope::marker_is_rebindable(&g, old_generation)
                    || crate::writer_scope::marker_is_rebindable(&g, new_generation) =>
            {
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
        // Finalize only roots this process may rebind (§6.2 item 10): ANY
        // bound generation that is Match-or-ScopeUpgrade against the new
        // staging generation qualifies — after phase 1 the dual marker's
        // SECOND entry is the new generation, which is why the whole
        // binding set is read rather than only the first. A root bound to
        // a PEER's node scope is left untouched: the pre-item-10 code
        // finalized any marker at all, which would have re-stamped
        // another node's staging root to this node's generation.
        let bindings = crate::cache::read_staging_generation_bindings(dir).await;
        if bindings
            .iter()
            .any(|g| crate::writer_scope::marker_is_rebindable(g, new_generation))
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

/// One member's observation for the membership verbs: path + superblock
/// uuid + stamp.
struct MemberObs {
    path: String,
    uuid: [u8; 16],
    stamp: crate::meta_backend::kv::checkpoint::MembershipStamp,
}

/// Observe an all-stamped coherent-uuid set (membership verbs tolerate
/// epoch/count spread — THEY are the §5.5.2b re-run that converges it).
async fn observe_stamped_members(meta_lvs: &[String]) -> Result<Vec<MemberObs>> {
    let obs = crate::meta_backend::observe_meta_set(meta_lvs).await?;
    let mut out = Vec::with_capacity(obs.len());
    for o in obs {
        let crate::meta_backend::MetaVolumeObservation { path, uuid, stamp } = o;
        let Some(stamp) = stamp else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "metadata volume {path} carries no §5.5.1a membership stamp — every \
                 dynamic-routing format stamps its members at format time, so this ledger \
                 is torn or foreign; run `squeezefs volume repair-set` (inferable states) \
                 or reformat"
            )));
        };
        out.push(MemberObs { path, uuid, stamp });
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

/// §5.5.2b crash-injection seams for [`add_meta_volume`] (the
/// `MigrationTestHooks` precedent): `None` everywhere in production; a
/// test aborts the coordinator at a named window and proves the
/// documented same-arguments re-run converges (VL8 item 8).
#[derive(Default)]
pub struct AddMetaHooks {
    pub crash_after: Option<AddMetaCrash>,
}

/// The named §5.5.2b add-meta crash windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddMetaCrash {
    /// After write 1 — the new member's durable claim @E — before any
    /// survivor re-stamp (survivors still declare the old count).
    NewMemberClaim,
    /// After writes 2..n — every survivor stamped @E, count n+1 —
    /// before source-keyspace teardown + staging finalize + config
    /// mirror (the "stamped-ahead" window the VL7 rig hit).
    SurvivorStamps,
}

/// The §6.2 MW-S1b crash seams of [`enable_multi_writer_with`]
/// (design-full-multi-writer §10): each injects a hard error AFTER the
/// named durable write, so the on-media state is exactly the kill-9
/// window's (the [`AddMetaCrash`] pattern).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnableMwCrash {
    /// After the `mw_upgrade:` intent marker landed durably on volume 0,
    /// before ANY bit write (MW-S1's "kill before any bit" face).
    AfterMarker,
    /// After `bits_done` bits (1..=9, in the §6.2 order
    /// 7→9→15→12→13→8→10→14→11) landed on canonical volume `volume`,
    /// before the next bit / volume / the marker delete. `bits_done = 9`
    /// on a non-final volume is MW-S1 (crash BETWEEN volumes); on the
    /// final volume it is the all-stamped-marker-still-up window.
    AfterBit { volume: usize, bits_done: usize },
}

/// Test seams for [`enable_multi_writer_with`] (MW-S1/S1b).
#[derive(Default)]
pub struct EnableMwHooks {
    pub crash_after: Option<EnableMwCrash>,
}

/// What [`enable_multi_writer`] did.
#[derive(Debug, Clone)]
pub struct EnableMwReport {
    /// Newly-written bits across the whole set (0 = the set was already
    /// fully multi-writer-capable and the verb wrote nothing).
    pub bits_stamped: usize,
    /// The set's volumes in canonical member order (volume 0 first).
    pub volumes: Vec<String>,
}

/// The §6.2 per-volume stamp order (KD-MW-1): dependencies first — bit 13
/// after bit 7 per its own refusal law — and bit 11 deliberately
/// TERMINAL, which is what makes *"bit 11 set ⇒ all nine set"* an
/// invariant the mount gate can enforce
/// ([`crate::meta_backend::refuse_mixed_multi_writer_set`]).
const MW_ENABLE_ORDER: [u64; 9] = [7, 9, 15, 12, 13, 8, 10, 14, 11];

async fn stamp_mw_bit(
    path: &Path,
    bit: u64,
) -> std::result::Result<bool, crate::meta_backend::kv::KvError> {
    use crate::meta_backend::kv::superblock as sb;
    match bit {
        7 => sb::set_durable_term_bit(path).await,
        8 => sb::set_partitioned_append_bit(path).await,
        9 => sb::set_block_refcounts_bit(path).await,
        10 => sb::set_writer_scoped_staging_bit(path).await,
        11 => sb::set_multi_writer_data_bit(path).await,
        12 => sb::set_ino_lanes_bit(path).await,
        13 => sb::set_block_key_incarnation_bit(path).await,
        14 => sb::set_claim_set_bit(path).await,
        15 => sb::set_layout_versions_bit(path).await,
        other => unreachable!("bit {other} is not in the MW_ENABLE_ORDER set"),
    }
}

/// `squeezefs volume enable-multi-writer <sqmeta-uri>` — the KD-MW-1
/// upgrade verb for EXISTING volume sets (design-full-multi-writer §6.2
/// pt 2): **offline** (the `add-meta` D0-guarded coordinator posture),
/// all volumes of the set in ONE invocation, per-volume bit order
/// `7→9→15→12→13→8→10→14→11` (`MW_ENABLE_ORDER`), each stamp barriered
/// (the superblock bit-setters' existing
/// semantics), idempotent (each setter is a no-op on a set bit) and
/// crash-resumable (re-run with the same URI).
///
/// The two §6.2 mechanisms:
///
/// 1. **The `mw_upgrade:` intent marker** — the verb's FIRST act writes
///    one [`crate::MwUpgradeMarker`] record on ino 1 of volume 0 (the
///    KD-2 plane) naming the target bit set + canonical volume list, and
///    its LAST act (after every volume's terminal bit 11) deletes it. A
///    writable mount refuses while the marker exists.
/// 2. **Serialization** — the verb asserts the D0 guard (a guarded
///    [`crate::meta_backend::kv::backend::KvMetaBackend::open`] of volume 0,
///    held across every write) before its first write, so
///    `set_incompat_bit`'s unsynchronized RMW has
///    exactly one setter: a second concurrent invocation refuses on the
///    guard, and no runtime stamper can run because the set is offline.
///    This guarded open is **the ONE marker-tolerant writable open**
///    (scoped here by construction — it never routes through
///    [`crate::meta_backend::open_routed_meta_set`]'s gate), which is what
///    lets the verb resume its own crashed run.
pub async fn enable_multi_writer(meta_lvs: &[String]) -> Result<EnableMwReport> {
    enable_multi_writer_with(meta_lvs, &EnableMwHooks::default()).await
}

/// [`enable_multi_writer`] with the §10 MW-S1/S1b crash seams exposed.
pub async fn enable_multi_writer_with(
    meta_lvs: &[String],
    hooks: &EnableMwHooks,
) -> Result<EnableMwReport> {
    use crate::meta_backend::kv::backend::KvMetaBackend;
    use crate::meta_backend::kv::superblock as sb;

    // Live-client gate on EVERY volume before anything is touched (the
    // add-meta posture; `true` = the already-formatted refusal does not
    // apply — upgrading formatted volumes is the whole point).
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume enable-multi-writer refused: {e}"))
            })?;
    }

    // Canonical member order (§5.5.1a discovery — volume 0 first). The
    // marker lives on volume 0 of THIS order, so resume and mount-gate
    // reads agree on where to look regardless of URI order.
    let disc = crate::meta_backend::discover_meta_set(meta_lvs).await?;
    let ordered = disc.ordered_paths.clone();

    // The D0 guard on volume 0, asserted BEFORE the first write and held
    // across every write of the verb (the sole-setter serialization law).
    let vol0 = KvMetaBackend::open(Path::new(&ordered[0])).await?;

    let body = async {
        let existing = vol0.getxattr(1, crate::MW_UPGRADE_MARKER_XATTR).await?;
        let already_uniform = {
            let mut all = true;
            for path in &ordered {
                let features = match sb::classify_volume(Path::new(path)).await? {
                    sb::VolumeFormat::V3(s) => s.features_incompat,
                    _ => 0,
                };
                if features & sb::MULTI_WRITER_FORMAT_BITS != sb::MULTI_WRITER_FORMAT_BITS {
                    all = false;
                }
            }
            all
        };
        match &existing {
            Some(raw) => {
                // Resume: the marker must name the SAME act — refusing a
                // mismatched list is what keeps "one invocation covers the
                // whole set" true across a crash.
                let marker = crate::MwUpgradeMarker::decode(raw).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "volume enable-multi-writer: the crashed run's intent marker is \
                         unusable ({e}) — refusing to guess the target set"
                    ))
                })?;
                if marker.volumes != ordered {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "volume enable-multi-writer: an upgrade-intent marker already \
                         names volumes {:?} — re-run the verb with exactly that set \
                         (this invocation named {:?})",
                        marker.volumes, ordered
                    )));
                }
            }
            None if already_uniform => {
                // Nothing to do and nothing was written — idempotent no-op.
                return Ok(0usize);
            }
            None => {
                // FIRST act: the durable intent marker, journal-committed
                // and checkpointed BEFORE any bit write (§6.2 mechanism i —
                // it is what covers shape (b) on any volume, including
                // volume 0 itself).
                let marker = crate::MwUpgradeMarker {
                    bits: sb::MULTI_WRITER_FORMAT_BITS,
                    volumes: ordered.clone(),
                };
                vol0.setxattr_internal(1, crate::MW_UPGRADE_MARKER_XATTR, &marker.encode())
                    .await?;
                vol0.checkpoint_now().await.map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "volume enable-multi-writer: the intent marker did not land \
                         durably: {e}"
                    ))
                })?;
            }
        }
        if hooks.crash_after == Some(EnableMwCrash::AfterMarker) {
            return Err(SqueezefsError::InvalidOperation(
                "crash injection (enable-multi-writer: after the intent marker)".to_string(),
            ));
        }

        // Per-volume ordered stamping — idempotent per bit, barriered per
        // write (the existing `set_incompat_bit` semantics). Volume 0's
        // sector 0 may be rewritten while its backend is open: the live
        // backend never writes sector 0 (checkpoints flip the root
        // ledger), and the per-path RMW lock serializes in-process.
        let mut stamped = 0usize;
        for (vi, path) in ordered.iter().enumerate() {
            for (bi, bit) in MW_ENABLE_ORDER.iter().enumerate() {
                if stamp_mw_bit(Path::new(path), *bit).await? {
                    stamped += 1;
                }
                if hooks.crash_after
                    == Some(EnableMwCrash::AfterBit {
                        volume: vi,
                        bits_done: bi + 1,
                    })
                {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "crash injection (enable-multi-writer: volume {vi} after \
                         {} bit(s))",
                        bi + 1
                    )));
                }
            }
        }

        // LAST act (after every volume's terminal bit): delete the marker
        // and checkpoint, re-admitting writable mounts.
        vol0.removexattr_internal(1, crate::MW_UPGRADE_MARKER_XATTR)
            .await?;
        vol0.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-multi-writer: the marker delete did not land durably: {e}"
            ))
        })?;
        Ok(stamped)
    }
    .await;

    // Release the guard on every path (a real kill-9 releases the flock
    // the same way; the durable state the seams simulate is unchanged by
    // this clean release).
    if let Err(e) = vol0.shutdown().await {
        log::warn!("releasing guard after enable-multi-writer: {e}");
    }
    Ok(EnableMwReport {
        bits_stamped: body?,
        volumes: ordered,
    })
}

/// [`OwnerAssignMarker`] wire version. Anything else refuses **loud**
/// (forward-only, the [`crate::MwUpgradeMarker`] law): an unknown version
/// means a newer binary began an assignment this one cannot reason about,
/// and guessing would name the wrong remedy at the mount gate.
pub const OWNER_ASSIGN_MARKER_VERSION: u8 = 1;

/// One volume's entry in an [`OwnerAssignMarker`]: which durable volume
/// gets which owner (`None` = the `--clear` act, restoring the unassigned
/// record), plus its KD-PV-12 successors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAssignment {
    /// The volume's durable `vol-{hex}` identity (KD-5) — never a path,
    /// an ordinal, or a set position.
    pub volume_id: String,
    /// The durable member id that will append to that volume.
    pub owner: Option<String>,
    /// Ordered, statically-declared adoption candidates (KD-PV-12).
    pub successors: Vec<String>,
}

/// The `owner_assign:` intent marker's content
/// (`docs/design-per-volume-claim-admission.md` §5.2.1/§7, KD-PV-2): the
/// per-volume assignment `squeezefs volume set-owners` is applying, so a
/// resume can verify it is completing the **same** act and a mount
/// refusal can name the remedy precisely.
///
/// It brackets the verb: written to ino 1 of the slot-0 volume first,
/// deleted last. Nothing writes it yet — PR 7 lands the verb; PR 4 lands
/// the mount-side probe that refuses a writable mount while it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAssignMarker {
    /// The act, one entry per volume the invocation names, in the set's
    /// canonical member order.
    pub assignments: Vec<OwnerAssignment>,
}

impl OwnerAssignMarker {
    /// Versioned + checksummed record image (the [`crate::MwUpgradeMarker`]
    /// pattern): `version u8 | count u16 LE | (volume_id | owner |
    /// successors) × count | xxh3-64 LE of everything before`, where every
    /// string is `len u16 LE | bytes` and `successors` is itself
    /// `count u16 LE | (len u16 LE | bytes) ×`. An absent owner is the
    /// empty string.
    pub fn encode(&self) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u16).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut out = Vec::with_capacity(1 + 2 + self.assignments.len() * 32 + 8);
        out.push(OWNER_ASSIGN_MARKER_VERSION);
        out.extend_from_slice(&(self.assignments.len() as u16).to_le_bytes());
        for a in &self.assignments {
            put(&mut out, &a.volume_id);
            put(&mut out, a.owner.as_deref().unwrap_or(""));
            out.extend_from_slice(&(a.successors.len() as u16).to_le_bytes());
            for s in &a.successors {
                put(&mut out, s);
            }
        }
        let sum = xxhash_rust::xxh3::xxh3_64(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    /// Decode + verify. Torn (checksum), truncated, trailing-byte,
    /// future-version and duplicate-volume images all refuse loud —
    /// presence alone is the mount gate's refusal predicate, so a marker
    /// that cannot be interpreted still refuses, but never silently
    /// misnames the act it is bracketing.
    pub fn decode(raw: &[u8]) -> std::result::Result<Self, String> {
        if raw.len() < 1 + 2 + 8 {
            return Err(format!(
                "owner_assign marker too short ({} B) — torn or foreign",
                raw.len()
            ));
        }
        let (body, sum_bytes) = raw.split_at(raw.len() - 8);
        let want = u64::from_le_bytes(sum_bytes.try_into().expect("8 B split"));
        if xxhash_rust::xxh3::xxh3_64(body) != want {
            return Err(
                "owner_assign marker checksum mismatch — torn write or corruption".to_string(),
            );
        }
        if body[0] != OWNER_ASSIGN_MARKER_VERSION {
            return Err(format!(
                "owner_assign marker version {} is not the supported version {} — a newer \
                 binary began this assignment; finish it with that binary",
                body[0], OWNER_ASSIGN_MARKER_VERSION
            ));
        }
        let mut pos = 3usize;
        let take = |body: &[u8], pos: &mut usize| -> std::result::Result<String, String> {
            if *pos + 2 > body.len() {
                return Err("owner_assign marker truncated before a field".to_string());
            }
            let len = u16::from_le_bytes(body[*pos..*pos + 2].try_into().expect("2 B")) as usize;
            *pos += 2;
            if *pos + len > body.len() {
                return Err("owner_assign marker truncated inside a field".to_string());
            }
            let s = std::str::from_utf8(&body[*pos..*pos + len])
                .map_err(|_| "owner_assign marker field is not UTF-8".to_string())?
                .to_string();
            *pos += len;
            Ok(s)
        };
        let count = u16::from_le_bytes(body[1..3].try_into().expect("2 B")) as usize;
        let mut assignments = Vec::with_capacity(count);
        for _ in 0..count {
            let volume_id = take(body, &mut pos)?;
            let owner = take(body, &mut pos)?;
            if pos + 2 > body.len() {
                return Err("owner_assign marker truncated before a successor count".to_string());
            }
            let succ_count =
                u16::from_le_bytes(body[pos..pos + 2].try_into().expect("2 B")) as usize;
            pos += 2;
            let mut successors = Vec::with_capacity(succ_count);
            for _ in 0..succ_count {
                successors.push(take(body, &mut pos)?);
            }
            assignments.push(OwnerAssignment {
                volume_id,
                owner: Some(owner).filter(|o| !o.is_empty()),
                successors,
            });
        }
        if pos != body.len() {
            return Err("owner_assign marker carries trailing bytes — torn or foreign".to_string());
        }
        for (i, a) in assignments.iter().enumerate() {
            if assignments[..i].iter().any(|p| p.volume_id == a.volume_id) {
                return Err(format!(
                    "owner_assign marker names volume {} twice — which assignment is the \
                     act cannot be answered, so it is refused rather than guessed",
                    a.volume_id
                ));
            }
        }
        Ok(Self { assignments })
    }
}

// ---------------------------------------------------------------------------
// `squeezefs volume set-owners` / `get-owners` / `locate` — the per-volume
// claim admission operator surface (design-per-volume-claim-admission
// §5.6/§6.1, KD-PV-15, rulings D19/D20; PR 7)
// ---------------------------------------------------------------------------

/// One volume's requested assignment, as the CLI grammar spells it:
/// `<vol-id>=<member-id>[+<successor-id>...][:<subtree-root-path>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAssignSpec {
    /// The DURABLE `vol-{hex}` identity (KD-5), as
    /// [`crate::meta_backend::kv::backend::durable_volume_id_of`] derives
    /// it and as `get-owners` / `locate` print it — never a path, an
    /// ordinal or a set position.
    pub volume_id: String,
    /// The durable member id that will append to that volume (KD-MW-2).
    pub owner: String,
    /// Ordered, statically-declared adoption candidates (KD-PV-12).
    pub successors: Vec<String>,
    /// The owner's subtree root (KD-PV-15). Absent is legal and WARNS:
    /// a node owning a volume but no subtree owns no new work (R16).
    pub subtree_root: Option<String>,
}

/// Parse one `<vol-id>=<member-id>[+<succ>...][:<path>]` argument.
///
/// The `:` split is unambiguous by construction: a member id is
/// `node_{hex}[.m{hex}]` (no colon) and a subtree root is an ABSOLUTE
/// path, which is refused here rather than resolved relative to
/// something the operator cannot see.
pub fn parse_owner_assign_spec(spec: &str) -> Result<OwnerAssignSpec> {
    let refuse = |why: &str| {
        SqueezefsError::InvalidOperation(format!(
            "volume set-owners: cannot read the assignment '{spec}': {why}. The spelling is \
             <vol-id>=<member-id>[+<successor-id>...][:<subtree-root-path>] — for example \
             vol-0a1b2c3d4e5f6071=node_00000000deadbeef.m00000001:/projects/a (ids come from \
             `squeezefs volume get-owners`)"
        ))
    };
    let (volume_id, rest) = spec
        .split_once('=')
        .ok_or_else(|| refuse("no '=' separates the volume id from its owner"))?;
    if volume_id.is_empty() {
        return Err(refuse("the volume id is empty"));
    }
    let (members, subtree_root) = match rest.split_once(':') {
        Some((m, path)) => {
            if !path.starts_with('/') {
                return Err(refuse(
                    "the subtree root must be an ABSOLUTE path inside the filesystem",
                ));
            }
            (m, Some(path.to_string()))
        }
        None => (rest, None),
    };
    let mut ids = members.split('+').map(str::trim);
    let owner = ids
        .next()
        .filter(|o| !o.is_empty())
        .ok_or_else(|| refuse("no owner member id follows the '='"))?;
    let successors: Vec<String> = ids.map(str::to_string).collect();
    if successors.iter().any(String::is_empty) {
        return Err(refuse("an empty successor id"));
    }
    Ok(OwnerAssignSpec {
        volume_id: volume_id.to_string(),
        owner: owner.to_string(),
        successors,
        subtree_root,
    })
}

/// The KD-MW-2 durable member id grammar: `node_{16 hex}` (the
/// slot-wildcard roster form) or `node_{16 hex}.m{8 hex}` (a mount).
///
/// Checked because an id outside it can never equal any mount's
/// [`crate::cowriter::node_member_id`], so a volume assigned to one is a
/// volume no node may ever append to — a set that refuses every mount
/// until an offline re-run.
fn member_id_is_enrollable(id: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let Some(body) = id.strip_prefix("node_") else {
        return false;
    };
    match body.split_once(".m") {
        Some((node, slot)) => hex(node, 16) && hex(slot, 8),
        None => hex(body, 16),
    }
}

/// `--clear` / `--dry-run` / `--accept-cross-owner-names <N>`.
#[derive(Debug, Clone, Default)]
pub struct SetOwnersOptions {
    /// Unassign every volume of the set (the §7 rollback).
    pub clear: bool,
    /// Print the plan — including the M3 census — and write nothing.
    pub dry_run: bool,
    /// The operator's acknowledgement of the cross-owner name population
    /// (KD-PV-11 M3). It must MATCH the counted number.
    pub accept_cross_owner_names: Option<u64>,
}

/// The named crash windows of [`set_owners_with`] (the [`EnableMwCrash`]
/// pattern): each injects a hard error AFTER the named durable write, so
/// the on-media state is exactly the kill-9 window's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOwnersCrash {
    /// After the `owner_assign:` intent marker landed durably, before any
    /// record — the window a writable mount must refuse in.
    AfterMarker,
    /// After `volume`'s subtree root was minted, before its owner record
    /// (the KD-PV-15 resumability window §5.6 names).
    AfterRootMint { volume: usize },
    /// After `volume`'s owner record + enrollment landed, before the next
    /// volume — the half-assigned shape.
    AfterVolume { volume: usize },
}

/// Test seams for [`set_owners_with`].
#[derive(Default)]
pub struct SetOwnersHooks {
    pub crash_after: Option<SetOwnersCrash>,
}

/// One volume's row in a [`SetOwnersReport`].
#[derive(Debug, Clone)]
pub struct VolumeAssignmentRow {
    pub volume_id: String,
    pub path: String,
    /// `true` ⇔ this volume hosts slot 0, so its owner is the SET
    /// AUTHORITY (D20 / KD-PV-6).
    pub hosts_slot_0: bool,
    /// What the record said before this invocation.
    pub previous_owner: Option<String>,
    /// What it says after (or would, on a dry run). `None` = `--clear`.
    pub owner: Option<String>,
    pub successors: Vec<String>,
    pub subtree_root: Option<String>,
}

/// One subtree root the invocation minted or adopted (KD-PV-15).
#[derive(Debug, Clone)]
pub struct SubtreeRootPlan {
    pub path: String,
    pub volume_id: String,
    pub volume_idx: usize,
    pub owner: String,
    /// The ino found already in place and homing on the assigned volume
    /// (an idempotent re-run ADOPTS it).
    pub existing_ino: Option<u64>,
    /// The ino this run minted.
    pub minted_ino: Option<u64>,
    /// Does this root's own name span two owners? (Its dentry lives in
    /// the parent's volume, its inode on the assignee's — the one
    /// cross-owner name per root the supported shape pays.)
    pub cross_owner_name: bool,
}

/// KD-PV-11 **M3**: the cross-owner dentry population the proposed
/// assignment would create — measured in ONE pass over `TREE_DENTRIES`,
/// the pass fsck's C9 already runs.
#[derive(Debug, Clone, Default)]
pub struct CrossOwnerCensus {
    /// Names already in the tree whose parent's owner ≠ their inode's.
    pub existing: u64,
    /// Roots this run would mint that land cross-owner (one per root on
    /// the supported shape).
    pub roots: u64,
    /// `existing + roots` — the number the operator acknowledges.
    pub total: u64,
    /// Per PARENT volume (`vol-{hex}`, count) — where the names live.
    pub per_volume: Vec<(String, u64)>,
    /// A bounded sample, so the refusal shows WHICH names.
    pub sample: Vec<String>,
    /// Dentries walked — the pass's own engagement gauge.
    pub dentries_scanned: u64,
}

/// What [`set_owners`] did (or, on a dry run, would do).
#[derive(Debug, Clone)]
pub struct SetOwnersReport {
    pub volumes: Vec<VolumeAssignmentRow>,
    pub census: CrossOwnerCensus,
    pub roots: Vec<SubtreeRootPlan>,
    /// Loud, operator-facing notes that are NOT refusals (R16's unrooted
    /// assignment above all).
    pub warnings: Vec<String>,
    /// The owner of the slot-0 volume — the SET AUTHORITY (D20).
    pub set_authority: Option<String>,
    /// That volume's durable id.
    pub set_authority_volume: String,
    pub dry_run: bool,
    /// `--clear`: this run unassigned the set rather than assigning it.
    pub cleared: bool,
    /// Volume ownership records written by this run.
    pub records_written: usize,
    pub roots_minted: usize,
    /// Distinct members enrolled on every volume (KD-PV-4).
    pub members_enrolled: usize,
}

/// `squeezefs volume set-owners <sqmeta-uri> <vol-id>=<member-id>…` — the
/// **OFFLINE, D0-guarded, bracketed** assignment coordinator (ruling
/// **D19**; design-per-volume-claim-admission §5.6/§6.1).
///
/// # Why this verb exists at all, and why it is offline
///
/// Ownership is DERIVED at every mount from `claim_set.owner` conjoined
/// with the live claim (KD-PV-3). Nothing in the product writes the
/// assignment half — deliberately, because a live hand-off between two
/// nodes is a two-party protocol with its own failure matrix. D19
/// dissolves it: this verb takes the D0 claim on EVERY volume, so for the
/// length of the bracket the process is the sole authority of the whole
/// set and writes every record itself. There is no peer to agree with.
///
/// # The bracket, in order
///
/// 1. every volume's live-client preflight, then the **D0-guarded open of
///    the whole set** — the enforcement point: a heartbeat-fresh foreign
///    claim on any volume refuses the run and releases what it took;
/// 2. the declarative refusals, before any write: bit 14 on every volume,
///    a complete map, enrollable member ids, ≤ `MAX_LANES` members, no
///    `mw_upgrade:` bracket, **no open cross-volume intent** (§5.4a), and
///    a subtree-root plan whose paths resolve;
/// 3. the **M3 census** — one `TREE_DENTRIES` pass — refused unless
///    `--accept-cross-owner-names <N>` matches the counted number;
/// 4. the `owner_assign:` intent marker on ino 1 of volume 0 (written
///    FIRST, deleted LAST — a writable mount refuses while it stands, so
///    a half-assigned set is never something an operator can mount);
/// 5. per volume in canonical order: mint its subtree root (KD-PV-15,
///    through the preset-ino create path — deterministic, no round-robin
///    luck), enroll every member **pid-less** (KD-PV-4), write the owner
///    record, delete the stale rendezvous record from non-slot-0 volumes
///    (sweep row 17), checkpoint (so the assignment is projection-visible
///    — the §5.9.2 precondition);
/// 6. delete the marker, checkpoint, release every guard.
///
/// Idempotent and crash-resumable throughout: a re-run must name the SAME
/// act (the marker is compared), an already-minted root is adopted rather
/// than re-minted, and a fully-applied assignment with no open bracket
/// writes nothing at all.
pub async fn set_owners(
    meta_lvs: &[String],
    specs: &[OwnerAssignSpec],
    opts: &SetOwnersOptions,
) -> Result<SetOwnersReport> {
    set_owners_with(meta_lvs, specs, opts, &SetOwnersHooks::default()).await
}

/// [`set_owners`] with the named crash windows exposed.
pub async fn set_owners_with(
    meta_lvs: &[String],
    specs: &[OwnerAssignSpec],
    opts: &SetOwnersOptions,
    hooks: &SetOwnersHooks,
) -> Result<SetOwnersReport> {
    let out = set_owners_inner(meta_lvs, specs, opts, hooks).await;
    if out.is_err() {
        crate::meta_ship::note_owner_assign_refusal();
    }
    out
}

/// The set-authority announcement every successful assignment prints
/// (D20): one volume's owner runs the planes the set has exactly one of.
pub fn set_authority_announcement(vol_id: &str, owner: &str) -> String {
    format!(
        "volume {vol_id} hosts slot 0 — its owner {owner} is the SET AUTHORITY: it assigns \
         allocation lanes, serves the S9 custody endpoint, owns the ONLY freed-offset grace \
         ring, coordinates maintenance, and homes ino 1"
    )
}

async fn set_owners_inner(
    meta_lvs: &[String],
    specs: &[OwnerAssignSpec],
    opts: &SetOwnersOptions,
    hooks: &SetOwnersHooks,
) -> Result<SetOwnersReport> {
    use crate::meta_backend::kv::backend::durable_volume_id_of;

    // ---- 0. the spec itself, before any I/O -------------------------
    if opts.clear && !specs.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "volume set-owners --clear unassigns the WHOLE set and takes no per-volume \
             assignments — run it alone, or drop --clear to assign"
                .to_string(),
        ));
    }
    if !opts.clear && specs.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "volume set-owners: no assignments given. Name every volume of the set \
             (<vol-id>=<member-id>[+<successor-id>...][:<subtree-root-path>]), or pass \
             --clear to unassign the whole set"
                .to_string(),
        ));
    }
    for (i, s) in specs.iter().enumerate() {
        if let Some(dup) = specs[..i].iter().find(|p| p.volume_id == s.volume_id) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: volume {} is named twice ('{}' and '{}') — which \
                 assignment is the act cannot be answered, so it is refused rather than \
                 guessed",
                dup.volume_id, dup.owner, s.owner
            )));
        }
        for id in std::iter::once(&s.owner).chain(s.successors.iter()) {
            if !member_id_is_enrollable(id) {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume set-owners: '{id}' (named for volume {}) is not a durable member \
                     identity, so no mount could ever match it and the volume would be \
                     assigned to nobody. The form is KD-MW-2's node_{{16 hex}} or \
                     node_{{16 hex}}.m{{8 hex}} — read a node's id from `squeezefs clients \
                     <sqmeta-uri>` or from its mount log",
                    s.volume_id
                )));
            }
        }
        if let Some(root) = &s.subtree_root {
            if !root.starts_with('/') {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume set-owners: the subtree root '{root}' for volume {} is not an \
                     absolute path inside the filesystem",
                    s.volume_id
                )));
            }
        }
    }
    let mut members: Vec<String> = Vec::new();
    for s in specs {
        for id in std::iter::once(&s.owner).chain(s.successors.iter()) {
            if !members.iter().any(|m| m == id) {
                members.push(id.clone());
            }
        }
    }
    if members.len() > crate::alloc_lane_grant::MAX_LANES as usize {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume set-owners: this assignment names {} distinct members ({}), but a \
             volume set's journal admits at most MAX_LANES = {} appenders \
             (design-per-volume-claim-admission §5.7 — the allocation-lane width is \
             next_power_of_two(members) and the format's own bound is {}). Assign at most \
             {} members, ideally a power of two",
            members.len(),
            members.join(", "),
            crate::alloc_lane_grant::MAX_LANES,
            crate::alloc_lane_grant::MAX_LANES,
            crate::alloc_lane_grant::MAX_LANES
        )));
    }

    // ---- 1. the live-client preflight on every volume ---------------
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "volume set-owners refused: metadata volume '{path}' is not exclusively \
                     claimable — every owner of this set must be unmounted before ownership \
                     is assigned (ruling D19: the assignment runs as the momentary sole \
                     authority of the whole set): {e}"
                ))
            })?;
    }

    // ---- 2. discovery + the per-volume plan -------------------------
    let disc = crate::meta_backend::discover_meta_set(meta_lvs).await?;
    let ordered = disc.ordered_paths.clone();
    let vol_ids: Vec<String> = disc.uuids.iter().map(durable_volume_id_of).collect();
    let mut plan: Vec<Option<&OwnerAssignSpec>> = vec![None; ordered.len()];
    for s in specs {
        let Some(idx) = vol_ids.iter().position(|v| *v == s.volume_id) else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: no volume of this set carries the durable id {} — the \
                 set's volumes are {}. Ownership is keyed on the durable identity (KD-5), \
                 never on a path or a position; `squeezefs volume get-owners <sqmeta-uri>` \
                 prints it",
                s.volume_id,
                vol_ids.join(", ")
            )));
        };
        plan[idx] = Some(s);
    }
    if !opts.clear {
        if let Some(missing) = plan.iter().position(Option::is_none) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: metadata volume {} ({}) is not named by this assignment. \
                 A partial map has no coherent appender story — the unassigned volume \
                 belongs to everyone and to nobody, and every mount of the set would refuse \
                 (KD-PV-3). Re-run over the WHOLE set: {} volume(s), {}",
                vol_ids[missing],
                ordered[missing],
                vol_ids.len(),
                vol_ids.join(", ")
            )));
        }
    }

    // Bit 14 gates the whole program (KD-PV-9) — checked from the
    // superblocks, before a guard is taken.
    for (idx, path) in ordered.iter().enumerate() {
        let features =
            match crate::meta_backend::kv::superblock::classify_volume(Path::new(path)).await? {
                crate::meta_backend::kv::superblock::VolumeFormat::V3(s) => s.features_incompat,
                _ => 0,
            };
        // Symmetric PR 12 (design-symmetric-metadata §6.2 / §7.3, D19
        // reversed): on a symmetric-forest set ownership is a RAM slot
        // lease — first-writer-takes-it, handed over, recovered — so an
        // offline assignment names nothing any mount reads. Refused
        // forward-only naming the successor; a flat set keeps the verb
        // verbatim until the PR-14 flip deletes it.
        if features & crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST != 0
        {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners was RETIRED for a symmetric-forest set (forward-only — \
                 never a silent alias): metadata volume {} ({}) carries incompat bit 17, and \
                 under the symmetric plane every RW mount is a writer whose ownership is a \
                 slot LEASE (first-writer-takes-it, handed over by the holder, recovered on \
                 death — design-symmetric-metadata §5.1, ruling D19 reversed). There is \
                 nothing to assign: mount with SQUEEZEFS_SYMMETRIC_META=1 on every node; \
                 `squeezefs volume get-owners` / `locate` still read the set",
                vol_ids[idx], path
            )));
        }
        if !crate::membership::claim_set_engaged(features) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: metadata volume {} ({}) does not carry the claim-set \
                 capability (incompat bit 14), which is what expresses a SET of writers — \
                 per-volume ownership is gated on it and takes no bit of its own (KD-PV-9). \
                 Upgrade the set offline first: `squeezefs volume enable-multi-writer \
                 <sqmeta-uri>`",
                vol_ids[idx], path
            )));
        }
    }

    // ---- 3. the D0-guarded coordinator open of the WHOLE set --------
    let backends = crate::meta_backend::open_meta_volume_set(&ordered)
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume set-owners refused: {e}. The assignment runs under the D0-guarded \
                 coordinator open of the whole set (ruling D19), so every owner of this set \
                 must be unmounted — a heartbeat-fresh claim from another node is exactly \
                 what this refusal reports"
            ))
        })?;
    let routed = match crate::meta_backend::RoutedMetaBackend::with_slot_map_and_natives(
        backends.clone(),
        disc.routing_width,
        disc.slot_to_volume.clone(),
        disc.native_slots.clone(),
    ) {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => {
            // The guards are already taken; release exactly what the
            // open took before propagating (the `open_meta_volume_set`
            // rollback law, which cannot run for us because the routed
            // constructor consumed the vector).
            for be in &backends {
                if let Err(te) = be.shutdown().await {
                    log::warn!("releasing guard after a refused routed build failed too: {te}");
                }
            }
            return Err(e);
        }
    };
    let body = set_owners_body(&routed, &vol_ids, &plan, &members, opts, hooks).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after volume set-owners: {e}");
        }
    }
    body
}

/// [`set_owners_inner`]'s body, split so the D0 guards are released on
/// every path (the `clone_path_offline` posture).
async fn set_owners_body(
    routed: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    vol_ids: &[String],
    plan: &[Option<&OwnerAssignSpec>],
    members: &[String],
    opts: &SetOwnersOptions,
    hooks: &SetOwnersHooks,
) -> Result<SetOwnersReport> {
    let slot_0_v = routed.route_ino(1).0;
    // The marker lives on CANONICAL volume 0 because that is where every
    // writable mount's bracket probe reads it
    // (`open_intent_marker_refusal(&backends[0])`); on every set the
    // derived slot plan produces, that is also the slot-0 host. Writing
    // it anywhere else would make the bracket invisible to the gate it
    // exists to close.
    let marker_vol = &routed.volumes[0];

    // The `mw_upgrade:` bracket is somebody else's unfinished act.
    if marker_vol
        .getxattr(1, crate::MW_UPGRADE_MARKER_XATTR)
        .await?
        .is_some()
    {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume set-owners: a multi-writer upgrade-intent marker (`{}`) is present on \
             volume 0 — a `squeezefs volume enable-multi-writer` run crashed mid-upgrade and \
             this set's capability bits may be mixed. Finish it first (`squeezefs volume \
             enable-multi-writer <sqmeta-uri>`, idempotent)",
            crate::MW_UPGRADE_MARKER_XATTR
        )));
    }

    // The act this invocation is applying, in canonical volume order.
    let marker = OwnerAssignMarker {
        assignments: (0..routed.volumes.len())
            .map(|v| OwnerAssignment {
                volume_id: vol_ids[v].clone(),
                owner: plan[v].map(|s| s.owner.clone()).filter(|_| !opts.clear),
                successors: plan[v]
                    .map(|s| s.successors.clone())
                    .filter(|_| !opts.clear)
                    .unwrap_or_default(),
            })
            .collect(),
    };
    let resuming = match marker_vol
        .getxattr(1, crate::OWNER_ASSIGN_MARKER_XATTR)
        .await?
    {
        Some(raw) => {
            let open = OwnerAssignMarker::decode(&raw).map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "volume set-owners: the crashed run's intent marker is unusable ({e}) — \
                     refusing to guess which assignment it was applying. The bracket must be \
                     completed or cleared by a binary that can read it"
                ))
            })?;
            if open == marker {
                true
            } else if opts.clear {
                // `--clear` is the terminal state, and it is the remedy
                // the mismatch refusal below names: a set with no owners
                // is the shipped shape, so it may always supersede a
                // half-applied assignment rather than being refused into
                // "complete the act you no longer want, then undo it".
                log::warn!(
                    "volume set-owners --clear: an ownership-assignment bracket was open on \
                     this set naming [{}]; the clear SUPERSEDES it — every volume is \
                     unassigned and the bracket closes on this run",
                    act(&open)
                );
                false
            } else {
                let differing: Vec<String> = marker
                    .assignments
                    .iter()
                    .filter(|a| !open.assignments.iter().any(|o| *o == **a))
                    .map(|a| {
                        format!(
                            "{} → {}",
                            a.volume_id,
                            a.owner.as_deref().unwrap_or("(unassigned)")
                        )
                    })
                    .collect();
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume set-owners: an ownership-assignment bracket is already open on \
                     this set and names a DIFFERENT act — the open one assigns [{}], this \
                     invocation would assign [{}] (differing: {}). Re-run the verb with \
                     exactly the open act's arguments to complete it, or with --clear to \
                     unassign the set",
                    act(&open),
                    act(&marker),
                    differing.join(", ")
                )));
            }
        }
        None => false,
    };

    // §5.4a's barrier: an open cross-volume intent spans a transaction
    // this assignment could strand across two owners.
    for (v, vol) in routed.volumes.iter().enumerate() {
        let intents = vol.xv_scan_intents().await?;
        if !intents.is_empty() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: metadata volume {} ({}) carries {} open cross-volume \
                 intent(s) (tx {:?}). Assigning ownership now would leave a half-applied \
                 transaction spanning two OWNERS, which no node in the fleet could roll \
                 forward and which refuses the next mount. Mount the set once as a single \
                 authority so the intents recover (they roll forward at open), then re-run \
                 this verb",
                vol_ids[v],
                vol.device_path().display(),
                intents.len(),
                intents.iter().map(|(id, _)| *id).collect::<Vec<_>>()
            )));
        }
    }

    // ---- the per-volume rows + the subtree-root plan ----------------
    let mut rows: Vec<VolumeAssignmentRow> = Vec::with_capacity(routed.volumes.len());
    let mut owner_by_volume: Vec<Option<String>> = Vec::with_capacity(routed.volumes.len());
    for (v, vol) in routed.volumes.iter().enumerate() {
        let previous = crate::membership::ClaimSet::load(vol)
            .await
            .filter(|s| s.durable)
            .and_then(|s| s.owner);
        let owner = plan[v].map(|s| s.owner.clone()).filter(|_| !opts.clear);
        owner_by_volume.push(owner.clone());
        rows.push(VolumeAssignmentRow {
            volume_id: vol_ids[v].clone(),
            path: vol.device_path().display().to_string(),
            hosts_slot_0: v == slot_0_v,
            previous_owner: previous,
            owner,
            successors: plan[v]
                .map(|s| s.successors.clone())
                .filter(|_| !opts.clear)
                .unwrap_or_default(),
            subtree_root: plan[v].and_then(|s| s.subtree_root.clone()),
        });
    }
    let set_authority = owner_by_volume[slot_0_v].clone();

    let mut roots: Vec<SubtreeRootPlan> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    if !opts.clear {
        for (v, spec) in plan.iter().enumerate() {
            let Some(spec) = spec else { continue };
            let Some(path) = &spec.subtree_root else {
                // The slot-0 volume's owner needs no root: ino 1 homes
                // there, so every ino that is not under some other
                // owner's subtree is already its work. Warning about it
                // would be false, and a false warning is how a true one
                // stops being read.
                if v != slot_0_v {
                    warnings.push(format!(
                        "volume {} is assigned to {} with NO subtree root: that node will own \
                         a volume but no new work — every ino descends from root and inherits \
                         its parent's owner, so it will ship 100 % of its metadata verbs. \
                         Re-run with {}={}:<absolute-path> to mint one",
                        vol_ids[v], spec.owner, vol_ids[v], spec.owner
                    ));
                }
                continue;
            };
            let (parent, name) = resolve_subtree_parent(routed, path).await?;
            let existing = routed.lookup_dentry(parent, &name).await?;
            if let Some((ino, _)) = existing {
                let home = routed.route_ino(ino).0;
                if home != v {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "volume set-owners: the subtree root '{path}' already exists and its \
                         inode (ino {ino}) homes on metadata volume {} — not on {}, the \
                         volume it would be the root of. A root's inode must live on the \
                         volume its owner appends to, or every descendant would inherit the \
                         WRONG owner (KD-PV-15/M2). Confirm with `squeezefs volume locate \
                         <sqmeta-uri> {path}`, then either assign {} to {} or name a path \
                         that does not exist yet (the verb mints it)",
                        vol_ids[home], vol_ids[v], vol_ids[home], spec.owner
                    )));
                }
            }
            let parent_v = routed.route_ino(parent).0;
            let cross = !same_owner(
                owner_by_volume[parent_v].as_deref(),
                owner_by_volume[v].as_deref(),
            );
            roots.push(SubtreeRootPlan {
                path: path.clone(),
                volume_id: vol_ids[v].clone(),
                volume_idx: v,
                owner: spec.owner.clone(),
                existing_ino: existing.map(|(ino, _)| ino),
                minted_ino: None,
                cross_owner_name: cross,
            });
        }
    }

    // ---- is the act already fully applied? --------------------------
    if !resuming {
        let mut applied = true;
        for (v, vol) in routed.volumes.iter().enumerate() {
            let set = crate::membership::ClaimSet::load(vol)
                .await
                .filter(|s| s.durable);
            let want_owner = owner_by_volume[v].clone();
            let want_succ = rows[v].successors.clone();
            let have_owner = set.as_ref().and_then(|s| s.owner.clone());
            let have_succ = set
                .as_ref()
                .map(|s| s.successors.clone())
                .unwrap_or_default();
            let enrolled = set.as_ref().is_some_and(|s| {
                members.iter().all(|m| {
                    s.members
                        .iter()
                        .any(|e| e.identity.id == *m && e.identity.pid == 0)
                })
            }) || opts.clear;
            if have_owner != want_owner || have_succ != want_succ || !enrolled {
                applied = false;
                break;
            }
        }
        if applied && roots.iter().all(|r| r.existing_ino.is_some()) {
            return Ok(SetOwnersReport {
                volumes: rows,
                census: CrossOwnerCensus::default(),
                roots,
                warnings,
                set_authority,
                set_authority_volume: vol_ids[slot_0_v].clone(),
                dry_run: opts.dry_run,
                cleared: opts.clear,
                records_written: 0,
                roots_minted: 0,
                members_enrolled: members.len(),
            });
        }
    }

    // ---- 3. the M3 census, and its acknowledgement ------------------
    let mut census = if opts.clear {
        CrossOwnerCensus::default()
    } else {
        cross_owner_census(routed, vol_ids, &owner_by_volume).await?
    };
    census.roots = roots
        .iter()
        .filter(|r| r.cross_owner_name && r.existing_ino.is_none())
        .count() as u64;
    census.total = census.existing + census.roots;

    // The dry run REPORTS the census rather than refusing on it: showing
    // the operator the number they must acknowledge is the whole point of
    // the flag, and a refusal here would make the plan unprintable
    // exactly when it matters most.
    if opts.dry_run {
        return Ok(SetOwnersReport {
            volumes: rows,
            census,
            roots,
            warnings,
            set_authority,
            set_authority_volume: vol_ids[slot_0_v].clone(),
            dry_run: true,
            cleared: opts.clear,
            records_written: 0,
            roots_minted: 0,
            members_enrolled: members.len(),
        });
    }

    if census.total > 0 && opts.accept_cross_owner_names != Some(census.total) {
        let named = opts.accept_cross_owner_names;
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume set-owners: {} existing name(s) resolve to inodes on a volume their \
             parent's owner will not own{}. Those names cannot be unlinked, renamed or \
             relinked in place while this assignment stands: `unlink`/`rmdir` on them will \
             return EXDEV, and unlike `rename` there is no copy+unlink fallback. Remedies: \
             assign ownership on a set with no such names (the fresh-fleet recipe), reduce \
             the count by re-homing offline, or clear the assignment (`volume set-owners \
             --clear`), delete, and re-assign. To proceed, acknowledge the exact number: \
             --accept-cross-owner-names {}.\n  by parent volume: {}\n  sample: {}",
            census.total,
            match named {
                Some(n) => format!(
                    " — this invocation acknowledged {n}, which does not match the counted {}",
                    census.total
                ),
                None => format!(
                    " ({} already in the tree + {} subtree root(s) this run would mint)",
                    census.existing, census.roots
                ),
            },
            census.total,
            census
                .per_volume
                .iter()
                .map(|(v, n)| format!("{v}={n}"))
                .collect::<Vec<_>>()
                .join(" "),
            if census.sample.is_empty() {
                "(the roots this run mints)".to_string()
            } else {
                census.sample.join("; ")
            }
        )));
    }

    // ---- 4. the bracket's FIRST act: the intent marker --------------
    if !resuming {
        marker_vol
            .setxattr_internal(1, crate::OWNER_ASSIGN_MARKER_XATTR, &marker.encode())
            .await?;
        marker_vol.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume set-owners: the intent marker did not land durably: {e}"
            ))
        })?;
    }
    if hooks.crash_after == Some(SetOwnersCrash::AfterMarker) {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection (set-owners: after the intent marker)".to_string(),
        ));
    }

    // ---- 5. per volume, in canonical order --------------------------
    let (uid, gid) = invoking_owner();
    let mut records_written = 0usize;
    let mut roots_minted = 0usize;
    for v in 0..routed.volumes.len() {
        // (a) KD-PV-15: the subtree root, minted through the preset-ino
        // create path so its inode lands on the volume being assigned.
        if let Some(slot) = roots.iter().position(|r| r.volume_idx == v) {
            if roots[slot].existing_ino.is_none() {
                let path = roots[slot].path.clone();
                let (parent, name) = resolve_subtree_parent(routed, &path).await?;
                let ino = mint_subtree_root(routed, parent, &name, v, uid, gid).await?;
                roots[slot].minted_ino = Some(ino);
                roots_minted += 1;
                crate::meta_ship::note_subtree_root_minted();
                log::info!(
                    "volume set-owners: minted subtree root '{path}' (ino {ino}) on metadata \
                     volume {} — every descendant inherits its owner {} by M2",
                    vol_ids[v],
                    roots[slot].owner
                );
            }
            if hooks.crash_after == Some(SetOwnersCrash::AfterRootMint { volume: v }) {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "crash injection (set-owners: volume {v} after the subtree-root mint)"
                )));
            }
        }

        let vol = &routed.volumes[v];
        let term = vol.writer_term();
        // (b) KD-PV-4: the roster enrollment is PID-LESS — deliberately
        // process-less, so the rung-8 same-boot prune (which exempts it)
        // can never manufacture an assignment-vs-enrollment disagreement.
        if !opts.clear {
            for id in members {
                crate::membership::upsert_writer_member(
                    vol,
                    &crate::membership::MemberIdentity {
                        id: id.clone(),
                        role: crate::membership::MemberRole::Writer,
                        pid: 0,
                        boot: String::new(),
                        endpoint: None,
                        pr_key: 0,
                    },
                    term,
                )
                .await?;
            }
        }
        // (c) the assignment itself.
        crate::membership::set_volume_owner(
            vol,
            owner_by_volume[v].as_deref(),
            &rows[v].successors,
            term,
        )
        .await?;
        records_written += 1;
        crate::meta_ship::note_owner_assignment();
        // (d) sweep row 17: under a per-volume posture only the slot-0
        // volume carries the membership rendezvous, and a stale copy on a
        // peer-owned volume is one NOBODY can remove later (the peer
        // never writes there and its owner never reads it). This is the
        // one moment in the set's life at which it can be deleted.
        if !opts.clear && v != slot_0_v {
            crate::membership::clear_owner_record(vol).await?;
        }
        // (e) §5.9.2's precondition: the assignment must be visible to a
        // peer's PROJECTION, which reads checkpoints.
        vol.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume set-owners: volume {}'s assignment did not land durably: {e}",
                vol_ids[v]
            ))
        })?;
        if hooks.crash_after == Some(SetOwnersCrash::AfterVolume { volume: v }) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "crash injection (set-owners: after volume {v}'s owner record)"
            )));
        }
    }

    // ---- 6. the bracket's LAST act ----------------------------------
    marker_vol
        .removexattr_internal(1, crate::OWNER_ASSIGN_MARKER_XATTR)
        .await?;
    marker_vol.checkpoint_now().await.map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "volume set-owners: the marker delete did not land durably: {e}"
        ))
    })?;

    if let Some(owner) = &set_authority {
        log::warn!("{}", set_authority_announcement(&vol_ids[slot_0_v], owner));
    }
    for w in &warnings {
        log::warn!("volume set-owners: {w}");
    }
    Ok(SetOwnersReport {
        volumes: rows,
        census,
        roots,
        warnings,
        set_authority,
        set_authority_volume: vol_ids[slot_0_v].clone(),
        dry_run: false,
        cleared: opts.clear,
        records_written,
        roots_minted,
        members_enrolled: if opts.clear { 0 } else { members.len() },
    })
}

/// One assignment act, rendered the way both bracket messages name it:
/// `vol-…=<owner>` per volume, in canonical order.
fn act(marker: &OwnerAssignMarker) -> String {
    marker
        .assignments
        .iter()
        .map(|a| {
            format!(
                "{}={}",
                a.volume_id,
                a.owner.as_deref().unwrap_or("(unassigned)")
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Two assignment entries name the same owner? A bare `node_{hex}` roster
/// entry is the slot WILDCARD form (`member_id_matches`), so it is the
/// same owner as any of that node's mounts — never a cross-owner name.
fn same_owner(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            crate::membership::member_id_matches(a, b) || crate::membership::member_id_matches(b, a)
        }
        _ => false,
    }
}

/// Resolve a subtree root path to `(parent ino, leaf name)`. A missing
/// parent REFUSES rather than being created implicitly (§6.1) — an
/// operator who mistyped `/projcets/a` must be told, not given a new
/// top-level directory.
async fn resolve_subtree_parent(
    routed: &crate::meta_backend::RoutedMetaBackend,
    path: &str,
) -> Result<(u64, String)> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some((name, dirs)) = parts.split_last() else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume set-owners: '{path}' is the filesystem root, which is ino 1 and homes on \
             the slot-0 volume by construction (KD-PV-6) — name a directory under it"
        )));
    };
    let mut parent = 1u64;
    let mut walked = String::new();
    for dir in dirs {
        walked.push('/');
        walked.push_str(dir);
        let found = routed.lookup_dentry(parent, dir).await?;
        let Some((ino, mode)) = found else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: the subtree root '{path}' cannot be minted because its \
                 parent directory '{walked}' does not exist. The verb never creates \
                 intermediate directories implicitly — create the parent through a mount \
                 first, then re-run"
            )));
        };
        if mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume set-owners: '{walked}' (ino {ino}) on the way to the subtree root \
                 '{path}' is not a directory"
            )));
        }
        parent = ino;
    }
    Ok((parent, (*name).to_string()))
}

/// Mint one subtree root ON `volume_idx` (KD-PV-15) through the existing
/// preset-ino create path: pick a mint slot hosted by that volume, mint
/// the ino from it (`make_global_ino_width` then routes it to that volume
/// by construction), and create the directory with the ino pre-supplied.
///
/// Deterministic by construction — the alternative (create, inspect,
/// retry) depends on round-robin luck and leaves rejects behind.
async fn mint_subtree_root(
    routed: &crate::meta_backend::RoutedMetaBackend,
    parent: u64,
    name: &str,
    volume_idx: usize,
    uid: u32,
    gid: u32,
) -> Result<u64> {
    let mint_slot = routed.pick_mint_slot(volume_idx);
    let (_local, global) = routed.allocate_local_ino_in_slot(volume_idx, mint_slot)?;
    if routed.route_ino(global).0 != volume_idx {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume set-owners: the mint of subtree root '{name}' produced ino {global}, \
             which routes to metadata volume {} rather than the assigned {volume_idx} — \
             refusing to create a root whose descendants would inherit the wrong owner",
            routed.route_ino(global).0
        )));
    }
    let ts_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let inode = routed
        .create_with_rdev_preset(
            parent,
            name,
            libc::S_IFDIR | 0o755,
            uid,
            gid,
            0,
            0,
            Some(crate::meta_backend::IntentCreatePreset {
                global_ino: global,
                ts_ns,
            }),
        )
        .await?;
    Ok(inode.ino)
}

/// KD-PV-11 **M3**: ONE sequential pass over `TREE_DENTRIES` on every
/// volume, counting the names whose PARENT's owner differs from the
/// owner of the volume hosting the named inode.
///
/// The walk direction is fsck C9's: a dentry lives on its parent's volume
/// and its value names a child that may live on another, so the parent's
/// owner comes from the volume being scanned and the child's from
/// `route_ino`. Bounded memory: counts plus a small sample, never a set.
async fn cross_owner_census(
    routed: &crate::meta_backend::RoutedMetaBackend,
    vol_ids: &[String],
    owner_by_volume: &[Option<String>],
) -> Result<CrossOwnerCensus> {
    use crate::meta_backend::kv::record::{decode_dentry_key, DentryValue};
    const SAMPLE_MAX: usize = 8;
    const PAGE: usize = 512;

    let mut census = CrossOwnerCensus {
        per_volume: vol_ids.iter().map(|v| (v.clone(), 0)).collect(),
        ..CrossOwnerCensus::default()
    };
    for (v, kv) in routed.volumes.iter().enumerate() {
        let mut cursor: Vec<u8> = vec![0u8; crate::meta_backend::kv::record::DENTRY_KEY_LEN];
        let end = [0xFFu8; crate::meta_backend::kv::record::DENTRY_KEY_LEN];
        loop {
            let page = kv
                .range_kind(
                    crate::meta_backend::kv::record::TREE_DENTRIES,
                    &cursor,
                    &end,
                    PAGE,
                )
                .await?;
            let Some((last, _)) = page.last() else { break };
            cursor = crate::meta_backend::kv::node::key_successor(last);
            for (k, value) in &page {
                let Ok(d) = DentryValue::decode(value) else {
                    continue; // fsck C1's business, never this census's
                };
                census.dentries_scanned += 1;
                let child_v = routed.route_ino(d.child_ino).0;
                if same_owner(
                    owner_by_volume.get(v).and_then(|o| o.as_deref()),
                    owner_by_volume.get(child_v).and_then(|o| o.as_deref()),
                ) {
                    continue;
                }
                census.existing += 1;
                if let Some(entry) = census.per_volume.get_mut(v) {
                    entry.1 += 1;
                }
                if census.sample.len() < SAMPLE_MAX {
                    let parent = decode_dentry_key(k)
                        .ok()
                        .and_then(|(local_parent, _, _)| {
                            routed.try_make_global_ino(local_parent, v)
                        })
                        .unwrap_or(0);
                    // A dentry VALUE is never an authority (fsck C9's
                    // law): an out-of-range child index renders as `?`
                    // rather than indexing this census off a cliff.
                    let named = |idx: usize| vol_ids.get(idx).map(String::as_str).unwrap_or("?");
                    let owner = |idx: usize| {
                        owner_by_volume
                            .get(idx)
                            .and_then(|o| o.as_deref())
                            .unwrap_or("(unassigned)")
                    };
                    census.sample.push(format!(
                        "ino {parent}/{} → ino {} on {} (owner {}), parent on {} (owner {})",
                        String::from_utf8_lossy(&d.name),
                        d.child_ino,
                        named(child_v),
                        owner(child_v),
                        named(v),
                        owner(v),
                    ));
                }
            }
        }
    }
    census.total = census.existing;
    Ok(census)
}

/// One volume's row in `squeezefs volume get-owners` — the DRIFT
/// instrument: the durable assignment printed beside the live evidence
/// the derivation would conjoin it with (KD-PV-3).
#[derive(Debug, Clone)]
pub struct OwnerStatusRow {
    pub volume_id: String,
    pub path: String,
    pub hosts_slot_0: bool,
    pub owner: Option<String>,
    pub successors: Vec<String>,
    /// The live claim's holder, resolved to its DURABLE member id through
    /// the KD-PV-17 attestation. `None` with a live claim = silence.
    pub holder: Option<String>,
    /// The live claim's per-mount uuid, when one exists.
    pub claim_id: Option<String>,
    /// The durable writer era the claim names.
    pub term: u64,
    /// Seconds since the claim's last heartbeat.
    pub claim_age_secs: Option<u64>,
    pub claim_fresh: bool,
    /// The assignment-vs-evidence verdict, in words. `None` = they agree.
    pub drift: Option<String>,
}

/// `squeezefs volume get-owners <sqmeta-uri>` — assignment beside
/// evidence, through read-only probe opens (no guard, no claim, nothing
/// written: it answers on a live set as well as an idle one).
pub async fn get_owners(meta_lvs: &[String]) -> Result<Vec<OwnerStatusRow>> {
    use crate::partial_authority::ClaimStanding;
    let probes = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    let slot_0_v = probes.route_ino(1).0;
    let now = crate::membership::unix_now_secs();
    let mut rows = Vec::with_capacity(probes.volumes.len());
    for (v, vol) in probes.volumes.iter().enumerate() {
        let claim = vol.read_writer_claim().await;
        let set = crate::membership::ClaimSet::load(vol)
            .await
            .filter(|s| s.durable);
        let holder = claim
            .as_ref()
            .zip(set.as_ref())
            .and_then(|(c, s)| s.resolve_holder(c))
            .map(str::to_string);
        let owner = set.as_ref().and_then(|s| s.owner.clone());
        let standing = vol.claim_standing().await;
        let drift = match (&owner, &claim, &holder) {
            (None, _, _) => None,
            (Some(_), None, _) => Some(
                "DEGRADED: assigned owner is not claiming — that node has not started yet, \
                 is down, or the assignment is stale. Other nodes still mount; every verb \
                 about THIS volume's subtree refuses loud at the ship site until its owner \
                 arrives (ownership does not fail over: start it, declare a successor, or \
                 re-assign offline)"
                    .to_string(),
            ),
            (Some(_), Some(_), None) => Some(
                "DRIFT: a live claim whose holder resolves to nothing — the claim's own id \
                 is a per-mount uuid and no KD-PV-17 holder attestation names it. Silence \
                 never moves ownership, so a peer refuses this volume"
                    .to_string(),
            ),
            (Some(owner), Some(_), Some(holder)) => {
                let entitled = crate::membership::member_id_matches(owner, holder)
                    || set.as_ref().is_some_and(|s| {
                        s.successors
                            .iter()
                            .any(|n| crate::membership::member_id_matches(n, holder))
                    });
                if entitled {
                    None
                } else {
                    Some(format!(
                        "DRIFT: the live claim is held by '{holder}', which this volume's \
                         durable assignment set does not name — a mount derives this as a \
                         POISONED entry and refuses (owner_map_poisoned_volumes)"
                    ))
                }
            }
        };
        rows.push(OwnerStatusRow {
            volume_id: vol.durable_volume_id(),
            path: vol.device_path().display().to_string(),
            hosts_slot_0: v == slot_0_v,
            owner,
            successors: set
                .as_ref()
                .map(|s| s.successors.clone())
                .unwrap_or_default(),
            holder,
            claim_id: claim.as_ref().map(|c| c.id.clone()),
            term: claim.as_ref().map(|c| c.term).unwrap_or(0),
            claim_age_secs: claim.as_ref().map(|c| now.saturating_sub(c.ts)),
            claim_fresh: standing == ClaimStanding::Fresh,
            drift,
        });
    }
    // An assigned set with NO claim anywhere is a fleet that is not
    // running — the state every assignment is in the moment it is made.
    // Calling that "drift" per volume cries wolf on the expected shape
    // and teaches the operator to skip the line that matters: ONE volume
    // unclaimed while its siblings are held.
    if rows.iter().all(|r| r.claim_id.is_none()) {
        for row in rows.iter_mut().filter(|r| r.owner.is_some()) {
            row.drift = Some(
                "not mounted: no volume of this set carries a live claim, so this volume's \
                 assigned owner is not claiming it either. Expected while the fleet is down — \
                 each owner's mount is what makes this row live"
                    .to_string(),
            );
        }
    }
    Ok(rows)
}

/// What `squeezefs volume locate` answers.
#[derive(Debug, Clone)]
pub struct LocateReport {
    /// The path as asked for (or `ino <n>` when resolved from a live
    /// mount's `stat`).
    pub path: String,
    pub ino: u64,
    /// The routing slot the ino homes in (`slot_of_ino`).
    pub slot: u64,
    pub volume_id: String,
    pub volume_idx: usize,
    /// `true` ⇔ that volume hosts slot 0 (its owner is the SET AUTHORITY).
    pub hosts_slot_0: bool,
    /// The volume's assigned owner (`None` = unassigned: this node's).
    pub owner: Option<String>,
    /// The live claim holder's durable id, when attested.
    pub holder: Option<String>,
}

/// `squeezefs volume locate <sqmeta-uri> <path>` — the question nothing
/// in the CLI could answer before (§5.5.1: the manual bootstrap's missing
/// instrument, PR 8's setup assertion, and the first thing to reach for
/// when `cross_owner_refusals` moves).
///
/// Read-only probe opens: no guard, no claim, nothing written.
pub async fn locate_path(meta_lvs: &[String], path: &str) -> Result<LocateReport> {
    let probes = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    let mut ino = 1u64;
    let mut walked = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        walked.push('/');
        walked.push_str(part);
        let Some((child, _)) = probes.lookup_dentry(ino, part).await? else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume locate: '{path}' does not resolve — '{walked}' was not found. (A \
                 path created through a live mount becomes visible to this offline read at \
                 the writer's next checkpoint.)"
            )));
        };
        ino = child;
    }
    locate_report(&probes, path.to_string(), ino).await
}

/// [`locate_path`] for an ino resolved elsewhere — the live-mountpoint
/// form, where `stat(2)` on the mount answers with the GLOBAL ino
/// directly and is current rather than checkpoint-lagged.
pub async fn locate_ino(meta_lvs: &[String], ino: u64, label: &str) -> Result<LocateReport> {
    let probes = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    locate_report(&probes, label.to_string(), ino).await
}

async fn locate_report(
    probes: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    path: String,
    ino: u64,
) -> Result<LocateReport> {
    let (volume_idx, _) = probes.route_ino(ino);
    let vol = probes.volumes.get(volume_idx).ok_or_else(|| {
        SqueezefsError::InvalidOperation(format!(
            "volume locate: ino {ino} routes to volume {volume_idx}, which this set does not \
             have ({} volumes)",
            probes.volumes.len()
        ))
    })?;
    let claim = vol.read_writer_claim().await;
    let set = crate::membership::ClaimSet::load(vol)
        .await
        .filter(|s| s.durable);
    let holder = claim
        .as_ref()
        .zip(set.as_ref())
        .and_then(|(c, s)| s.resolve_holder(c))
        .map(str::to_string);
    Ok(LocateReport {
        path,
        ino,
        slot: probes.slot_of_ino(ino),
        volume_id: vol.durable_volume_id(),
        volume_idx,
        hosts_slot_0: volume_idx == probes.route_ino(1).0,
        owner: set.and_then(|s| s.owner),
        holder,
    })
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
    add_meta_volume_with(meta_lvs, device, take, &AddMetaHooks::default()).await
}

/// [`add_meta_volume`] with the §5.5.2b crash seams exposed.
pub async fn add_meta_volume_with(
    meta_lvs: &[String],
    device: &str,
    take: &TakeSlots,
    hooks: &AddMetaHooks,
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

    // Resume detection: is the device already a stamped member of THIS
    // set (a crashed prior attempt)? Read BEFORE the config/generation
    // probes (VL8 item 8): a stamped-ahead crash leaves the old URI
    // refusing discovery, and the device's claim is the evidence that
    // licenses the resume fallbacks below.
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
    // The §5.5.2b resume evidence: the device claims membership of THIS
    // set at exactly count n+1 (writes 1..n of the crashed run landed).
    let resume_evidence = dev_stamp
        .as_ref()
        .is_some_and(|st| usize::from(st.member_count) == meta_lvs.len() + 1);

    // KD-MW-1 (design-full-multi-writer §6.2, the two interaction rules):
    // the bit-11 uniformity law read AT the add, BEFORE anything
    // destructive. A bit-11-uniform set stamps the new member to match
    // (`set_multi_writer` selects the mw format arm below); the converse —
    // a bit-11 volume joining a non-upgraded set — refuses here, and a
    // MIXED set refuses naming its own resume remedy.
    let bit11_of = |fmt: &crate::meta_backend::kv::superblock::VolumeFormat| match fmt {
        crate::meta_backend::kv::superblock::VolumeFormat::V3(sb) => {
            sb.features_incompat
                & crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
                != 0
        }
        _ => false,
    };
    let mut member_mw = Vec::with_capacity(members.len());
    for m in &members {
        member_mw.push(bit11_of(
            &crate::meta_backend::kv::superblock::classify_volume(Path::new(&m.path)).await?,
        ));
    }
    let set_multi_writer = member_mw.iter().all(|&b| b) && !member_mw.is_empty();
    if !set_multi_writer && member_mw.iter().any(|&b| b) {
        return Err(SqueezefsError::InvalidOperation(
            "volume add-meta refused: bit-11 (multi-writer) presence differs across the \
             existing set — converge it first with `squeezefs volume \
             enable-multi-writer <sqmeta-uri>` (idempotent), then re-run the add"
                .to_string(),
        ));
    }
    let device_mw = matches!(
        crate::meta_backend::kv::superblock::classify_volume(Path::new(device)).await,
        Ok(ref fmt) if bit11_of(fmt)
    );
    if device_mw && !set_multi_writer {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume add-meta refused: device {device} carries the multi-writer data-plane \
             bit (11) but the set it would join is not multi-writer-capable — the bit-11 \
             uniformity law (design-full-multi-writer §6.2) holds at the add in both \
             directions. Upgrade the set first (`squeezefs volume enable-multi-writer`) \
             or add an unformatted device"
        )));
    }
    if set_multi_writer && dev_stamp.is_some() && !device_mw {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume add-meta refused: device {device} is a crashed prior member WITHOUT \
             the multi-writer bits while the set is multi-writer-capable — a mixed-era \
             resume. Finish it with `squeezefs volume enable-multi-writer <sqmeta-uri>` \
             over the extended set after the add converges, or reformat the device"
        )));
    }

    // Config-less sets (library/test harnesses format volumes without
    // the bootstrap xattr) simply have no staging dirs to barrier.
    // VL8 item 8: after a stamped-ahead crash the OLD URI refuses
    // discovery — with resume evidence, retry on the extended URI.
    let cfg = match read_volume_format_config(meta_lvs).await {
        Ok(c) => Some(c),
        Err(_) if resume_evidence => {
            let mut extended: Vec<String> = meta_lvs.to_vec();
            extended.push(device.to_string());
            match read_volume_format_config(&extended).await {
                Ok(c) => Some(c),
                Err(_) => {
                    log::warn!(
                        "add-meta resume: format config unreadable on both the old and \
                         extended URIs (mid-stamp crash state) — staging rebind marks \
                         are finalized by the next coherent run"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };

    // KD-8 phase 1 BEFORE anything flips: refuse pending write custody,
    // dual-mark the isolated staging roots so durable staged payloads
    // REBIND across the membership change (never a discard window).
    let staging_dirs = staging_isolated_roots(cfg.as_ref());
    let old_gen = match crate::meta_backend::volume_set_generation(meta_lvs).await {
        Ok(g) => g,
        Err(e) => {
            // VL8 item 8 — the stamped-ahead crash window the VL7 rig
            // hit: every member (old + new) already declares n+1, so the
            // old URI refuses discovery and the same-arguments re-run
            // used to die HERE. With the device's claim as evidence,
            // recompute the OLD set's generation from the old members'
            // superblock uuids in member-position order (identical to
            // the pre-add discovery order). Without that evidence the
            // refusal stands — it is the §5.5.1a gate working.
            if !resume_evidence {
                return Err(e);
            }
            let mut ordered: Vec<&MemberObs> = members.iter().collect();
            ordered.sort_by_key(|m| m.stamp.member_position);
            crate::meta_backend::generation_from_uuids(
                &ordered.iter().map(|m| m.uuid).collect::<Vec<_>>(),
            )
        }
    };
    // §6.2 item 10: the marker binds the NODE-SCOPED staging generation on
    // a stamped set (byte-identical to `old_gen` on every other set).
    let old_gen = staging_generation_for_set(meta_lvs, &old_gen).await;

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
        (Some(st), _) if !st.slots_hosted.is_empty() => st.slots_hosted.to_vec(),
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
                // A member may lose all but one slot. Census economy
                // (design-dynamic-meta-routing §5.6): only the native
                // slot and cursor-bearing guest slots can carry records
                // — virgin hosted slots are empty BY CONSTRUCTION (the
                // same law that makes lazy cursors sound), so they
                // census as zero without a probe and only ever pad the
                // ranking's tail.
                let native = m.stamp.resolved_native_slot();
                let mut loads = Vec::new();
                let mut loaded_probed = 0usize;
                for s in hosted.iter() {
                    let bearing = Some(s) == native || m.stamp.cursor_for(s).is_some();
                    if bearing {
                        loads.push((slot_record_count(&m.path, s, native).await?, s));
                        loaded_probed += 1;
                    } else if loads.len()
                        < loaded_probed + usize::try_from(*k).unwrap_or(usize::MAX)
                    {
                        loads.push((0, s));
                    }
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
        let stamp = crate::meta_backend::kv::checkpoint::MembershipStamp {
            set_uuid: members[0].stamp.set_uuid,
            set_epoch: epoch,
            member_position: new_position,
            member_count: old_count + 1,
            routing_width: width,
            slots_hosted: crate::meta_backend::kv::slot_set::SlotSet::new(),
            native_slot: None,
            slot_cursors: Vec::new(),
        };
        if set_multi_writer {
            // §6.2 interaction rule 1: growing a bit-11-uniform set stamps
            // the new member to match AS PART OF THE ADD — the fresh-format
            // arm (one plan, one superblock write; no marker needed, a
            // fresh volume has no prior state to sequence through). Since
            // the rung-10b flip this is also just the default class.
            crate::meta_backend::kv::builder::format_v3_stamped(
                Path::new(device),
                volume_len,
                &opts,
                stamp,
            )
            .await?;
        } else {
            // The SAME uniformity law in the other direction: a
            // non-upgraded (single-writer-class) set grows with an
            // UNSTAMPED member — the rung-10b default would mint a
            // bit-11 volume inside a non-upgraded set, exactly the
            // mixed shape the mount gate refuses.
            crate::meta_backend::kv::builder::format_v3_stamped_single_writer(
                Path::new(device),
                volume_len,
                &opts,
                stamp,
            )
            .await?;
        }
    }

    // KD-8 phase 1 (the §5.5.2b write-0 class: durable, flips nothing):
    // the new set's generation is derivable now that the member is
    // formatted — dual-mark the staging roots.
    let new_gen = {
        let mut uris: Vec<String> = meta_lvs.to_vec();
        uris.push(device.to_string());
        match crate::meta_backend::volume_set_generation(&uris).await {
            // The NEW set resolves its OWN scope (see
            // `staging_generation_for_set`): a fresh member without the bit
            // makes the extended set un-scoped, and the roots must be
            // stamped the way the next mount will compute them.
            Ok(g) => staging_generation_for_set(&uris, &g).await,
            Err(_) => old_gen.clone(),
        }
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
                        .filter(|st| st.slots_hosted.contains(slot))
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
        claim.slots_hosted = crate::meta_backend::kv::slot_set::SlotSet::from_slots(&taken);
        claim.native_slot = None;
        claim.slot_cursors = cursors;
        target.set_membership_stamp(claim);
        target
            .checkpoint_now()
            .await
            .map_err(|e| SqueezefsError::InvalidOperation(format!("claim write failed: {e}")))?;
        if hooks.crash_after == Some(AddMetaCrash::NewMemberClaim) {
            return Err(SqueezefsError::InvalidOperation(
                "crash injection (add-meta: after new member claim)".to_string(),
            ));
        }

        // Write 2..n: every OLD member's stamp @E (count n+1, minus the
        // slots it lost).
        for (_, be) in &sources {
            let Some(mut st) = be.membership_stamp() else {
                continue;
            };
            st.native_slot = st.resolved_native_slot();
            st.set_epoch = epoch;
            st.member_count = old_count + 1;
            st.slots_hosted.retain(|s| !taken.contains(&s));
            st.slot_cursors.retain(|(s, _)| !taken.contains(s));
            for s in &taken {
                be.remove_guest_cursor(*s);
            }
            be.set_membership_stamp(st);
            be.checkpoint_now().await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("member re-stamp failed: {e}"))
            })?;
        }
        if hooks.crash_after == Some(AddMetaCrash::SurvivorStamps) {
            return Err(SqueezefsError::InvalidOperation(
                "crash injection (add-meta: after survivor stamps)".to_string(),
            ));
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
    let new_gen = staging_generation_for_set(&new_uris, &new_gen).await;
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
    // §6.2 item 10 (see `staging_generation_for_set`).
    let old_gen = staging_generation_for_set(meta_lvs, &old_gen).await;
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
            // The SAME resolver the checkpoint task uses (derivation
            // sweep 2026-08-04) — the preflight headroom must price the
            // cap the survivors will actually run.
            let max_dirty = crate::meta_backend::kv::checkpoint::resolve_max_dirty_nodes(
                crate::mem_budget::MEM_BUDGET.resolve_budget_now(),
                u64::from(be.superblock().node_size),
                std::env::var(crate::meta_backend::kv::checkpoint::CHECKPOINT_MAX_DIRTY_NODES_ENV)
                    .ok()
                    .as_deref(),
            );
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
    let new_gen_planned = match crate::meta_backend::volume_set_generation(&survivors_paths).await {
        // The SURVIVOR set resolves its own scope (see
        // `staging_generation_for_set`).
        Ok(g) => staging_generation_for_set(&survivors_paths, &g).await,
        Err(_) => old_gen.clone(),
    };
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
        for slot in victim_slots.iter() {
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
                    st.slots_hosted.insert(*slot);
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
        tomb.slots_hosted = crate::meta_backend::kv::slot_set::SlotSet::new();
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
    let new_gen = staging_generation_for_set(&survivors_paths, &new_gen).await;
    staging_rebind_finalize(&staging_dirs, &new_gen).await?;
    update_meta_config_mirror(&survivors_paths).await?;
    Ok(())
}

/// Per-member hosted-slot runs for the FormatConfig mirror: O(runs)
/// per member, never O(W) (design-dynamic-meta-routing §5.6).
fn slot_runs_mirror(volume_count: usize, slot_to_volume: &[usize]) -> Vec<Vec<(u16, u16, u32)>> {
    let mut hosted: Vec<Vec<u16>> = vec![Vec::new(); volume_count];
    for (slot, &v) in slot_to_volume.iter().enumerate() {
        hosted[v].push(slot as u16);
    }
    hosted
        .into_iter()
        .map(|slots| {
            crate::meta_backend::kv::slot_set::SlotSet::from_slots(&slots)
                .runs()
                .iter()
                .map(|r| (r.start, r.stride, r.count))
                .collect()
        })
        .collect()
}

/// Rewrite the informational FormatConfig meta mirror (width / slot runs
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
        cfg.meta_slot_runs = Some(slot_runs_mirror(
            disc.ordered_paths.len(),
            &disc.slot_to_volume,
        ));
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

// ---------------------------------------------------------------------------
// PR MW-1 (design-full-multi-writer §5.1, KD-MW-2): the moved-mount-point
// law — staging-root liveness locks, the foreign-slot residue scan, the
// mount-side sibling adoption, and the `squeezefs staging adopt|discard`
// verbs. Crash window MW-1b's contract: foreign-slot staged residue of the
// same set+node is detected at mount, reported LOUD on EVERY mount until
// resolved, rendered by `squeezefs clients`, and resolved by an explicit
// operator act — never silently stranded (the never-lossy law) and never
// silently destroyed.
// ---------------------------------------------------------------------------

/// The per-staging-root liveness lock: a daemon-lifetime `flock(LOCK_EX)`
/// a SCOPED mount holds on every staging root it has bound (the D0
/// Layer-A flock discipline — kernel-arbitrated, instant crash reclaim).
/// It is what lets the residue scan and the `staging` verbs distinguish a
/// LIVE co-located sibling's root (skip silently / refuse to touch) from
/// a DEAD client's residue (report / resolve). Un-scoped mounts take no
/// lock: only pair-decorated markers are ever classified as residue, and
/// only scoped binaries mint those — so the solo-dark footprint is zero.
pub const STAGING_OWNER_LOCK: &str = ".squeezefs_owner.lock";

/// The `staging adopt` verb's tombstone: marks a residue root whose
/// marker was re-bound to the NODE-ONLY scope by an explicit operator
/// act, which is what authorizes the next scoped mount of this set on
/// this node to bind it (winner-takes via the liveness lock). Without the
/// tombstone a node-only-marked sibling is NEVER auto-bound — a KD-8
/// membership verb also restamps roots node-only, and auto-binding those
/// would let one co-located mount swallow its siblings' roots after an
/// `add-meta`.
pub const STAGING_NODE_ADOPTED_MARKER: &str = ".squeezefs_adopted_to_node";

/// Locks held for the daemon's lifetime on every staging root this mount
/// has bound or adopted (dropping a `File` releases its flock, so they
/// are parked here).
static STAGING_ROOT_LOCKS: std::sync::Mutex<Vec<std::fs::File>> = std::sync::Mutex::new(Vec::new());

/// `flock` a staging root's liveness lock file. `Ok(None)` = the lock is
/// HELD by a live process; `Ok(Some(file))` = acquired (hold the `File`
/// to keep it). `create` = mint the lock file if absent (owners create,
/// probes do not).
fn try_staging_root_lock(dir: &Path, create: bool) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let path = dir.join(STAGING_OWNER_LOCK);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .open(&path)
    {
        Ok(f) => f,
        // No lock file and we may not create one: nothing holds it (a
        // pre-pair binary's root, or a probe over a never-locked root).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) => return Err(e),
    };
    // SAFETY: valid owned fd; LOCK_NB never blocks.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(err)
}

/// `true` ⇔ a live process holds `dir`'s staging-root liveness lock.
/// A missing lock file reads as DEAD: only scoped binaries mint
/// pair-decorated markers, and every scoped mount takes the lock — so a
/// pair-marked root without a lock file is a crashed/departed client's
/// (or a pre-pair binary's, whose markers are never pair-decorated).
pub fn staging_root_owner_is_live(dir: &Path) -> bool {
    if !dir.join(STAGING_OWNER_LOCK).exists() {
        return false;
    }
    match try_staging_root_lock(dir, false) {
        Ok(Some(_probe)) => false, // acquired ⇒ nobody held it (drop releases)
        Ok(None) => true,
        // An unreadable lock file proves nothing either way — treat as
        // LIVE, the do-not-touch direction (never destroy on ambiguity).
        Err(_) => true,
    }
}

/// Release every parked staging-root liveness lock — the unmount/teardown
/// half of [`hold_staging_root_lock`] (a daemon exit releases them anyway;
/// the in-process suites simulate successive mounts and need the explicit
/// release, because `flock` treats a second fd of one file in one process
/// as a second owner).
pub fn release_staging_root_locks() {
    STAGING_ROOT_LOCKS
        .lock()
        .expect("staging root lock registry poisoned")
        .clear();
}

/// Acquire and PARK a staging root's liveness lock for the daemon's
/// lifetime. Refuses loud when a live process already holds it.
pub fn hold_staging_root_lock(dir: &Path) -> Result<()> {
    match try_staging_root_lock(dir, true) {
        Ok(Some(file)) => {
            STAGING_ROOT_LOCKS
                .lock()
                .expect("staging root lock registry poisoned")
                .push(file);
            Ok(())
        }
        Ok(None) => Err(SqueezefsError::InvalidOperation(format!(
            "staging root {} is HELD by a live process (its {STAGING_OWNER_LOCK} flock is \
             taken) — another mount owns this root",
            dir.display()
        ))),
        Err(e) => Err(SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "cannot take the staging-root liveness lock at {}: {e}",
                dir.display()
            ),
        ))),
    }
}

/// [`hold_staging_root_lock`] with the bounded teardown-race wait-out
/// (2026-08-16, the stamped-solo QUICK gate's first live catch —
/// generic/003's zero-dwell remount): `umount(8)` returns when the kernel
/// FUSE connection closes, but the predecessor daemon's flock releases
/// only at PROCESS EXIT, so a successor mount at the same mount point can
/// meet its OWN root held by a holder that is milliseconds from gone. A
/// dying holder frees the flock within one poll pass; a genuinely live
/// co-located collision never does and pays the bound ONCE before the
/// unchanged loud refusal (the `await_transient_flock_release`
/// posture, applied to the staging plane — the lock file carries no
/// holder claim, and every holder of a mount's OWN root is same-mount-
/// point class, so the bound applies to all of them). Used ONLY for the
/// prelude's own-dirs arm; probes and adoption arms stay one-shot.
pub async fn hold_staging_root_lock_waiting(dir: &Path) -> Result<()> {
    /// Generous vs. a daemon exit's ms-grade lock release; a live
    /// collision pays it once before the refusal.
    const TEARDOWN_FLOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
    const POLL: std::time::Duration = std::time::Duration::from_millis(5);
    let deadline = std::time::Instant::now() + TEARDOWN_FLOCK_WAIT;
    loop {
        match hold_staging_root_lock(dir) {
            Err(SqueezefsError::InvalidOperation(_)) if std::time::Instant::now() < deadline => {
                squeezefs_ipc::sqz_time::sleep(POLL).await;
            }
            outcome => return outcome,
        }
    }
}

/// One same-node staging root bound to a mount slot (KD-MW-2): the
/// residue scan's row, the `squeezefs clients` residue row, and the
/// `staging adopt|discard` verbs' unit.
#[derive(Debug, Clone)]
pub struct SlotResidue {
    /// The staging root.
    pub dir: PathBuf,
    /// The scope its marker carries (same node as ours, slotted).
    pub scope: crate::writer_scope::WriterScope,
    /// Live staged write-custody keys (sample, up to 8) — empty means the
    /// root holds no live custody (its content is discardable-lossless,
    /// but it is still an operator's to resolve, never auto-destroyed).
    pub live_units: Vec<String>,
    /// `true` ⇔ a live process holds the root's liveness lock (a live
    /// co-located sibling mount — not residue; the scan reports it only
    /// so the verbs can refuse to touch it).
    pub live_owner: bool,
}

/// Enumerate this node's slot-decorated staging roots for one volume set:
/// every sibling directory under `containers` whose generation marker
/// names `set_generation`'s set, OUR `node`, and a nonzero mount slot
/// (optionally filtered to `want_slot`). Directories in `exclude` (the
/// calling mount's own roots) are skipped.
pub async fn find_slot_roots(
    containers: &[PathBuf],
    exclude: &[PathBuf],
    set_generation: &str,
    node: u64,
    want_slot: Option<u32>,
) -> Vec<SlotResidue> {
    let (want_set, _) = crate::writer_scope::split_staging_generation(set_generation);
    let mut out = Vec::new();
    for container in containers {
        let Ok(entries) = std::fs::read_dir(container) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !entry.metadata().map(|m| m.is_dir()).unwrap_or(false)
                || entry.file_name() == "cache_segment"
                || exclude.contains(&dir)
            {
                continue;
            }
            let Ok(Some(marker)) = crate::cache::read_staging_generation_marker(&dir).await else {
                continue;
            };
            let (marker_set, Some(scope)) = crate::writer_scope::split_staging_generation(&marker)
            else {
                continue;
            };
            if marker_set != want_set
                || scope.node != node
                || scope.slot == 0
                || want_slot.is_some_and(|s| s != scope.slot)
            {
                continue;
            }
            let live_owner = staging_root_owner_is_live(&dir);
            let live_units = crate::cache::scan_live_staged_custody(&dir, 8)
                .await
                .unwrap_or_default();
            out.push(SlotResidue {
                dir,
                scope,
                live_units,
                live_owner,
            });
        }
    }
    out
}

/// What the mount-side sibling walk decided (design §5.1(a) + the
/// `-o client_slot=` adoption arm).
#[derive(Debug)]
pub struct ScopedSiblingScan {
    /// Sibling roots ADOPTED into this mount's staging-dir set: their
    /// markers carry exactly OUR pair (the `-o client_slot=` remedy, or a
    /// spelling-variant of our own mount point), or the node-only scope
    /// plus the `staging adopt` tombstone. Liveness locks already held.
    pub adopted: Vec<PathBuf>,
    /// Foreign-slot DEAD roots holding live staged custody — the MW-1b
    /// residue this mount must report LOUD (and must not touch).
    pub residue: Vec<SlotResidue>,
}

/// The scoped mount's staging prelude (design §5.1; runs ONLY when the
/// writer scope is engaged, so un-scoped solo mounts are byte-identical
/// to prior releases):
///
/// 1. take the liveness lock on every OWN staging root;
/// 2. refuse LOUD on an exact-pair LIVE sibling (OQ-5's resolved form —
///    an observed mount-slot collision within one claim set refuses the
///    mount, naming both mount points and the `-o client_slot=` remedy);
/// 3. adopt exact-pair DEAD siblings (the `-o client_slot=` adoption arm:
///    same client identity, different directory spelling) and node-only
///    tombstoned roots (the `staging adopt` verb's completion half);
/// 4. collect the foreign-slot dead residue for the caller's report.
pub async fn mount_scoped_staging_prelude(
    containers: &[PathBuf],
    own_dirs: &[PathBuf],
    mount_point: &str,
    staging_generation: &str,
) -> Result<ScopedSiblingScan> {
    use crate::writer_scope::GenerationBinding;
    let (_, Some(ours)) = crate::writer_scope::split_staging_generation(staging_generation) else {
        return Ok(ScopedSiblingScan {
            adopted: Vec::new(),
            residue: Vec::new(),
        });
    };
    for dir in own_dirs {
        // The waiting form: absorbs the predecessor daemon's exit racing a
        // zero-dwell remount (see `hold_staging_root_lock_waiting`).
        hold_staging_root_lock_waiting(dir).await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "cannot own this mount's staging root: {e}. If a previous mount at this \
                 mount point is still running, unmount it first",
            ))
        })?;
    }
    let mut scan = ScopedSiblingScan {
        adopted: Vec::new(),
        residue: Vec::new(),
    };
    for container in containers {
        let Ok(entries) = std::fs::read_dir(container) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !entry.metadata().map(|m| m.is_dir()).unwrap_or(false)
                || entry.file_name() == "cache_segment"
                || own_dirs.contains(&dir)
            {
                continue;
            }
            let Ok(Some(marker)) = crate::cache::read_staging_generation_marker(&dir).await else {
                continue;
            };
            match crate::writer_scope::classify_generation(&marker, staging_generation, Some(ours))
            {
                GenerationBinding::Match => {
                    if staging_root_owner_is_live(&dir) {
                        // OQ-5's resolved form: two LIVE mounts of one
                        // claim set presenting ONE client identity.
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "MOUNT-SLOT COLLISION (design-full-multi-writer §5.1, OQ-5): this \
                             mount at '{mount_point}' presents writer scope {} but a LIVE mount \
                             already owns staging root {} under the same identity (the root's \
                             directory name is that mount's sanitized mount point). Two mount \
                             points of one claim set derived — or were given — one mount slot; \
                             client identity must stay injective. Remedy: give one of them a \
                             distinct explicit slot with `-o client_slot=<hex8>`",
                            ours.render(),
                            dir.display(),
                        )));
                    }
                    log::warn!(
                        "ADOPTING staging root {} (design §5.1): its marker carries exactly this \
                         mount's writer scope {} — the `-o client_slot=` adoption arm (or a \
                         spelling variant of this mount point). Its staged custody recovers as \
                         OURS through the normal mount-time recovery scan",
                        dir.display(),
                        ours.render(),
                    );
                    hold_staging_root_lock(&dir)?;
                    scan.adopted.push(dir);
                }
                GenerationBinding::ScopeUpgrade => {
                    // Only a marker that CARRIES the node-only scope AND
                    // the `staging adopt` tombstone is bindable here (see
                    // STAGING_NODE_ADOPTED_MARKER on why bare node-only
                    // roots are never auto-bound).
                    let (_, marker_scope) = crate::writer_scope::split_staging_generation(&marker);
                    let tombstone = dir.join(STAGING_NODE_ADOPTED_MARKER);
                    if marker_scope.is_some_and(|m| m.is_node_only())
                        && tombstone.exists()
                        && !staging_root_owner_is_live(&dir)
                    {
                        match try_staging_root_lock(&dir, true) {
                            Ok(Some(file)) => {
                                STAGING_ROOT_LOCKS
                                    .lock()
                                    .expect("staging root lock registry poisoned")
                                    .push(file);
                                let _ = std::fs::remove_file(&tombstone);
                                log::warn!(
                                    "ADOPTING node-adopted staging root {} (design §5.1(b)): \
                                     `squeezefs staging adopt` re-bound it to this node and this \
                                     mount won its liveness lock — binding it; its durable \
                                     staged payloads recover as ours",
                                    dir.display(),
                                );
                                scan.adopted.push(dir);
                            }
                            // Lost the race to a co-located sibling (or the
                            // lock is unreadable): theirs, not ours.
                            _ => {}
                        }
                    }
                }
                GenerationBinding::ForeignScope(m)
                    if m.node == ours.node && m.slot != 0 && !staging_root_owner_is_live(&dir) =>
                {
                    let live_units = crate::cache::scan_live_staged_custody(&dir, 8)
                        .await
                        .unwrap_or_default();
                    if !live_units.is_empty() {
                        scan.residue.push(SlotResidue {
                            dir,
                            scope: m,
                            live_units,
                            live_owner: false,
                        });
                    }
                }
                // Live co-located siblings, other nodes' roots, foreign
                // sets, un-scoped roots: not ours to touch or report.
                _ => {}
            }
        }
    }
    Ok(scan)
}

/// Render the MW-1b residue report (design §5.1(a)): LOUD, repeated on
/// every mount until resolved, naming each residue slot and the exact
/// remedy string. `None` when there is nothing to report.
pub fn residue_report(residue: &[SlotResidue]) -> Option<String> {
    if residue.is_empty() {
        return None;
    }
    let mut out = format!(
        "MOVED-MOUNT-POINT STAGING RESIDUE (design-full-multi-writer §5.1, crash window \
         MW-1b): {} staging root(s) on this node hold LIVE staged write custody under this \
         volume set and node but a FOREIGN mount slot — acked staged work, possibly awaiting \
         writeback, stranded by a mount-point move. Nothing is adopted or discarded \
         automatically, and this report repeats on EVERY mount until it is resolved:",
        residue.len()
    );
    for r in residue {
        out.push_str(&format!(
            "\n  slot m{slot:08x} at {dir}: {n} live staged unit(s), e.g. {units:?}\n    \
             remedy: remount at the original path, or mount with -o client_slot={slot:08x} to \
             adopt; or `squeezefs staging adopt --slot {slot:08x} <sqmeta-uri>` (re-bind the \
             residue to this node) / `squeezefs staging discard --slot {slot:08x} \
             <sqmeta-uri>` (destroy it)",
            slot = r.scope.slot,
            dir = r.dir.display(),
            n = r.live_units.len(),
            units = r.live_units,
        ));
    }
    Some(out)
}

/// The `squeezefs clients` residue arm: enumerate THIS NODE's
/// slot-decorated staging roots for the set (probe-only — no guard, no
/// writes), so an operator can LIST the slot value a `-o client_slot=`
/// remedy needs (design §5.1: "residue-holding dead client slots").
pub async fn slot_residue_for_set(meta_lvs: &[String]) -> Result<Vec<SlotResidue>> {
    let Some(scope) = crate::writer_scope::resolve_scope_for_set(meta_lvs).await? else {
        return Ok(Vec::new());
    };
    let set = crate::meta_backend::volume_set_generation(meta_lvs).await?;
    let cfg = read_format_config(&meta_lvs[0]).await?;
    let containers: Vec<PathBuf> = cfg
        .disk_cache_paths
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|d| d.join(&cfg.name))
        .collect();
    Ok(find_slot_roots(&containers, &[], &set, scope.node, None).await)
}

/// Shared preamble of the `staging adopt|discard` verbs: the live-client
/// gate on every volume (the D0-guarded offline-verb posture — the
/// `set-cache-paths` pattern), scope + generation resolution, and the
/// residue-root lookup for `slot`. Refuses loud when the set is not
/// writer-scoped, has no staging paths, no roots match, or any matching
/// root has a LIVE owner.
async fn staging_verb_roots(
    meta_lvs: &[String],
    slot: u32,
) -> Result<(String, crate::writer_scope::WriterScope, Vec<SlotResidue>)> {
    if meta_lvs.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "no metadata volumes named".to_string(),
        ));
    }
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true).await?;
    }
    let Some(scope) = crate::writer_scope::resolve_scope_for_set(meta_lvs).await? else {
        return Err(SqueezefsError::InvalidOperation(
            "this volume set is not writer-scoped (incompat bit 10 is not stamped on every \
             volume), so no mount-slot staging residue can exist for it"
                .to_string(),
        ));
    };
    let set = crate::meta_backend::volume_set_generation(meta_lvs).await?;
    let cfg = read_format_config(&meta_lvs[0]).await?;
    let containers: Vec<PathBuf> = cfg
        .disk_cache_paths
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|d| d.join(&cfg.name))
        .collect();
    if containers.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "this filesystem is cache-less (no staging paths in its format config) — it \
             carries no staging residue"
                .to_string(),
        ));
    }
    let roots = find_slot_roots(&containers, &[], &set, scope.node, Some(slot)).await;
    if roots.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "no staging root on this node is bound to slot m{slot:08x} of this volume set and \
             node (searched {} container(s): {containers:?}). `squeezefs clients` lists the \
             residue slots this node holds",
            containers.len(),
        )));
    }
    if let Some(live) = roots.iter().find(|r| r.live_owner) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "staging root {} (slot m{slot:08x}) is owned by a LIVE mount — refusing to touch a \
             live client's staging root. Unmount it first",
            live.dir.display(),
        )));
    }
    Ok((set, scope, roots))
}

/// `squeezefs staging adopt --slot <hex8> <sqmeta-uri>` (design §5.1(b)):
/// re-bind slot `slot`'s residue roots to the invoking identity — the
/// NODE (a CLI process has no mount point, so its identity is the
/// node-only scope) — via the KD-8 two-phase rebind machinery, D0-guarded.
///
/// KD-8's own refusal law carries over verbatim: PENDING write custody
/// (`active_block:` / `active_block_ext:` records) refuses the rebind,
/// because those records' keys carry the DEAD slot's scope and a root
/// rebind cannot re-key them — the drain path for live custody is a mount
/// with `-o client_slot=<hex8>` (writeback drains it, then this verb or a
/// clean unmount leaves nothing behind). Durable staged payloads (uuid
/// file ids, unscoped keys) rebind and are recovered by the next scoped
/// mount of this set on this node, which finds the re-bound root through
/// its tombstone ([`STAGING_NODE_ADOPTED_MARKER`]) and wins it by
/// liveness lock.
pub async fn staging_adopt(meta_lvs: &[String], slot: u32) -> Result<Vec<PathBuf>> {
    let (set, scope, roots) = staging_verb_roots(meta_lvs, slot).await?;
    let old_gen = crate::writer_scope::staging_generation(
        &set,
        Some(crate::writer_scope::WriterScope::new(scope.node, slot)),
    );
    let new_gen = crate::writer_scope::staging_generation(&set, Some(scope));
    let dirs: Vec<PathBuf> = roots.iter().map(|r| r.dir.clone()).collect();
    staging_rebind_prepare(&dirs, &old_gen, &new_gen)
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "{e}. For MOVED-MOUNT-POINT residue the drain mount is `mount … -o \
                 client_slot={slot:08x}` — it adopts the residue as its own identity, and \
                 writeback drains the pending custody"
            ))
        })?;
    staging_rebind_finalize(&dirs, &new_gen).await?;
    for dir in &dirs {
        // The tombstone that authorizes the next mount to bind this root.
        crate::uring_fs::write_all(
            &dir.join(STAGING_NODE_ADOPTED_MARKER),
            format!(
                "adopted-to-node from slot m{slot:08x} at unix {}\n",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )
            .into_bytes(),
        )
        .await?;
    }
    Ok(dirs)
}

/// `squeezefs staging discard --slot <hex8> <sqmeta-uri>` (design
/// §5.1(b)): DESTROY slot `slot`'s residue roots — the stale-token
/// orphan-discard posture made an explicit operator act, with the freed
/// keys enumerated (the attested-verb loudness class: the caller prints
/// every key and every directory this returned). D0-guarded; refuses a
/// live owner's root.
pub async fn staging_discard(
    meta_lvs: &[String],
    slot: u32,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let (_, _, roots) = staging_verb_roots(meta_lvs, slot).await?;
    let mut freed_keys = Vec::new();
    let mut dirs = Vec::new();
    for r in &roots {
        // Enumerate EVERY live staged custody key before destruction —
        // never destroy silently (the attestation half of the verb).
        let keys = crate::cache::scan_live_staged_custody(&r.dir, usize::MAX)
            .await
            .unwrap_or_default();
        freed_keys.extend(keys);
        std::fs::remove_dir_all(&r.dir).map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                e.kind(),
                format!("destroying residue root {}: {e}", r.dir.display()),
            ))
        })?;
        dirs.push(r.dir.clone());
    }
    Ok((dirs, freed_keys))
}

// ===========================================================================
// `squeezefs volume enable-symmetric` — the OFFLINE conversion of a
// flat metadata set into the slot-tree forest (design-symmetric-metadata
// §6.2, §7.1–§7.3; PR 11).
// ===========================================================================

/// [`SymUpgradeMarker`] wire version (forward-only: anything else refuses
/// loud, the [`crate::MwUpgradeMarker`] law).
pub const SYM_UPGRADE_MARKER_VERSION: u8 = 1;

/// The `sym_upgrade:` conversion marker's content: the canonical volume
/// list the running-or-crashed conversion covers, so a resume can verify
/// it is completing the SAME act. Written on ino 1 of EVERY volume of the
/// set as the verb's first act; each volume's copy is deleted as that
/// volume's last act. The record carries no cursor: every intermediate
/// state of a volume's conversion is decided from the volume's own
/// durable structures (bit 17, the ledger's roots, the marker's presence
/// — [`enable_symmetric_with`]), never from a progress field a crash
/// could leave stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymUpgradeMarker {
    /// The set's volume paths in canonical member order (volume 0 first).
    pub volumes: Vec<String>,
}

impl SymUpgradeMarker {
    /// `version u8 | count u16 LE | (len u16 LE | bytes) × volumes |
    /// xxh3-64 LE of everything before` (the mw marker's shape without
    /// its bit word). Refuses a volume count or a path the `u16` fields
    /// cannot carry rather than truncating them.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let too_wide = |what: &str, n: usize| {
            SqueezefsError::InvalidOperation(format!(
                "sym_upgrade marker: {what} of {n} does not fit the record's u16 field"
            ))
        };
        let count = u16::try_from(self.volumes.len())
            .map_err(|_| too_wide("a volume count", self.volumes.len()))?;
        let mut out =
            Vec::with_capacity(1 + 2 + self.volumes.iter().map(|v| 2 + v.len()).sum::<usize>() + 8);
        out.push(SYM_UPGRADE_MARKER_VERSION);
        out.extend_from_slice(&count.to_le_bytes());
        for v in &self.volumes {
            let b = v.as_bytes();
            let len = u16::try_from(b.len()).map_err(|_| too_wide("a path length", b.len()))?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(b);
        }
        let sum = xxhash_rust::xxh3::xxh3_64(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        Ok(out)
    }

    /// Decode + verify; torn, truncated and future-version images refuse
    /// loud (presence alone is the mount gate's predicate).
    pub fn decode(raw: &[u8]) -> std::result::Result<Self, String> {
        fn le_u16(b: &[u8], at: usize) -> std::result::Result<usize, String> {
            b.get(at..at + 2)
                .map(|s| usize::from(u16::from_le_bytes([s[0], s[1]])))
                .ok_or_else(|| "sym_upgrade marker truncated inside a length field".to_string())
        }
        if raw.len() < 1 + 2 + 8 {
            return Err(format!(
                "sym_upgrade marker too short ({} B) — torn or foreign",
                raw.len()
            ));
        }
        let (body, sum_bytes) = raw.split_at(raw.len() - 8);
        let mut sum = [0u8; 8];
        sum.copy_from_slice(sum_bytes);
        if xxhash_rust::xxh3::xxh3_64(body) != u64::from_le_bytes(sum) {
            return Err(
                "sym_upgrade marker checksum mismatch — torn write or corruption".to_string(),
            );
        }
        if body[0] != SYM_UPGRADE_MARKER_VERSION {
            return Err(format!(
                "sym_upgrade marker version {} is not the supported version {} — a newer \
                 binary began this conversion; finish it with that binary",
                body[0], SYM_UPGRADE_MARKER_VERSION
            ));
        }
        let count = le_u16(body, 1)?;
        let mut pos = 3usize;
        let mut volumes = Vec::with_capacity(count);
        for _ in 0..count {
            let len = le_u16(body, pos)
                .map_err(|_| "sym_upgrade marker truncated before a volume entry".to_string())?;
            pos += 2;
            let entry = body
                .get(pos..pos + len)
                .ok_or_else(|| "sym_upgrade marker truncated inside a volume entry".to_string())?;
            volumes.push(
                std::str::from_utf8(entry)
                    .map_err(|_| "sym_upgrade marker volume entry is not UTF-8".to_string())?
                    .to_string(),
            );
            pos += len;
        }
        if pos != body.len() {
            return Err("sym_upgrade marker carries trailing bytes — torn or foreign".to_string());
        }
        Ok(Self { volumes })
    }
}

/// `squeezefs volume enable-symmetric` flags.
#[derive(Debug, Clone, Default)]
pub struct EnableSymOptions {
    /// Print the plan and every refusal; write nothing.
    pub dry_run: bool,
    /// Continue a crashed run (a marker present on any volume refuses a
    /// plain run — a crash must be acknowledged).
    pub resume: bool,
    /// Undo a crashed run on every volume still FLAT under its marker:
    /// the marker is removed and any extents its aborted build claimed
    /// are reclaimed, leaving the volume as it was before the verb. A
    /// volume already past its flip (stamped, marker present) refuses —
    /// the forest is that volume's truth and `--resume` its only exit.
    pub abort: bool,
}

/// The conversion's crash seams: each injects a hard error AFTER the named
/// durable write, so the on-media state is exactly the kill-9 window's
/// (the [`EnableMwCrash`] pattern). `volume` indexes the canonical order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnableSymCrash {
    /// Every volume's marker landed; no volume converted.
    AfterMarker,
    /// The forest nodes, tree 0, the directory extent, appender 0's page
    /// and the bitmap claiming them are durable; the ledger still names
    /// the flat roots only (the orphaned-build window — the resume
    /// reclaims it).
    AfterBuild { volume: usize },
    /// The hybrid ledger (flat roots + forest roots) is durable; bit 17
    /// is not stamped (the flat layout is still current).
    AfterLedger { volume: usize },
    /// Bit 17 + the appender directory are stamped; the old trees'
    /// extents are still claimed; the marker is present.
    AfterStamp { volume: usize },
    /// The old trees are freed and folded out of the ledger; the marker
    /// is present (the last window before the marker's removal).
    AfterFree { volume: usize },
}

/// Test seams for [`enable_symmetric_with`].
#[derive(Default)]
pub struct EnableSymHooks {
    pub crash_after: Option<EnableSymCrash>,
}

/// What the verb did to one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionOutcome {
    /// Converted by this invocation from a flat volume.
    Converted,
    /// A crashed run's volume completed by this `--resume` invocation.
    Resumed,
    /// Already carried bit 17 with no marker — skipped.
    AlreadySymmetric,
    /// `--dry-run`: planned, nothing written.
    Planned,
    /// `--abort`: a flat volume's marker removed and its aborted build's
    /// extents reclaimed — the volume as before the verb.
    Aborted,
    /// `--abort`: a flat volume the crashed run never marked — untouched.
    Untouched,
}

/// One volume's row of an [`EnableSymReport`].
#[derive(Debug, Clone)]
pub struct VolumeConversionRow {
    pub path: String,
    pub outcome: ConversionOutcome,
    /// Records moved (planned, for a dry run).
    pub records: u64,
    /// Their key + value bytes.
    pub bytes: u64,
    /// Slot trees the forest holds (native included).
    pub slot_trees: u64,
    /// The capacity preflight's PEAK CLAIM: the heap extents the build
    /// claims while the shared trees stay claimed (Σ planned nodes + the
    /// directory extent) — the plan the tie contract compares against
    /// `extents_written`.
    pub extents_needed: u64,
    /// What the preflight found claimable for the build: free extents
    /// plus the orphans its census reclaims, less the compaction floor.
    pub extents_available: u64,
    /// Heap extents the forest occupies (nodes + the directory extent).
    pub extents_written: u64,
    /// Heap extents the emptied shared trees returned.
    pub extents_freed: u64,
    /// Claimed-but-unreachable extents reclaimed before the build (a
    /// crashed build's orphans, or the shipped root-swap leak class).
    pub orphans_reclaimed: u64,
    /// Wall seconds of the conversion pass.
    pub secs: f64,
}

impl VolumeConversionRow {
    fn new(path: &str, outcome: ConversionOutcome) -> Self {
        Self {
            path: path.to_string(),
            outcome,
            records: 0,
            bytes: 0,
            slot_trees: 0,
            extents_needed: 0,
            extents_available: 0,
            extents_written: 0,
            extents_freed: 0,
            orphans_reclaimed: 0,
            secs: 0.0,
        }
    }

    fn with_plan(mut self, plan: Option<&BuildPlan>) -> Self {
        if let Some(p) = plan {
            self.records = p.records;
            self.bytes = p.bytes;
            self.slot_trees = p.slot_trees;
            self.extents_needed = p.needed;
            self.extents_available = p.available;
            self.orphans_reclaimed = p.orphans;
        }
        self
    }
}

/// What [`enable_symmetric`] did.
#[derive(Debug, Clone)]
pub struct EnableSymReport {
    /// The set's volumes in canonical member order.
    pub volumes: Vec<String>,
    pub rows: Vec<VolumeConversionRow>,
    /// Shipped-shape state the forest gives no meaning to (§7.3):
    /// `volume set-owners` assignments and solo bit-8 partition records —
    /// decodable, reported, never written under bit 17.
    pub dropped: Vec<String>,
}

/// `squeezefs volume enable-symmetric <sqmeta-uri>` — the OFFLINE
/// conversion of every volume of a set from the three shared per-kind
/// trees to the slot-tree forest (design-symmetric-metadata §6.2 / §7.1;
/// the `enable-multi-writer` posture: live-client preflight on every
/// volume, the D0 ladder asserted per volume, whole-set in one
/// invocation, crash-resumable).
///
/// **Everything that can refuse runs BEFORE the first marker, and the
/// marker is the verb's first write.** The live-client gate, the set
/// discovery, the verb's own D0 flock on every volume (held for the
/// verb's duration, released only around its guarded opens — a
/// concurrent invocation refuses at Layer A instead of racing the
/// inspection into the same extents), then one write-free inspection
/// per volume through a probe: the layout bit and the marker, the
/// membership stamp, the bit-8 partition record, the replay window,
/// the open cross-volume intents (a set the field left as a crash did —
/// a window past the tail, an open intent, a non-solo partition record
/// — is QUIESCED by the verb itself first, the mount's own crash
/// recovery through the routed writer door under the conversion's
/// admission: since PR 14 that door refuses the pre-flip class
/// presence-required for everyone else; `--dry-run` names the quiesce it
/// would run and writes nothing) and in-flight jobs,
/// the record collection (the codec's slot-range refusals fire here),
/// the bitmap-vs-reachability census and the **capacity preflight**
/// (the build plan, `extents_needed` / `extents_available` on the row):
/// the build's peak claim, computed by the tree writer's own chunking,
/// against what the volume can supply. A set that
/// passes is a set the build cannot leave stranded under its markers by
/// a refusal this binary could have made first.
///
/// Per volume, in canonical order, the durable steps and the state each
/// leaves (every one is what a `kill -9` right after it leaves; the
/// resume decides where it is from these alone):
///
/// 1. **the marker** — `sym_upgrade:` on ino 1 (every volume, before any
///    conversion), journal-committed + checkpointed under the volume's
///    guarded open. From here every writable open but the verb's refuses
///    at the D0 gate BEFORE its claim, so no refused mount rewrites the
///    ledger under the conversion; readers and probes serve the trees
///    the layout bit names.
/// 2. **the build** — the volume is quiesced (guarded open → checkpoint →
///    clean shutdown ⇒ an EMPTY replay window, asserted), read through a
///    probe (every live record of every kind, folded), and the forest is
///    written into FRESH extents: one mixed-kind slot tree per slot the
///    records name — and one EMPTY slot tree per hosted slot whose stamp
///    carries a cursor but whose records were all deleted, so tree 0 is
///    the cursor's durable home without the stamp (§5.1.8) — tree 0
///    naming every guest root `Unleased { root, cursor: max(stamp cursor,
///    max live ino + 1), g: 0 }`, the appender directory extent, the
///    fixed ring zeroed and appender 0's `Free` page in its first slot
///    (§5.3.1); every record is written at seq 0 (checkpoint-covered by
///    construction, the format law). The ring's seq space is kept: both
///    layouts resume at the quiesced tail `T` over the zeros, so a flat
///    write in the window before the stamp still sorts above the flat
///    trees' records. The bitmap claiming the new extents lands at a
///    new generation. Before
///    the build, every claimed extent no ledger root reaches is
///    RECLAIMED (a crashed build's orphans — the volume is quiesced with
///    an empty window, so reachability from the ledger's roots is exact)
///    and the writer's node-seq floor is raised above every stamp those
///    extents' residue carries ([`crate::meta_backend::kv::node::residue_seq_ceiling`]).
/// 3. **the hybrid ledger** — ONE 4 KiB checksummed slot write naming the
///    flat roots AND tree 0 + the native slot tree, the quiesced tail, the
///    new bitmap generation. A flat open still finds its roots (and reads
///    the zeroed ring as empty — the appender page in page 0 is not a
///    journal page); nothing references the forest yet but this record.
/// 4. **the stamp** — bit 17 + `appender_dir` in one sector write: the
///    volume's layout flips to the forest, which the same ledger already
///    describes. The marker rode the build into the native slot tree, so
///    a forest open finds it and writers keep refusing.
/// 5. **the free** — the old trees' nodes (reachable from the flat roots
///    the hybrid ledger still names; nothing else can reach them) are
///    released, the bitmap written, and a ledger record naming the forest
///    roots ALONE folds them out (§4.7's protocol collapsed to its
///    offline form: the ledger write is the atomic point, and until it
///    lands the old roots are still named and the walk is repeatable).
/// 6. **the marker's removal** — through the forest's own guarded open
///    (its join, checkpoint and leave), the volume's last act. The marker
///    outlives the stamp by construction: windows 4 and 5 refuse writers.
///
/// Two CLASSES of crash window are stated and pinned at five seams:
/// before the stamp (after the markers, the build, the hybrid ledger) the
/// volume is a FLAT volume whose bitmap and ledger may also name an
/// unreferenced forest (the resume rebuilds — the census reclaims the
/// forest as orphans and the hybrid record is overwritten; `--abort`
/// reclaims it and removes the marker instead); after it (after the
/// stamp, after the free) it is a FOREST whose ledger may still name the
/// old trees (the resume frees them, or finds them already folded out,
/// then removes the marker; `--abort` refuses — the forest is the truth).
pub async fn enable_symmetric(
    meta_lvs: &[String],
    opts: &EnableSymOptions,
) -> Result<EnableSymReport> {
    enable_symmetric_with(meta_lvs, opts, &EnableSymHooks::default()).await
}

/// The verb's D0 Layer-A hold on every volume of the set: the same
/// `flock(LOCK_EX | LOCK_NB)` on the device node a writer open takes
/// ([`crate::meta_backend::kv::backend::KvMetaBackend::open`]'s guard), so
/// a concurrent invocation — or a mount that began after the live-client
/// gate — refuses at Layer A instead of racing the inspection and building
/// into the same extents. Held for the verb's duration; released around
/// the verb's OWN guarded opens (a second open file description in this
/// process conflicts with its own hold) and re-taken the moment they
/// close, so the unguarded window is the open's own.
struct VerbFlocks {
    paths: Vec<String>,
    held: Vec<Option<std::fs::File>>,
}

impl VerbFlocks {
    fn acquire(paths: &[String]) -> Result<Self> {
        let mut out = Self {
            paths: paths.to_vec(),
            held: Vec::with_capacity(paths.len()),
        };
        for vi in 0..paths.len() {
            out.held.push(None);
            out.take(vi)?;
        }
        Ok(out)
    }

    fn take(&mut self, vi: usize) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let path = &self.paths[vi];
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric: cannot open {path} for the writer lock: {e}"
                ))
            })?;
        // SAFETY: valid owned fd; LOCK_NB never blocks.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric refused: {path}: another process holds the writer \
                     lock — a concurrent `volume enable-symmetric`, or a mount that began after \
                     the live-client gate; one invocation converts a set at a time \
                     (single-writer guard). Retry when it has finished"
                ))
            } else {
                SqueezefsError::Io(err)
            });
        }
        self.held[vi] = Some(file);
        Ok(())
    }

    /// Release volume `vi`'s hold for one of the verb's guarded opens.
    fn release(&mut self, vi: usize) {
        self.held[vi] = None;
    }

    /// Re-take volume `vi`'s hold after the guarded open closed. A holder
    /// that slipped into the open's window is given the same bounded
    /// wait a mount grants a transient flock holder before the refusal
    /// ([`VERB_FLOCK_RETAKE_WAIT`]), so the ms-grade collision — a mount
    /// that took Layer A inside the window and refuses at the marker gate
    /// within it — never aborts the verb post-marker.
    async fn retake(&mut self, vi: usize) -> Result<()> {
        let deadline = std::time::Instant::now() + VERB_FLOCK_RETAKE_WAIT;
        loop {
            match self.take(vi) {
                Ok(()) => return Ok(()),
                Err(e) if self.held_elsewhere(&e) && std::time::Instant::now() < deadline => {
                    squeezefs_ipc::sqz_time::sleep(VERB_FLOCK_RETAKE_POLL).await;
                }
                Err(e) => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "{e} (the lock was released for the verb's own guarded open and taken \
                         by another process before it could be re-taken within {} s; re-run with \
                         `--resume` once the holder has finished)",
                        VERB_FLOCK_RETAKE_WAIT.as_secs()
                    )))
                }
            }
        }
    }

    /// Whether a [`Self::take`] error is the busy class (a live holder),
    /// as opposed to an I/O failure that no wait cures.
    fn held_elsewhere(&self, e: &SqueezefsError) -> bool {
        matches!(e, SqueezefsError::InvalidOperation(msg) if msg.contains("holds the writer lock"))
    }
}

/// The bounded window [`VerbFlocks::retake`] polls a holder for — the
/// same 2 s `KvMetaBackend::open` grants a transient flock holder before
/// its own refusal (`backend.rs` `TRANSIENT_FLOCK_WAIT`; that constant is
/// private to PR 4's file — make it `pub` and tie the two at the rebase).
const VERB_FLOCK_RETAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// The retake's poll period (the mount's own transient-wait poll).
const VERB_FLOCK_RETAKE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// The capacity preflight of one flat volume — computed from the
/// write-free inspection, BEFORE the first marker.
struct BuildPlan {
    records: u64,
    bytes: u64,
    slot_trees: u64,
    /// The extents of the shared trees (claimed until step 5).
    old_extents: u64,
    /// Claimed extents no ledger root reaches (the census credit).
    orphans: u64,
    /// The peak claim: Σ planned nodes over every slot tree and tree 0,
    /// plus the directory extent.
    needed: u64,
    /// `free + orphans − compaction_floor(reserve)`.
    available: u64,
    /// The bitmap's free extents (the refusal text's term).
    free: u64,
    /// The §4.7 compaction floor in extents (the refusal text's term).
    floor: u64,
    /// One extent, in KiB (the refusal text's term).
    extent_kib: u64,
}

/// One volume's durable state as the verb finds it (write-free).
struct SymInspection {
    symmetric: bool,
    /// The volume is NOT of the multi-writer class (a `--single-writer`
    /// format): the forest presumes the nine bits (the join ladder's rung
    /// 2), so the conversion refuses naming `enable-multi-writer` first.
    single_writer_class: bool,
    marker: Option<SymUpgradeMarker>,
    /// The newest ledger record carries a NON-SOLO bit-8 partition.
    partition_non_solo: bool,
    /// The newest ledger record carries a solo partition suffix (dropped).
    partition_solo_record: bool,
    /// The newest ledger record carries no membership stamp.
    stamp_absent: bool,
    /// Journal entries past the ledger's checkpoint tail the probe
    /// replayed — nonzero means the volume was not cleanly unmounted.
    window_entries: u64,
    open_intents: Vec<u64>,
    /// In-flight (non-terminal) `job:` records, by id.
    jobs_in_flight: Vec<String>,
    /// The `volume set-owners` assignment, if any (dropped).
    owner: Option<String>,
    /// The build plan (flat volumes only).
    plan: Option<BuildPlan>,
}

async fn inspect_for_symmetric(path: &str, ordered: &[String]) -> Result<SymInspection> {
    use crate::meta_backend::kv::alloc_ext::{
        compaction_floor_extents, compaction_reserve_extents, ExtentAllocator,
    };
    use crate::meta_backend::kv::backend::{KvMetaBackend, PENDING_FREE_CAP};
    use crate::meta_backend::kv::builder::TreeWriter;
    use crate::meta_backend::kv::checkpoint::read_newest_ledger;
    use crate::meta_backend::kv::node::NodeLayout;
    use crate::meta_backend::kv::record::{
        forest_key, xattr_key, xattr_name_hash56, Record, XattrValue, TREE_XATTRS,
    };
    use crate::meta_backend::kv::superblock::{classify_volume, VolumeFormat};

    let p = Path::new(path);
    let sb = match classify_volume(p).await? {
        VolumeFormat::V3(sb) => sb,
        VolumeFormat::Blank => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} is not formatted"
            )))
        }
        VolumeFormat::V2Legacy => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} is a legacy format-v2 volume — v2 support was \
                 removed; reformat required"
            )))
        }
    };
    let ledger = read_newest_ledger(p, sb.root_ledger.start)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} has no valid root-ledger record"
            ))
        })?;
    let partition_non_solo = ledger.append_partition.is_some_and(|part| !part.is_solo());
    let partition_solo_record = ledger.append_partition.is_some_and(|part| part.is_solo());
    let mut out = SymInspection {
        symmetric: sb.symmetric_forest_stamped(),
        single_writer_class: !sb.multi_writer_class(),
        marker: None,
        partition_non_solo,
        partition_solo_record,
        stamp_absent: ledger.membership_stamp.is_none(),
        window_entries: 0,
        open_intents: Vec::new(),
        jobs_in_flight: Vec::new(),
        owner: None,
        plan: None,
    };
    if partition_non_solo || out.stamp_absent {
        // Refused below without a probe: a partitioned era's slot
        // arithmetic is not a solo mount's, and a stamp-less volume is
        // not a dynamic-routing set member.
        return Ok(out);
    }
    let be = KvMetaBackend::open_probe(p).await?;
    out.window_entries = be.replay_stats().entries;
    if let Some(raw) = be.getxattr(1, crate::SYM_UPGRADE_MARKER_XATTR).await? {
        let marker = SymUpgradeMarker::decode(&raw).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: the crashed run's marker on {path} is unusable \
                 ({e}) — refusing to guess the target set"
            ))
        })?;
        out.marker = Some(marker);
    }
    out.open_intents = be
        .xv_scan_intents()
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for name in be.listxattr(1).await? {
        let Some(rest) = name.strip_prefix(crate::jobs::JOB_XATTR_PREFIX) else {
            continue;
        };
        if rest.contains(':') || name == crate::job_wire::JOB_ENROLL_XATTR {
            continue; // a shard/progress sub-record, or the enrollment secret
        }
        let Some(bytes) = be.getxattr(1, &name).await? else {
            continue;
        };
        match serde_json::from_slice::<crate::jobs::JobRecord>(&bytes) {
            Ok(rec) if rec.state.is_terminal() => {}
            Ok(rec) => out.jobs_in_flight.push(rec.job_id),
            // Never guess about a record that may be in flight.
            Err(e) => out
                .jobs_in_flight
                .push(format!("{rest} (undecodable: {e})")),
        }
    }
    out.owner = crate::membership::ClaimSet::load(&be)
        .await
        .filter(|s| s.durable)
        .and_then(|s| s.owner);
    if out.symmetric {
        return Ok(out);
    }

    // ---- The capacity preflight (a flat volume) ------------------------
    let stamp = ledger
        .membership_stamp
        .as_ref()
        .ok_or_else(|| SqueezefsError::InvalidOperation("stamp checked above".to_string()))?;
    let mut flat = collect_flat_records(&be, stamp).await?;
    if out.marker.is_none() {
        // The marker the verb is about to write rides the build as an
        // ino-1 xattr record of the native slot: plan it, so the plan is
        // the build's record set and not one record short.
        let marker = SymUpgradeMarker {
            volumes: ordered.to_vec(),
        };
        let key = xattr_key(
            1,
            xattr_name_hash56(crate::SYM_UPGRADE_MARKER_XATTR.as_bytes(), sb.hash_seed),
            0,
        );
        let value = XattrValue {
            name: crate::SYM_UPGRADE_MARKER_XATTR.as_bytes().to_vec(),
            value: marker.encode()?,
        }
        .encode()?;
        let fkey = forest_key(TREE_XATTRS, &key)?;
        flat.records += 1;
        flat.bytes += (fkey.len() + value.len()) as u64;
        flat.by_slot
            .entry(crate::meta_backend::kv::record::NATIVE_FOREST_SLOT)
            .or_default()
            .push(Record::put(fkey, 0, value));
    }
    // The forest is written with the v2 stamped frame (bit 17's frame,
    // design-symmetric-metadata §5.8.2), so the plan prices the build's
    // own geometry — the tie `extents_needed ≡ extents_written` holds
    // only if both read one layout.
    let layout = NodeLayout::new_symmetric(sb.node_size as usize)?;
    let slots = forest_slots_of(&flat, stamp);
    let mut planned_nodes = 0u64;
    for slot in &slots {
        let mut records = flat.by_slot.remove(slot).unwrap_or_default();
        records.sort_by(|a, b| a.key.cmp(&b.key));
        planned_nodes += TreeWriter::planned_nodes(&layout, &records);
    }
    let control = control_records_planned(&slots);
    planned_nodes += TreeWriter::planned_nodes(&layout, &control);

    // The census: old extents (the shared trees' reachable nodes) and the
    // orphans (claimed, reachable from no root).
    let mut reachable = std::collections::BTreeSet::new();
    for tree in be.all_trees() {
        for addr in tree.reachable_node_addrs().await? {
            reachable.insert(extent_of(&sb, addr)?);
        }
    }
    let total = sb.total_extents();
    let reserve = compaction_reserve_extents(total);
    let alloc = ExtentAllocator::load(
        p,
        sb.alloc_bitmap.start,
        total,
        reserve,
        PENDING_FREE_CAP,
        ledger.journal_tail_seq,
        &[],
    )
    .await?;
    // The census credit is exact only over an EMPTY replay window (an
    // in-window SMO's successor is claimed and reachable from no
    // checkpointed root); a marked volume's window is emptied by the
    // build's own quiesce, so its plan takes no credit here.
    let orphans = if out.window_entries == 0 {
        (0..total)
            .filter(|e| alloc.is_allocated(*e) && !reachable.contains(e))
            .count() as u64
    } else {
        0
    };
    // The peak claim: the build writes `planned_nodes` fresh nodes (one
    // heap extent each — the tree writer's own chunking, not an
    // estimate) plus the appender directory extent, while the shared
    // trees' `old_extents` stay claimed until step 5 frees them. The
    // internal class may claim down to the last extent, so the build
    // COMPLETES iff `needed ≤ free + orphans`; the preflight keeps the
    // §4.7 compaction floor out of the claim as well — it doubles as the
    // SLACK for what happens between this plan and the build: the
    // marker's and the quiesce's guarded opens commit their claim /
    // unclaim / `writer_term` records, and a leaf those split or compact
    // costs the build one or two extents (or moves one chunk boundary)
    // the pre-quiesce plan could not see; the floor (≥ 4 extents) covers
    // it, and the converted volume's first flush pass still finds it:
    //   needed    = Σ_slot nodes(slot records) + nodes(tree 0) + 1
    //   available = free + orphans − compaction_floor(reserve)
    // Tie-tested in `sym_convert_tests` (`needed ≡ extents_written`).
    let needed = planned_nodes + 1;
    let free = alloc.free_extents();
    let floor = compaction_floor_extents(reserve);
    let available = (free + orphans).saturating_sub(floor);
    out.plan = Some(BuildPlan {
        records: flat.records,
        bytes: flat.bytes,
        slot_trees: slots.len() as u64,
        old_extents: reachable.len() as u64,
        orphans,
        needed,
        available,
        free,
        floor,
        extent_kib: u64::from(sb.node_size) / 1024,
    });
    Ok(out)
}

/// The slot trees the forest holds: every slot the records name, the
/// native slot, and every hosted slot whose stamp carries a cursor
/// (§5.1.8 — a cursor is never lowered, and tree 0 is its durable home
/// even where every ino it covers was deleted). Index-ascending.
fn forest_slots_of(
    flat: &FlatRecords,
    stamp: &crate::meta_backend::kv::checkpoint::MembershipStamp,
) -> Vec<crate::meta_backend::kv::record::ForestSlot> {
    use crate::meta_backend::kv::record::{guest_forest_slot, NATIVE_FOREST_SLOT};
    let mut slots: std::collections::BTreeSet<_> = flat.by_slot.keys().copied().collect();
    slots.insert(NATIVE_FOREST_SLOT);
    for (slot, _) in &stamp.slot_cursors {
        if stamp.resolved_native_slot() != Some(*slot) {
            slots.insert(guest_forest_slot(*slot));
        }
    }
    slots.into_iter().collect()
}

/// Tree 0's records as the plan sees them: one `Unleased` record per
/// guest slot — its value is fixed-width (no tails), so the plan's
/// records encode to the build's exact lengths whatever the roots are.
fn control_records_planned(
    slots: &[crate::meta_backend::kv::record::ForestSlot],
) -> Vec<crate::meta_backend::kv::record::Record> {
    use crate::meta_backend::kv::record::{Record, NATIVE_FOREST_SLOT};
    use crate::meta_backend::kv::slot_state::{slot_state_key, SlotState};
    use crate::meta_backend::kv::tree::RootPtr;
    slots
        .iter()
        .filter(|s| **s != NATIVE_FOREST_SLOT)
        .filter_map(|slot| {
            let state = SlotState::Unleased {
                root: RootPtr { addr: 0, seq: 0 },
                cursor: 0,
                g: 0,
                slot_tree_extents: 0,
                last_written: 0,
                seq_floor: 0,
            };
            Some(Record::put(slot_state_key(*slot), 0, state.encode()))
        })
        .collect()
}

/// [`enable_symmetric`] with the crash seams exposed.
pub async fn enable_symmetric_with(
    meta_lvs: &[String],
    opts: &EnableSymOptions,
    hooks: &EnableSymHooks,
) -> Result<EnableSymReport> {
    if opts.abort && (opts.resume || opts.dry_run) {
        return Err(SqueezefsError::InvalidOperation(
            "volume enable-symmetric: `--abort` stands alone (it is neither a resume nor a plan)"
                .to_string(),
        ));
    }
    // Live-client gate on EVERY volume before anything is touched (the
    // enable-multi-writer posture; `true` = the already-formatted refusal
    // does not apply).
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("volume enable-symmetric refused: {e}"))
            })?;
    }
    let disc = crate::meta_backend::discover_meta_set(meta_lvs).await?;
    let ordered = disc.ordered_paths.clone();

    // The verb's D0 hold, BEFORE the inspection: from here a concurrent
    // invocation refuses at Layer A, so no two verbs pass the inspection
    // against the same extents.
    let mut flocks = VerbFlocks::acquire(&ordered)?;

    let mut inspections = Vec::with_capacity(ordered.len());
    for path in &ordered {
        inspections.push(inspect_for_symmetric(path, &ordered).await?);
    }

    // ---- the QUIESCE (PR 14, §7.2): a pre-flip set the field left as a
    // crash did — a replay window past the tail, an open cross-volume
    // intent, a non-solo bit-8 partition record — was told "mount it once
    // and unmount cleanly" while the mount's writer door still admitted
    // the class; since the flip that door refuses it presence-required,
    // so the verb runs that mount itself: the ordinary routed writer open
    // (the replay, the intents rolled forward, the solo re-checkpoint),
    // one checkpoint per volume, the clean leave — the mount's own crash
    // recovery, not conversion state (no marker is written; the three
    // refusals below stand as belts for what a quiesce cannot cure). A
    // dry run writes nothing and names the quiesce it would run.
    let needs_quiesce: Vec<&String> = ordered
        .iter()
        .zip(&inspections)
        .filter(|(_, i)| {
            !i.symmetric
                && i.marker.is_none()
                && (i.window_entries > 0 || !i.open_intents.is_empty() || i.partition_non_solo)
        })
        .map(|(p, _)| p)
        .collect();
    if !needs_quiesce.is_empty() && !opts.abort {
        if opts.dry_run {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric --dry-run: {needs_quiesce:?} was not cleanly \
                 unmounted (a replay window past the checkpoint tail, an open cross-volume \
                 intent, or a non-solo partition record) — a run without --dry-run QUIESCES \
                 the set first (the mount's own crash recovery: replay, intents rolled \
                 forward, one checkpoint per volume, a clean unmount; nothing of the \
                 conversion is written) and plans against the quiesced volumes; a dry run \
                 writes nothing and cannot plan against a window"
            )));
        }
        quiesce_set_for_symmetric(&ordered, &mut flocks).await?;
        inspections.clear();
        for path in &ordered {
            inspections.push(inspect_for_symmetric(path, &ordered).await?);
        }
    }

    // ---- refusals FIRST, loud, naming the remedy ----------------------
    let markers: Vec<&String> = ordered
        .iter()
        .zip(&inspections)
        .filter(|(_, i)| i.marker.is_some())
        .map(|(p, _)| p)
        .collect();
    if inspections.iter().all(|i| i.symmetric) && markers.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume enable-symmetric: the set is already symmetric — every one of its {} \
             volume(s) carries incompat bit 17 and no conversion marker; nothing to convert",
            ordered.len()
        )));
    }
    for (path, ins) in ordered.iter().zip(&inspections) {
        if let Some(m) = &ins.marker {
            if m.volumes != ordered {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric: the conversion marker on {path} names volumes \
                     {:?} — re-run the verb with exactly that set (this invocation named {:?})",
                    m.volumes, ordered
                )));
            }
        }
    }
    if opts.abort {
        return abort_conversion(&ordered, &inspections, &markers, &mut flocks).await;
    }
    if !markers.is_empty() && !opts.resume {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume enable-symmetric: a conversion marker (`{}`) is present on {:?} — a \
             previous run crashed mid-conversion and every writable mount refuses until it is \
             finished. Re-run with `--resume` to continue it from the crash point, or with \
             `--abort` to undo it on every volume still flat under its marker",
            crate::SYM_UPGRADE_MARKER_XATTR,
            markers
        )));
    }
    if opts.resume && markers.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "volume enable-symmetric --resume: nothing to resume — no volume of the set \
             carries a conversion marker (run the verb without --resume to convert a flat set)"
                .to_string(),
        ));
    }
    for (path, ins) in ordered.iter().zip(&inspections) {
        if ins.symmetric {
            continue;
        }
        if ins.partition_non_solo {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} still carries a bit-8 NON-SOLO partition \
                 record after the verb's own solo quiesce (its newest ledger record was \
                 written by a multi-appender era and the solo re-checkpoint did not replace \
                 it) — the conversion reads one appender's structures; refusing \
                 (design-symmetric-metadata §7.2)"
            )));
        }
        if ins.stamp_absent {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} carries no membership stamp — not a \
                 dynamic-routing set member (reformat required)"
            )));
        }
        if ins.single_writer_class {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} is a `--single-writer` (flat, unstamped) volume \
                 — the forest presumes the nine multi-writer format bits (design-symmetric-\
                 metadata §6.2; the join ladder's rung 2 demands them on every volume). Run \
                 `squeezefs volume enable-multi-writer <sqmeta-uri>` offline first, then re-run"
            )));
        }
        if ins.window_entries > 0 && ins.marker.is_none() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} still holds {} journal entr(ies) past its \
                 checkpoint tail after the verb's own quiesce (the routed open's replay + \
                 checkpoint + clean unmount), so its trees are not the whole truth and the \
                 census the conversion runs is not exact; refusing",
                ins.window_entries
            )));
        }
        if !ins.open_intents.is_empty() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} still carries {} open cross-volume intent(s) \
                 (tx {:?}) after the verb's own quiesce rolled the set's intents forward — \
                 converting now would strand a half-applied transaction across two layouts; \
                 refusing (a holder the roll-forward could not reach keeps the intent open)",
                ins.open_intents.len(),
                ins.open_intents
            )));
        }
        if !ins.jobs_in_flight.is_empty() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} carries {} in-flight `job:` record(s) ({:?}) \
                 — a maintenance job would resume onto a layout it did not plan for. Let the \
                 jobs finish or cancel them (`squeezefs job cancel`) and re-run",
                ins.jobs_in_flight.len(),
                ins.jobs_in_flight
            )));
        }
        if let Some(plan) = &ins.plan {
            // A dry run REPORTS the shortfall in its row (the operator's
            // instrument for the remedy); the real run refuses on it.
            if plan.needed > plan.available && !opts.dry_run {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric refused: {path} cannot hold the forest beside its \
                     shared trees — the build claims {} fresh extents ({} nodes over {} slot \
                     trees + tree 0, plus the appender directory) while the trees' {} extents \
                     stay claimed until the end, and only {} are claimable ({} free + {} \
                     reclaimable orphan(s), less the {}-extent compaction floor; one extent = \
                     {} KiB). Free space on the volume (delete files, or `squeezefs defrag \
                     --meta` to compact its nodes), move slots off it (`volume \
                     migrate-meta-slot`), or rebuild the set on larger metadata volumes (a \
                     smaller `--meta-node-kib` lowers the per-slot floor); `--dry-run` prints \
                     these numbers for every volume",
                    plan.needed,
                    plan.needed - 1,
                    plan.slot_trees,
                    plan.old_extents,
                    plan.available,
                    plan.free,
                    plan.orphans,
                    plan.floor,
                    plan.extent_kib,
                )));
            }
        }
    }

    // ---- what the forest gives no meaning to (§7.3): reported, dropped -
    let mut dropped = Vec::new();
    for (path, ins) in ordered.iter().zip(&inspections) {
        if let Some(owner) = &ins.owner {
            dropped.push(format!(
                "{path}: the `volume set-owners` assignment to '{owner}' is DROPPED — ownership \
                 is a lease under the forest (D19 reversed); the record stays decodable and is \
                 never written under bit 17"
            ));
        }
        if ins.partition_solo_record {
            dropped.push(format!(
                "{path}: the solo bit-8 partition suffix on the ledger record is DROPPED — the \
                 forest's ledger is the manager's, un-suffixed"
            ));
        }
    }

    if opts.dry_run {
        let rows = ordered
            .iter()
            .zip(&inspections)
            .map(|(path, ins)| {
                let outcome = if ins.symmetric && ins.marker.is_none() {
                    ConversionOutcome::AlreadySymmetric
                } else {
                    ConversionOutcome::Planned
                };
                VolumeConversionRow::new(path, outcome).with_plan(ins.plan.as_ref())
            })
            .collect();
        return Ok(EnableSymReport {
            volumes: ordered,
            rows,
            dropped,
        });
    }

    // ---- 1. the markers, on every unconverted volume without one ------
    let marker = SymUpgradeMarker {
        volumes: ordered.clone(),
    };
    for (vi, (path, ins)) in ordered.iter().zip(&inspections).enumerate() {
        if ins.symmetric || ins.marker.is_some() {
            continue;
        }
        write_sym_marker(path, vi, &marker, &mut flocks).await?;
    }
    if hooks.crash_after == Some(EnableSymCrash::AfterMarker) {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection (enable-symmetric: after the markers)".to_string(),
        ));
    }

    // ---- 2..6. per volume, canonical order ----------------------------
    let mut rows = Vec::with_capacity(ordered.len());
    for (vi, (path, ins)) in ordered.iter().zip(&inspections).enumerate() {
        if ins.symmetric && ins.marker.is_none() {
            rows.push(VolumeConversionRow::new(
                path,
                ConversionOutcome::AlreadySymmetric,
            ));
            continue;
        }
        let t0 = std::time::Instant::now();
        let mut row = VolumeConversionRow::new(
            path,
            if opts.resume {
                ConversionOutcome::Resumed
            } else {
                ConversionOutcome::Converted
            },
        )
        .with_plan(ins.plan.as_ref());
        if !ins.symmetric {
            let built = convert_volume_to_forest(path, vi, hooks, &mut flocks).await?;
            row.records = built.records;
            row.bytes = built.bytes;
            row.slot_trees = built.slot_trees;
            row.extents_written = built.extents_written;
            row.orphans_reclaimed = built.orphans_reclaimed;
        }
        row.extents_freed = finish_forest_conversion(path, vi, hooks, &mut flocks).await?;
        row.secs = t0.elapsed().as_secs_f64();
        log::info!(
            "volume enable-symmetric: {path} converted — {} records ({} B) into {} slot \
             tree(s), {} extents written (planned {}, {} claimable), {} freed, {} orphans \
             reclaimed, {:.2} s",
            row.records,
            row.bytes,
            row.slot_trees,
            row.extents_written,
            row.extents_needed,
            row.extents_available,
            row.extents_freed,
            row.orphans_reclaimed,
            row.secs
        );
        rows.push(row);
    }
    Ok(EnableSymReport {
        volumes: ordered,
        rows,
        dropped,
    })
}

/// `--abort`: undo a crashed run on every volume still FLAT under its
/// marker. Refuses the whole set, touching nothing, when any volume is
/// past its flip (stamped, marker present): the forest is that volume's
/// truth — its old trees are unreferenced or already freed — and
/// `--resume` is the only exit. Otherwise, per flat marked volume: the
/// flat trees are verified intact (the digest walk reads every live
/// record), the extents the aborted build claimed are reclaimed (claimed
/// and reachable from no ledger root; the bitmap written at a new
/// generation), and the marker is removed through the marker-tolerant
/// guarded open — the volume is a flat volume of the same records, as
/// before the verb (pinned: pre-verb digest, writer-mountable).
async fn abort_conversion(
    ordered: &[String],
    inspections: &[SymInspection],
    markers: &[&String],
    flocks: &mut VerbFlocks,
) -> Result<EnableSymReport> {
    if markers.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "volume enable-symmetric --abort: nothing to abort — no volume of the set carries \
             a conversion marker"
                .to_string(),
        ));
    }
    if let Some((path, _)) = ordered
        .iter()
        .zip(inspections)
        .find(|(_, i)| i.symmetric && i.marker.is_some())
    {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume enable-symmetric --abort refused: {path} is past its flip (incompat bit 17 \
             stamped, marker present) — the forest is its truth and its shared trees are \
             unreferenced or already freed; nothing was undone on any volume. Finish the \
             conversion with `--resume`"
        )));
    }
    // The torn-stamp window: `set_symmetric_forest` writes the DUR-5
    // backup copy FIRST, so a kill between its two sector writes leaves
    // sector 0 FLAT under a STAMPED copy. Every reader honours the
    // primary, so the volume IS flat and the abort above would admit it —
    // but it writes no superblock, and the stale stamped copy would stay
    // for a later sector-0 failure to fall back onto (a forest superblock
    // over a flat ledger: a loud refusal only surgery exits). `--resume`
    // rewrites both copies at a new generation; the abort refuses.
    for (path, ins) in ordered.iter().zip(inspections) {
        if ins.symmetric || ins.marker.is_none() {
            continue;
        }
        if let Some(backup) =
            crate::meta_backend::kv::superblock::read_backup_superblock(Path::new(path)).await?
        {
            if backup.symmetric_forest_stamped() {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric --abort refused: {path}'s redundant superblock copy \
                     already carries incompat bit 17 while sector 0 is flat — the run was killed \
                     inside the stamp's two sector writes. Undoing it would leave a stale \
                     stamped copy on the device for a later sector-0 failure to fall back onto; \
                     nothing was undone on any volume. Finish the conversion with `--resume` \
                     (it rewrites both copies)"
                )));
            }
        }
    }
    let mut rows = Vec::with_capacity(ordered.len());
    for (vi, (path, ins)) in ordered.iter().zip(inspections).enumerate() {
        let outcome = if ins.symmetric {
            ConversionOutcome::AlreadySymmetric
        } else if ins.marker.is_none() {
            ConversionOutcome::Untouched
        } else {
            let mut row = VolumeConversionRow::new(path, ConversionOutcome::Aborted);
            row.orphans_reclaimed = abort_volume_conversion(path, vi, flocks).await?;
            log::info!(
                "volume enable-symmetric --abort: {path}: marker removed, {} extent(s) of the \
                 aborted build reclaimed — a flat volume as before the verb",
                row.orphans_reclaimed
            );
            rows.push(row);
            continue;
        };
        rows.push(VolumeConversionRow::new(path, outcome));
    }
    Ok(EnableSymReport {
        volumes: ordered.to_vec(),
        rows,
        dropped: Vec::new(),
    })
}

/// One flat marked volume's abort (see [`abort_conversion`]). Returns the
/// extents reclaimed.
async fn abort_volume_conversion(path: &str, vi: usize, flocks: &mut VerbFlocks) -> Result<u64> {
    use crate::meta_backend::kv::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
    use crate::meta_backend::kv::backend::{KvMetaBackend, PENDING_FREE_CAP};
    use crate::meta_backend::kv::builder::{digest_backend, digest_backend_kind_set};
    use crate::meta_backend::kv::checkpoint::{write_ledger_slot, LedgerRecord};
    use crate::meta_backend::kv::node::residue_seq_ceiling;
    use crate::meta_backend::kv::record::{
        KIND_INTERIOR, TREE_BLOCK_MAP, TREE_BLOCK_REFS, TREE_CONTROL,
    };

    let p = Path::new(path);
    let (sb, ledger, reachable, window_entries) = {
        let be = KvMetaBackend::open_probe(p).await?;
        if be.symmetric_forest() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric --abort: {path} mounted as a forest — refusing"
            )));
        }
        // Intact = every live record of EVERY kind the build would relay
        // reads and folds — the user kinds through the ordered digest,
        // the references and the block map through the per-kind set
        // oracle; a corrupt node fails the walk loud, never an abort
        // over it.
        digest_backend(&be).await?;
        digest_backend_kind_set(&be, TREE_BLOCK_REFS).await?;
        digest_backend_kind_set(&be, TREE_BLOCK_MAP).await?;
        let sb = be.superblock().clone();
        let ledger = be.mounted_ledger().clone();
        let mut reachable = std::collections::BTreeSet::new();
        for tree in be.all_trees() {
            for addr in tree.reachable_node_addrs().await? {
                reachable.insert(extent_of(&sb, addr)?);
            }
        }
        (sb, ledger, reachable, be.replay_stats().entries)
    };
    let mut reclaimed = 0u64;
    // The census is exact over an empty window only; a marked volume
    // whose window is not empty (a kill inside the marker's own guarded
    // open) leaves its orphans — if any — to the next verb's census.
    if window_entries == 0 {
        let total = sb.total_extents();
        let alloc = ExtentAllocator::load(
            p,
            sb.alloc_bitmap.start,
            total,
            compaction_reserve_extents(total),
            PENDING_FREE_CAP,
            ledger.journal_tail_seq,
            &[],
        )
        .await?;
        // The reclaimed extents go back to the FLAT free list, and their
        // residue — the crashed build's images — carries node seqs ABOVE
        // the flat ledger's watermark (Issue 4's law, the abort's twin):
        // the flat volume's next SMOs would mint `watermark + k` into
        // exactly these lowest-free extents. The ceiling over their
        // residue raises the watermark the ledger record below carries,
        // so the marker-removal mount (and every flat mount after it)
        // seeds its node-seq counter above every stamp they hold.
        let node_size = u64::from(sb.node_size);
        let mut seq_floor = ledger.node_seq_watermark;
        for extent in 0..total {
            if alloc.is_allocated(extent) && !reachable.contains(&extent) {
                let image = crate::uring_fs::read_at(
                    p,
                    sb.heap.start + extent * node_size,
                    sb.node_size as usize,
                )
                .await?;
                seq_floor = seq_floor.max(residue_seq_ceiling(&image));
                alloc.release_unpublished(extent);
                reclaimed += 1;
            }
        }
        if reclaimed > 0 {
            let generation = ledger
                .alloc_bitmap_generation
                .max(alloc.resume_generation())
                + 1;
            alloc
                .write_dirty_pages(p, sb.alloc_bitmap.start, generation)
                .await?;
            crate::uring_fs::fdatasync(p.to_path_buf()).await?;
            // ONE ledger record: the flat roots ALONE (a hybrid record's
            // forest roots now name reclaimed extents), the same tail,
            // the new bitmap generation, the raised watermark. Until it
            // lands the previous record still describes a consistent
            // flat volume (its forest roots are ignored by a flat open).
            let restated = LedgerRecord {
                seq: ledger.seq + 1,
                tree_roots: ledger
                    .tree_roots
                    .iter()
                    .copied()
                    .filter(|r| r.tree_id != TREE_CONTROL && r.tree_id != KIND_INTERIOR)
                    .collect(),
                journal_tail_seq: ledger.journal_tail_seq,
                next_ino: ledger.next_ino,
                alloc_bitmap_generation: generation,
                node_seq_watermark: seq_floor,
                membership_stamp: ledger.membership_stamp.clone(),
                append_partition: ledger.append_partition,
            };
            write_ledger_slot(p, sb.root_ledger.start, &restated).await?;
            crate::uring_fs::fdatasync(p.to_path_buf()).await?;
        }
    }
    remove_sym_marker(path, vi, flocks).await?;
    Ok(reclaimed)
}

/// The verb's QUIESCE of a pre-flip set (PR 14): the ordinary routed
/// writer open under the conversion's process-scoped admission
/// ([`crate::meta_backend::kv::backend::admit_pre_flip_writers`] — the
/// mount's own door refuses the class presence-required), which replays
/// every volume's window and rolls the set's open cross-volume intents
/// forward at bring-up; then one checkpoint per volume (a solo mount's
/// ledger record — the bit-8 non-solo record's remedy) and the clean
/// leave. Every verb flock is released for the open and re-taken after
/// it. Nothing of the conversion is written.
async fn quiesce_set_for_symmetric(ordered: &[String], flocks: &mut VerbFlocks) -> Result<()> {
    for vi in 0..ordered.len() {
        flocks.release(vi);
    }
    let body = async {
        let _admit = crate::meta_backend::kv::backend::admit_pre_flip_writers();
        let routed = crate::meta_backend::open_routed_meta_set(ordered)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric: the quiesce of the set (the mount's own crash \
                     recovery before the conversion) refused to open it: {e}"
                ))
            })?;
        let mut first_err: Option<SqueezefsError> = None;
        for vol in &routed.volumes {
            if let Err(e) = vol.checkpoint_now().await {
                first_err.get_or_insert(SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric: the quiesce checkpoint on {} failed: {e}",
                    vol.device_path().display()
                )));
            }
        }
        for vol in &routed.volumes {
            if let Err(e) = vol.shutdown().await {
                first_err.get_or_insert(SqueezefsError::InvalidOperation(format!(
                    "volume enable-symmetric: the quiesce's clean leave of {} failed: {e}",
                    vol.device_path().display()
                )));
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    .await;
    for vi in 0..ordered.len() {
        flocks.retake(vi).await?;
    }
    body
}

/// Write the marker on `path` through its guarded (marker-tolerant) open:
/// journal-committed, checkpointed, then a clean shutdown. The verb's own
/// flock is released for the open and re-taken after it.
async fn write_sym_marker(
    path: &str,
    vi: usize,
    marker: &SymUpgradeMarker,
    flocks: &mut VerbFlocks,
) -> Result<()> {
    use crate::meta_backend::kv::backend::KvMetaBackend;
    let image = marker.encode()?;
    flocks.release(vi);
    let be = KvMetaBackend::open_for_sym_upgrade(Path::new(path)).await?;
    let body = async {
        be.setxattr_internal(1, crate::SYM_UPGRADE_MARKER_XATTR, &image)
            .await?;
        be.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: the marker on {path} did not land durably: {e}"
            ))
        })
    }
    .await;
    be.shutdown().await?;
    flocks.retake(vi).await?;
    body
}

/// Remove the marker on `path` through its guarded (marker-tolerant)
/// open — on a forest, its join, checkpoint and leave. The verb's own
/// flock is released for the open and re-taken after it.
async fn remove_sym_marker(path: &str, vi: usize, flocks: &mut VerbFlocks) -> Result<()> {
    use crate::meta_backend::kv::backend::KvMetaBackend;
    flocks.release(vi);
    let be = KvMetaBackend::open_for_sym_upgrade(Path::new(path)).await?;
    let body = async {
        be.removexattr_internal(1, crate::SYM_UPGRADE_MARKER_XATTR)
            .await?;
        be.checkpoint_now().await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: the marker delete on {path} did not land durably: {e}"
            ))
        })
    }
    .await;
    be.shutdown().await?;
    flocks.retake(vi).await?;
    body
}

/// The flat records of one volume, bucketed by the forest slot their key
/// names, framed as forest keys at seq 0.
struct FlatRecords {
    by_slot: std::collections::BTreeMap<
        crate::meta_backend::kv::record::ForestSlot,
        Vec<crate::meta_backend::kv::record::Record>,
    >,
    /// Highest LOCAL inode ino per slot (the §5.1.8 cursor's live term).
    max_local_ino: std::collections::BTreeMap<crate::meta_backend::kv::record::ForestSlot, u64>,
    records: u64,
    bytes: u64,
}

/// Walk every live record of every kind a flat volume holds (inodes,
/// dentries, xattrs; the block map and the block references when
/// engaged) through the probe's kind-routed range walks — the same walk
/// the digest oracle runs, so the forest built from it digests equal.
///
/// Block references (kind 6) are re-keyed by their owner's LOCAL KEY ino
/// on the way in (`shared_refs::local_key_owner` under the stamp's width
/// and native slot — the routed layer's `forest_ref_ops` law): the flat
/// ledger keys the GLOBAL owner, and a forest routes a reference by the
/// owner's slot bits, so a converted reference must carry the form the
/// live path writes or the pack law's one-slot probe would never see it.
async fn collect_flat_records(
    be: &crate::meta_backend::kv::backend::KvMetaBackend,
    stamp: &crate::meta_backend::kv::checkpoint::MembershipStamp,
) -> Result<FlatRecords> {
    use crate::meta_backend::kv::block_refs::{decode_block_ref_key, BLOCK_REF_OWNER_OFF};
    use crate::meta_backend::kv::node::key_successor;
    use crate::meta_backend::kv::record::{
        forest_key, forest_key_slot, Record, TREE_BLOCK_MAP, TREE_BLOCK_REFS, TREE_INODES,
    };
    use crate::meta_backend::kv::shared_refs::local_key_owner;
    let width = u64::from(stamp.routing_width);
    let native = stamp.resolved_native_slot();
    use crate::meta_backend::kv::tree::KEY_SPACE_MAX;
    use crate::meta_backend::GUEST_NS_SHIFT;

    // One leaf's worth of records per page (the census walks' shape).
    const PAGE: usize = 1024;
    let mut out = FlatRecords {
        by_slot: std::collections::BTreeMap::new(),
        max_local_ino: std::collections::BTreeMap::new(),
        records: 0,
        bytes: 0,
    };
    let mut kinds: Vec<u8> = crate::meta_backend::kv::backend::KvMetaBackend::USER_KINDS.to_vec();
    if be.block_map_tree_engaged() {
        kinds.push(TREE_BLOCK_MAP);
    }
    if be.block_refs_engaged() {
        kinds.push(TREE_BLOCK_REFS);
    }
    for kind in kinds {
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = be.range_kind(kind, &cursor, &KEY_SPACE_MAX, PAGE).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                let keyed: std::borrow::Cow<'_, [u8]> = if kind == TREE_BLOCK_REFS {
                    let r = decode_block_ref_key(k)?;
                    let mut re = k.to_vec();
                    re[BLOCK_REF_OWNER_OFF..BLOCK_REF_OWNER_OFF + 8].copy_from_slice(
                        &local_key_owner(r.owner_ino, width, native).to_be_bytes(),
                    );
                    std::borrow::Cow::Owned(re)
                } else {
                    std::borrow::Cow::Borrowed(k)
                };
                let key = forest_key(kind, &keyed)?;
                let slot = forest_key_slot(&key)?;
                if kind == TREE_INODES {
                    let ino_bytes: [u8; 8] =
                        k.get(..8).and_then(|s| s.try_into().ok()).ok_or_else(|| {
                            SqueezefsError::InvalidOperation(format!(
                                "volume enable-symmetric: an inode key of {} bytes (8 expected)",
                                k.len()
                            ))
                        })?;
                    let ino = u64::from_be_bytes(ino_bytes);
                    let local = ino & ((1u64 << GUEST_NS_SHIFT) - 1);
                    let e = out.max_local_ino.entry(slot).or_insert(0);
                    *e = (*e).max(local);
                }
                out.records += 1;
                out.bytes += (key.len() + v.len()) as u64;
                out.by_slot
                    .entry(slot)
                    .or_default()
                    .push(Record::put(key, 0, v.to_vec()));
            }
            if page.len() < PAGE {
                break;
            }
        }
    }
    Ok(out)
}

/// What [`convert_volume_to_forest`] produced.
struct ForestBuild {
    records: u64,
    bytes: u64,
    slot_trees: u64,
    extents_written: u64,
    orphans_reclaimed: u64,
}

/// Heap extent index of a node address; an address below the heap is
/// corruption, never a wrap.
fn extent_of(sb: &crate::meta_backend::kv::superblock::SuperblockV3, addr: u64) -> Result<u64> {
    addr.checked_sub(sb.heap.start)
        .map(|off| off / u64::from(sb.node_size))
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: node address {addr:#x} lies below the heap ({:#x})",
                sb.heap.start
            ))
        })
}

/// Refuse unless the ring window over `extent` is EMPTY — the conversion's
/// offline steps read the trees through the ledger's roots, which is exact
/// only when nothing in the ring is ahead of them. Reached only after the
/// volume's own quiesce (or on a stamped volume under its marker), so the
/// remedy is the verb's: a `--resume` re-quiesces.
async fn assert_quiesced_ring(
    path: &Path,
    extent: crate::meta_backend::kv::superblock::ExtentRef,
    tail_seq: u64,
) -> Result<()> {
    use crate::meta_backend::kv::journal::{
        checkpoint_reserve_bytes, JournalRing, JOURNAL_PAGE_LEN,
    };
    let (_ring, recovery) = JournalRing::recover(
        path,
        extent.start,
        extent.len / JOURNAL_PAGE_LEN,
        checkpoint_reserve_bytes(extent.len),
        tail_seq,
    )
    .await?;
    if !recovery.entries.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "volume enable-symmetric: {} still holds {} journal entr(ies) past its checkpoint \
             tail after the quiesce — refusing to convert a volume whose trees are not the \
             whole truth (the shutdown checkpoint did not converge; re-run with `--resume`, \
             which quiesces the volume again)",
            path.display(),
            recovery.entries.len()
        )));
    }
    Ok(())
}

/// Steps 2–4 of [`enable_symmetric`] on one flat volume: quiesce, read,
/// build the forest into fresh extents, write the hybrid ledger, stamp.
async fn convert_volume_to_forest(
    path: &str,
    vi: usize,
    hooks: &EnableSymHooks,
    flocks: &mut VerbFlocks,
) -> Result<ForestBuild> {
    use crate::meta_backend::kv::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
    use crate::meta_backend::kv::appender::{
        appender0_page_offsets, appender0_ring_extent, dir_header_offset, dir_pairs_per_extent,
        page_slot_for, AppenderPage, DirHeader,
    };
    use crate::meta_backend::kv::backend::{KvMetaBackend, PENDING_FREE_CAP};
    use crate::meta_backend::kv::builder::{zero_range, TreeWriter};
    use crate::meta_backend::kv::checkpoint::{write_ledger_slot, LedgerRecord, TreeRoot};
    use crate::meta_backend::kv::node::{residue_seq_ceiling, NodeLayout};
    use crate::meta_backend::kv::record::{
        Record, KIND_INTERIOR, NATIVE_FOREST_SLOT, TREE_CONTROL,
    };
    use crate::meta_backend::kv::slot_state::{slot_state_key, SlotState};
    use crate::meta_backend::kv::superblock::{set_symmetric_forest, ExtentRef};
    use crate::meta_backend::kv::tree::RootPtr;

    let p = Path::new(path);
    let crash = |w: EnableSymCrash| -> Result<()> {
        if hooks.crash_after == Some(w) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "crash injection (enable-symmetric: volume {vi} {w:?})"
            )));
        }
        Ok(())
    };

    // (2a) Quiesce under the D0 ladder: a guarded open runs the claim
    // gate (a live foreign writer refuses here, never gets its ring
    // zeroed under it), the checkpoint + clean shutdown leave an EMPTY
    // window with every record in the trees.
    {
        flocks.release(vi);
        let be = KvMetaBackend::open_for_sym_upgrade(p).await?;
        let r = be.checkpoint_now().await;
        be.shutdown().await?;
        flocks.retake(vi).await?;
        r?;
    }

    // (2b) Read: the ledger, the empty window (asserted before anything
    // is collected), the reachable image set, every live record.
    let (sb, ledger, reachable, flat, stamp) = {
        let be = KvMetaBackend::open_probe(p).await?;
        if be.symmetric_forest() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} mounted as a forest mid-conversion — refusing"
            )));
        }
        let sb = be.superblock().clone();
        let ledger = be.mounted_ledger().clone();
        assert_quiesced_ring(p, sb.journal, ledger.journal_tail_seq).await?;
        let mut reachable = std::collections::BTreeSet::new();
        for tree in be.all_trees() {
            for addr in tree.reachable_node_addrs().await? {
                reachable.insert(extent_of(&sb, addr)?);
            }
        }
        let stamp = ledger.membership_stamp.clone().ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} carries no membership stamp — not a \
                 dynamic-routing set member (reformat required)"
            ))
        })?;
        let flat = collect_flat_records(&be, &stamp).await?;
        (sb, ledger, reachable, flat, stamp)
    };

    // (2c) The allocator off the durable bitmap; reclaim what no root
    // reaches (a crashed build's forest, the shipped root-swap leak), and
    // raise the node-seq floor above every stamp their residue carries:
    // the ledger's watermark law covers extents freed UNDER a record,
    // never these (`residue_seq_ceiling`).
    let total = sb.total_extents();
    let alloc = ExtentAllocator::load(
        p,
        sb.alloc_bitmap.start,
        total,
        compaction_reserve_extents(total),
        PENDING_FREE_CAP,
        ledger.journal_tail_seq,
        &[],
    )
    .await?;
    let node_size = u64::from(sb.node_size);
    let mut orphans_reclaimed = 0u64;
    let mut seq_floor = ledger.node_seq_watermark;
    for extent in 0..total {
        if alloc.is_allocated(extent) && !reachable.contains(&extent) {
            let image = crate::uring_fs::read_at(
                p,
                sb.heap.start + extent * node_size,
                sb.node_size as usize,
            )
            .await?;
            seq_floor = seq_floor.max(residue_seq_ceiling(&image));
            alloc.release_unpublished(extent);
            orphans_reclaimed += 1;
        }
    }
    if orphans_reclaimed > 0 {
        log::warn!(
            "volume enable-symmetric: {path}: reclaimed {orphans_reclaimed} claimed extent(s) no \
             ledger root reaches (a crashed conversion's build, or leaked root-swap images); \
             node seqs resume above {seq_floor} (ledger watermark {})",
            ledger.node_seq_watermark
        );
    }

    // (2d) The forest: one slot tree per slot (an EMPTY one for a hosted
    // slot whose stamp carries a cursor but whose records are gone —
    // tree 0 is the cursor's durable home, §5.1.8), tree 0, the
    // directory, appender 0's page in the zeroed fixed ring, the bitmap.
    // The forest's frames are v2 under the manager's `(0, 0)` stamp
    // (§5.8.2) — the layout the stamped volume's every later open reads.
    let layout = NodeLayout::new_symmetric(sb.node_size as usize)?;
    let mut writer = TreeWriter::new(p, &layout, sb.heap.start, &alloc, seq_floor);
    let mut control: Vec<Record> = Vec::new();
    let mut native_root: Option<RootPtr> = None;
    let slots = forest_slots_of(&flat, &stamp);
    let slot_trees = slots.len() as u64;
    let mut by_slot = flat.by_slot;
    for slot in slots {
        let mut records = by_slot.remove(&slot).unwrap_or_default();
        records.sort_by(|a, b| a.key.cmp(&b.key));
        let nodes_before = writer.nodes_written();
        let (addr, seq) = writer.write_tree(KIND_INTERIOR, records).await?;
        let root = RootPtr { addr, seq };
        // One heap extent per node written for this slot tree.
        let slot_tree_extents =
            u32::try_from(writer.nodes_written() - nodes_before).map_err(|_| {
                SqueezefsError::InvalidOperation(
                    "volume enable-symmetric: a slot tree exceeds the page's extent-count width"
                        .to_string(),
                )
            })?;
        if slot == NATIVE_FOREST_SLOT {
            native_root = Some(root);
            continue;
        }
        // §5.1.8: a cursor is never lowered — the stamp's durable cursor
        // (it survives deletes of the slot's top inos) or the highest live
        // ino + 1, whichever is higher.
        let guest = u16::try_from(slot - 1).map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: forest slot {slot} is above the guest namespace"
            ))
        })?;
        let live_cursor = flat.max_local_ino.get(&slot).map_or(0, |m| m + 1);
        let cursor = stamp.cursor_for(guest).unwrap_or(0).max(live_cursor);
        // Offline: no lessee ever wrote the slot; the seq floor is the kept
        // quiesced tail so every later stamp on any ring exceeds ring 0's
        // pre-conversion history (the seq-space law, design §5.1.4).
        let state = SlotState::Unleased {
            root,
            cursor,
            g: 0,
            slot_tree_extents,
            last_written: 0,
            seq_floor: ledger.journal_tail_seq,
        };
        control.push(Record::put(slot_state_key(slot), 0, state.encode()));
    }
    let native_root = native_root.ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "volume enable-symmetric: the native slot tree was not written".to_string(),
        )
    })?;
    control.sort_by(|a, b| a.key.cmp(&b.key));
    let (control_addr, control_seq) = writer.write_tree(TREE_CONTROL, control).await?;

    let dir_extent = alloc.claim_internal()?;
    let appender_dir = ExtentRef {
        start: sb.heap.start + dir_extent * node_size,
        len: node_size,
    };
    zero_range(p, appender_dir.start, appender_dir.len).await?;
    let hdr = DirHeader {
        chain_index: 0,
        next: ExtentRef { start: 0, len: 0 },
        pairs: dir_pairs_per_extent(node_size) as u16,
    };
    crate::uring_fs::write_at(
        p,
        dir_header_offset(&appender_dir),
        bytes::Bytes::from(hdr.encode()),
    )
    .await?;
    // The fixed ring is ZEROED (its window is empty — asserted above — so
    // everything it holds is dead history no layout may ever replay) and
    // appender 0's `Free` page lands in the first of its four slots. The
    // ring's SEQ SPACE is NOT restarted: the hybrid ledger keeps the
    // quiesced tail `T`, and both layouts resume their ring at `T` over
    // the zeros (an empty window, the PR-3 re-carve law). The forest's
    // records are at seq 0 ≤ T (checkpoint-covered by construction); the
    // FLAT trees' records carry seqs ≤ T — and a flat open of the hybrid
    // state (the window before the stamp: `--resume`'s quiesce, `--abort`'s
    // marker removal) mints its record seqs ABOVE the head it resumes at.
    // A tail of 0 restarted that space below the flat records: every
    // per-key LWW fold then lost the new write (the marker's Delete, an
    // interior child-pointer update → a routing hole), which is what the
    // `--abort` contracts found.
    zero_range(p, sb.journal.start, sb.journal.len).await?;
    let mut page0 = AppenderPage::free(0, 1);
    page0.segments = vec![appender0_ring_extent(&sb.journal)];
    page0.ledger_tail_seq = ledger.journal_tail_seq;
    page0.head_hint = ledger.journal_tail_seq;
    let offs = appender0_page_offsets(&sb.journal);
    crate::uring_fs::write_at(
        p,
        offs[page_slot_for(page0.generation)],
        bytes::Bytes::from(page0.encode()?),
    )
    .await?;
    let generation = ledger
        .alloc_bitmap_generation
        .max(alloc.resume_generation())
        + 1;
    alloc
        .write_dirty_pages(p, sb.alloc_bitmap.start, generation)
        .await?;
    crate::uring_fs::fdatasync(p.to_path_buf()).await?;
    crash(EnableSymCrash::AfterBuild { volume: vi })?;

    // (3) The hybrid ledger: the flat roots stay (a flat open still finds
    // them), the forest roots join, the quiesced tail stays (see the ring
    // note above), the new bitmap generation.
    let mut tree_roots: Vec<TreeRoot> = ledger
        .tree_roots
        .iter()
        .copied()
        .filter(|r| r.tree_id != TREE_CONTROL && r.tree_id != KIND_INTERIOR)
        .collect();
    tree_roots.push(TreeRoot {
        tree_id: TREE_CONTROL,
        node_addr: control_addr,
        node_seq: control_seq,
    });
    tree_roots.push(TreeRoot {
        tree_id: KIND_INTERIOR,
        node_addr: native_root.addr,
        node_seq: native_root.seq,
    });
    let hybrid = LedgerRecord {
        seq: ledger.seq + 1,
        tree_roots,
        journal_tail_seq: ledger.journal_tail_seq,
        next_ino: ledger.next_ino,
        alloc_bitmap_generation: generation,
        node_seq_watermark: writer.node_seq_watermark(),
        membership_stamp: Some(stamp),
        append_partition: None,
    };
    write_ledger_slot(p, sb.root_ledger.start, &hybrid).await?;
    crate::uring_fs::fdatasync(p.to_path_buf()).await?;
    crash(EnableSymCrash::AfterLedger { volume: vi })?;

    // (4) The stamp: bit 17 + the directory, one sector, barriered.
    set_symmetric_forest(p, appender_dir).await?;
    crash(EnableSymCrash::AfterStamp { volume: vi })?;

    Ok(ForestBuild {
        records: flat.records,
        bytes: flat.bytes,
        slot_trees,
        extents_written: writer.nodes_written() + 1,
        orphans_reclaimed,
    })
}

/// Steps 5–6 of [`enable_symmetric`] on a stamped volume still under its
/// marker: free the old trees the hybrid ledger names and fold them out,
/// then remove the marker through the forest's guarded open. Returns the
/// extents freed (0 when a previous run already folded them out).
async fn finish_forest_conversion(
    path: &str,
    vi: usize,
    hooks: &EnableSymHooks,
    flocks: &mut VerbFlocks,
) -> Result<u64> {
    use crate::meta_backend::kv::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
    use crate::meta_backend::kv::appender::appender0_ring_extent;
    use crate::meta_backend::kv::backend::PENDING_FREE_CAP;
    use crate::meta_backend::kv::checkpoint::{
        read_newest_ledger, write_ledger_slot, LedgerRecord,
    };
    use crate::meta_backend::kv::node::NodeLayout;
    use crate::meta_backend::kv::node_cache::{
        NodeCache, NodeCacheConfig, DEFAULT_CACHE_BUDGET_BYTES, DEFAULT_WRITEBACK_DELTA_BYTES,
    };
    use crate::meta_backend::kv::record::{KIND_INTERIOR, TREE_CONTROL};
    use crate::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
    use crate::meta_backend::kv::tree::{KvTree, RootPtr};

    let p = Path::new(path);
    let sb = match classify_volume(p).await? {
        VolumeFormat::V3(sb) if sb.symmetric_forest_stamped() => sb,
        _ => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} is not stamped bit 17 at the free step — refusing"
            )))
        }
    };
    let ledger = read_newest_ledger(p, sb.root_ledger.start)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "volume enable-symmetric: {path} has no valid root-ledger record"
            ))
        })?;
    let old_roots: Vec<_> = ledger
        .tree_roots
        .iter()
        .copied()
        .filter(|r| r.tree_id != TREE_CONTROL && r.tree_id != KIND_INTERIOR)
        .collect();
    let mut freed = 0u64;
    if !old_roots.is_empty() {
        // (5) Nothing reaches the old trees but these roots, and no forest
        // mount has run (the marker refuses writers; this step precedes
        // the verb's own): their image set is exact and the walk is
        // repeatable until the ledger below drops the roots.
        assert_quiesced_ring(
            p,
            appender0_ring_extent(&sb.journal),
            ledger.journal_tail_seq,
        )
        .await?;
        let total = sb.total_extents();
        let alloc = ExtentAllocator::load(
            p,
            sb.alloc_bitmap.start,
            total,
            compaction_reserve_extents(total),
            PENDING_FREE_CAP,
            ledger.journal_tail_seq,
            &[],
        )
        .await?;
        // The OLD trees are the flat layout's (v1 frames): the walk that
        // frees them reads them under the layout they were written with,
        // whatever sector 0 now says.
        let layout = NodeLayout::new(sb.node_size as usize)?;
        let cache = NodeCache::new(NodeCacheConfig {
            path: p.to_path_buf(),
            layout,
            heap_base: sb.heap.start,
            budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        // The legacy shared handle (the flat trees being freed are the
        // manager's space; nothing is minted here).
        let seq = std::sync::Arc::new(crate::meta_backend::kv::node_seq::NodeSeqHandle::shared(
            ledger.seq.max(ledger.node_seq_watermark),
        ));
        for root in &old_roots {
            let tree = KvTree::open(
                cache.clone(),
                root.tree_id,
                RootPtr {
                    addr: root.node_addr,
                    seq: root.node_seq,
                },
                seq.clone(),
            )
            .await?;
            for addr in tree.reachable_node_addrs().await? {
                let extent = extent_of(&sb, addr)?;
                if alloc.is_allocated(extent) {
                    alloc.release_unpublished(extent);
                    freed += 1;
                }
            }
        }
        let generation = ledger
            .alloc_bitmap_generation
            .max(alloc.resume_generation())
            + 1;
        alloc
            .write_dirty_pages(p, sb.alloc_bitmap.start, generation)
            .await?;
        crate::uring_fs::fdatasync(p.to_path_buf()).await?;
        let folded = LedgerRecord {
            seq: ledger.seq + 1,
            tree_roots: ledger
                .tree_roots
                .iter()
                .copied()
                .filter(|r| r.tree_id == TREE_CONTROL || r.tree_id == KIND_INTERIOR)
                .collect(),
            journal_tail_seq: ledger.journal_tail_seq,
            next_ino: ledger.next_ino,
            alloc_bitmap_generation: generation,
            node_seq_watermark: ledger.node_seq_watermark,
            membership_stamp: ledger.membership_stamp.clone(),
            append_partition: None,
        };
        write_ledger_slot(p, sb.root_ledger.start, &folded).await?;
        crate::uring_fs::fdatasync(p.to_path_buf()).await?;
    }
    if hooks.crash_after == Some(EnableSymCrash::AfterFree { volume: vi }) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "crash injection (enable-symmetric: volume {vi} after the old trees' free)"
        )));
    }

    // (6) The marker's removal — the forest's own guarded open: join,
    // the delete committed, checkpointed, a clean leave.
    remove_sym_marker(path, vi, flocks).await?;
    Ok(freed)
}
