use crate::error::{Result, SqueezefsError};
use bytes::Bytes;
use log::{error, info};
use squeezefs_ipc::sqz_channel::mpsc;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use xxhash_rust::xxh3::xxh3_64;

/// Marker file binding a staging dir to the filesystem generation
/// (`meta_backend::volume_set_generation`) it was populated by. Lives at
/// the root of every staging dir handed to [`NvmeStaging::new`].
pub const STAGING_GENERATION_MARKER: &str = ".squeezefs_generation";

/// Marker header line: versions the marker format itself, so a future
/// identity-scheme change can re-stamp instead of misparsing.
const STAGING_GENERATION_HEADER: &str = "squeezefs-staging-generation-v1";

/// Marker file naming the highest staging CONTENT-FORMAT version this dir
/// may contain (design-random-small-writes §5.2 review Issue 17 — the
/// forward-only downgrade fence). Lives beside the generation marker at
/// the root of every staging dir.
pub const STAGING_FORMAT_MARKER: &str = ".squeezefs_staging_format";

/// Staging content-format version THIS binary reads and writes.
///
/// * **1** — implicit pre-RW4 content (plain staged `file_id` blobs +
///   `active_block:` whole-image records; no marker file existed).
/// * **2** — RW4: adds `active_block_ext:` extent records
///   ([`ExtentRecord`]).
/// * **3** — §6.2 item 8: records may be keyed with a WRITER-SCOPE
///   component (`…:w_{16 hex}`). Written only by a mount whose volume set
///   carries incompat bit 10 ([`staging_format_write_version`]), so an
///   un-stamped set keeps stamping v2 and stays adoptable by every shipped
///   binary. A pre-item-8 binary (max v2) refuses a v3 root loud, which is
///   exactly the forward-only downgrade fence: it would mint unscoped keys
///   beside scoped records and could not classify either.
///
/// The RW4 binary is the FIRST that validates this marker: a dir whose
/// marker names a version **greater** than this constant belongs to a
/// newer binary's dirty staging and is refused **as a unit, loudly**
/// (mount fails; never wiped — the content is acked custody of a format
/// this binary cannot parse). Downgrade below RW4 is declared unsupported
/// (the house forward-only law): pre-RW4 binaries validate nothing and
/// silently skip unknown keys — the §5.2 kill-9-then-downgrade residual,
/// forward-detected by the next RW4+ mount's orphan-record sweep
/// (`SqueezefsFilesystem::recover_extent_records`).
pub const STAGING_FORMAT_VERSION: u32 = 3;

/// Staging content-format version to STAMP: v3 only when the mount is
/// writer-scoped (§6.2 items 8/10 — its records may carry scope
/// components), else v2, byte-identical to every shipped release. Keeping
/// the read ceiling ([`STAGING_FORMAT_VERSION`]) above the write version
/// is what makes an un-stamped volume set's staging root still adoptable
/// by an older binary (ruling D9's compatibility requirement).
pub fn staging_format_write_version(scoped: bool) -> u32 {
    if scoped {
        3
    } else {
        2
    }
}

/// Full marker file image for `fs_generation`.
/// `(ino, block)` of an `active_block:inode_{i}:block_{b}` or
/// `active_block_ext:inode_{i}:block_{b}` staging key; `None` for plain
/// file_id keys (whole-file staged blobs have no per-block custody word).
///
/// Tolerates the §6.2-item-8 writer-scope component (`…:w_{16 hex}`):
/// the identity a scoped key names is the same `(ino, block)`, and every
/// parse site must see it — a foreign record has to be PARSEABLE before
/// it can be classified.
fn parse_block_custody_key(key: &str) -> Option<(u64, u32)> {
    let key = crate::writer_scope::strip_key_scope(key);
    let rest = key
        .strip_prefix("active_block:inode_")
        .or_else(|| key.strip_prefix("active_block_ext:inode_"))?;
    let (ino_str, block_str) = rest.split_once(":block_")?;
    Some((ino_str.parse().ok()?, block_str.parse().ok()?))
}

fn generation_marker_content(fs_generation: &str) -> Vec<u8> {
    format!("{STAGING_GENERATION_HEADER}\n{fs_generation}\n").into_bytes()
}

/// PR VL5b (KD-8): the TWO-PHASE rebind marker — a membership change
/// writes `old\nnew` BEFORE any stamp flips and finalizes to the single
/// new-generation marker after, so EVERY crash prefix leaves the dir
/// adoptable by whichever set is mountable (old set pre-flip, new set
/// post-flip) and durable staged custody is never discarded.
fn generation_marker_content_dual(old_gen: &str, new_gen: &str) -> Vec<u8> {
    format!("{STAGING_GENERATION_HEADER}\n{old_gen}\n{new_gen}\n").into_bytes()
}

/// The generations a marker image binds (one line each after the
/// header). Empty = unreadable/foreign.
fn marker_generations(bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut lines = text.lines();
    if lines.next() != Some(STAGING_GENERATION_HEADER) {
        return Vec::new();
    }
    lines
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Write (or rewrite) `dir`'s generation marker — the KD-8 restamp
/// primitive (PR VL5b): binds the dir to `fs_generation` WITHOUT wiping
/// content. Callers must have run the KD-8 barrier first (pending
/// `active_block:` custody refused; durable staged payloads rebind);
/// marker I/O is io_uring + fdatasync (never adopted volatile).
pub async fn write_staging_generation_marker(
    dir: &std::path::Path,
    fs_generation: &str,
) -> Result<()> {
    // VAL-7b: owner-only (0700) — the tree holds plaintext staged payloads
    // on passthrough volumes.
    crate::config_ops::create_private_dir_all(dir).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!("creating staging dir {}: {e}", dir.display()),
        ))
    })?;
    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    crate::uring_fs::write_all(&marker_path, generation_marker_content(fs_generation)).await?;
    crate::uring_fs::fdatasync(marker_path).await?;
    Ok(())
}

/// Write the TWO-PHASE rebind marker binding BOTH generations (KD-8
/// phase 1 — see `generation_marker_content_dual`).
pub async fn write_staging_generation_prepare_marker(
    dir: &std::path::Path,
    old_generation: &str,
    new_generation: &str,
) -> Result<()> {
    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    crate::uring_fs::write_all(
        &marker_path,
        generation_marker_content_dual(old_generation, new_generation),
    )
    .await?;
    crate::uring_fs::fdatasync(marker_path).await?;
    Ok(())
}

/// EVERY generation `dir`'s marker binds, in file order (a KD-8 dual
/// rebind marker binds two: `old`, then `new`). Empty = absent /
/// unreadable / foreign image. The KD-8 finalize step needs the whole set,
/// not just the first: after phase 1 the FIRST entry is the OLD generation,
/// so a first-entry-only test can never recognize its own dual marker.
pub async fn read_staging_generation_bindings(dir: &std::path::Path) -> Vec<String> {
    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    match crate::uring_fs::read_all(&marker_path).await {
        Ok(bytes) => marker_generations(&bytes),
        Err(_) => Vec::new(),
    }
}

/// Read `dir`'s generation-marker binding: `Ok(Some(generation))` for a
/// well-formed marker (a dual rebind marker reports its FIRST bound
/// generation), `Ok(None)` when absent/unreadable/foreign.
pub async fn read_staging_generation_marker(dir: &std::path::Path) -> Result<Option<String>> {
    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    let Ok(bytes) = crate::uring_fs::read_all(&marker_path).await else {
        return Ok(None);
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let mut lines = text.lines();
    if lines.next() != Some(STAGING_GENERATION_HEADER) {
        return Ok(None);
    }
    Ok(lines.next().map(str::to_string))
}

/// Enumerate LIVE staged write-custody keys in `dir` (the KD-8 barrier's
/// per-unit diagnostics): a standalone header scan of the
/// `staging_segment/` shard files. Sound because removal/flush zeroes
/// each record's on-disk `BLOCK_MAGIC` (`tiering::nvme::NvmeShardInner::
/// remove`), so only never-flushed custody still carries a live header —
/// a cleanly-unmounted dir scans EMPTY. Read cache (`cache_segment/`)
/// is deliberately not scanned: discarding it is always lossless.
pub async fn scan_live_staged_custody(
    dir: &std::path::Path,
    max_units: usize,
) -> Result<Vec<String>> {
    crate::tiering::nvme::scan_live_segment_keys(&dir.join("staging_segment"), max_units).await
}

/// PR VL6b (design-volume-lifecycle §5.6a, C4): extract the on-disk
/// image(s) of `key`'s live staged-custody record(s) in `dir` and, when
/// `kill` is set, retire them (magic zeroed AFTER extraction — the
/// recovery discard law made non-destructive by the caller's quarantine
/// copy). See [`crate::tiering::nvme::extract_and_kill_segment_records`].
pub async fn extract_and_kill_staged_custody(
    dir: &std::path::Path,
    key: &str,
    kill: bool,
) -> Result<Vec<bytes::Bytes>> {
    crate::tiering::nvme::extract_and_kill_segment_records(&dir.join("staging_segment"), key, kill)
        .await
}

/// Test seam (PR VL5b, KD-8 contracts): seed one live staged-custody
/// record (a minimal shard image with a single live header) so the
/// barrier's refusal path is exercisable without a mount.
pub async fn seed_staged_custody_for_test(dir: &std::path::Path, key: &str) -> Result<()> {
    let seg_dir = dir.join("staging_segment");
    crate::config_ops::create_private_dir_all(&seg_dir).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!("creating {}: {e}", seg_dir.display()),
        ))
    })?;
    let img = crate::tiering::nvme::encode_segment_record_for_test(key.as_bytes(), b"x");
    crate::uring_fs::write_all(&seg_dir.join("seeded_shard"), img).await?;
    Ok(())
}

/// Remove every regular file directly inside `dir` (segment files; the dir
/// itself and any nested dirs/symlinks stay). Follows `dir` when it is the
/// mount layout's `cache_segment -> ../cache_segment` symlink — stale
/// offset-keyed read-cache blocks are exactly as poisonous as stale staging
/// segments. Returns `(files_removed, bytes_removed)`; a missing dir is
/// zero work. Directory enumeration/unlink are std::fs by design — the
/// uring-fs exclusion list ("directory create/remove metadata").
fn wipe_segment_files(dir: &std::path::Path) -> std::io::Result<(usize, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            bytes += meta.len();
            fs::remove_file(entry.path())?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Remove stale GDS read-cache materializations (`*.gds_cache`) at the
/// staging-dir root — same offset/object-keyed poisoning class as the
/// segment files.
fn wipe_gds_cache_files(dir: &std::path::Path) -> std::io::Result<(usize, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let is_gds = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with(".gds_cache"));
        if !is_gds {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            bytes += meta.len();
            fs::remove_file(entry.path())?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Bind `dir` to `fs_generation` BEFORE any segment is mapped, recovered,
/// or budget-seeded (the reformat-over-stale-staging fix):
///
/// - marker matches ⇒ same generation, keep everything (warm restart);
/// - marker missing/unreadable/mismatched with NO segment data ⇒ stamp;
/// - marker missing/unreadable/mismatched WITH segment data ⇒ the content
///   belongs to a DEAD filesystem generation (or predates generation
///   stamping): discard it — wipe segment files in `cache_segment/` and
///   `staging_segment/` plus `*.gds_cache` materializations — with ONE
///   loud log line, bump `staging_generation_discards`, then stamp.
///
/// `fs_generation` is the **staging generation** — the volume-set
/// generation, node-scoped when the set carries incompat bit 10
/// ([`crate::writer_scope::staging_generation`]). That adds two arms to
/// the table above (spec §6.2 **item 10**), both decided by
/// [`crate::writer_scope::classify_generation`] so the gate and the KD-8
/// barrier cannot drift:
///
/// - same set, marker carries NO node scope while we do ⇒ the Phase-8
///   **upgrade**: adopt the content (it is ours — the D0 single-writer
///   guard governed the mount that wrote it) and re-stamp node-scoped.
///   This is also what makes a KD-8 rebind written by an unscoped binary
///   adoptable, i.e. belt and braces on the data-loss path;
/// - same set, marker carries a node scope we cannot claim ⇒ a PEER's
///   staging root. If it holds LIVE staged write custody the mount is
///   **refused loud** — wiping it would destroy another writer's acked
///   staged payloads, and its bytes are not ours to flush either. With no
///   live custody the content is dead and discarding it is lossless.
///
/// Marker I/O is io_uring (`crate::uring_fs`); the stamp is fdatasync'd so
/// a fresh generation is never adopted volatile.
async fn bind_staging_generation(dir: &std::path::Path, fs_generation: &str) -> Result<()> {
    use crate::writer_scope::GenerationBinding;

    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    let expected = generation_marker_content(fs_generation);

    let found = crate::uring_fs::read_all(&marker_path).await.ok();
    if found.as_deref() == Some(expected.as_slice()) {
        return Ok(());
    }
    // PR VL5b (KD-8): a mid-rebind dual marker (`old\nnew`) binds BOTH
    // generations — adopt when ours is listed and canonicalize to the
    // single marker (the crash-window adoption rule; module docs on
    // `generation_marker_content_dual`).
    let (_, want_scope) = crate::writer_scope::split_staging_generation(fs_generation);
    let bindings: Vec<GenerationBinding> = found
        .as_deref()
        .map(marker_generations)
        .unwrap_or_default()
        .iter()
        .map(|g| crate::writer_scope::classify_generation(g, fs_generation, want_scope))
        .collect();

    if bindings.contains(&GenerationBinding::Match) {
        crate::uring_fs::write_all(&marker_path, expected).await?;
        crate::uring_fs::fdatasync(&marker_path).await?;
        return Ok(());
    }

    // §6.2 item 10, the upgrade arm: our set, no node scope on the marker.
    if bindings.contains(&GenerationBinding::ScopeUpgrade) {
        crate::fuse_client::METRICS
            .staging_scope_upgrades
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let msg = format!(
            "STAGING SCOPE UPGRADE at {}: staging root was bound to the un-scoped \
             filesystem generation and this set is now writer-scoped — adopting the \
             content (single-writer guard owned the mount that wrote it) and re-stamping \
             node-scoped \"{fs_generation}\"; nothing discarded",
            dir.display(),
        );
        log::info!("{msg}");
        crate::uring_fs::write_all(&marker_path, expected).await?;
        crate::uring_fs::fdatasync(&marker_path).await?;
        return Ok(());
    }

    // §6.2 item 10, the foreign-node arm: never wipe a peer's live custody.
    if let Some(GenerationBinding::ForeignScope(other)) = bindings
        .iter()
        .find(|b| matches!(b, GenerationBinding::ForeignScope(_)))
        .copied()
    {
        let live = scan_live_staged_custody(dir, 8).await.unwrap_or_default();
        if !live.is_empty() {
            crate::fuse_client::METRICS
                .staging_foreign_scope_refusals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let msg = format!(
                "STAGING WRITER-SCOPE REFUSAL at {}: this staging root is bound to \
                 filesystem generation \"{fs_generation}\"'s set but to writer scope \
                 {other} (ours: {}), and it holds LIVE staged write custody \
                 ({} unit(s), e.g. {:?}) — refusing the staging root as a unit. Those \
                 bytes are another writer's acked custody: this client cannot flush them \
                 (their payload ring is that client's) and must not wipe them. Remedy: \
                 mount as the client that owns them (its node, and — for a mount-slot \
                 scope — its mount point, or `-o client_slot=<hex8>`) and let writeback \
                 drain, or give this mount a private staging path (`squeezefs config \
                 set-cache-paths`)",
                dir.display(),
                match want_scope {
                    Some(s) => s.render(),
                    None => "none (this volume set is not writer-scoped)".to_string(),
                },
                live.len(),
                live,
            );
            eprintln!("{msg}");
            log::error!("{msg}");
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                msg,
            )));
        }
        crate::fuse_client::METRICS
            .staging_foreign_scope_discards
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let msg = format!(
            "staging root {} was bound to writer scope {other} but carries no live \
             staged write custody — discarding its dead content (lossless) and stamping \
             \"{fs_generation}\"",
            dir.display(),
        );
        eprintln!("{msg}");
        log::warn!("{msg}");
    }

    let cache_dir = dir.join("cache_segment");
    let staging_dir = dir.join("staging_segment");
    let has_data = dir_has_segment_data(&cache_dir) || dir_has_segment_data(&staging_dir);
    if has_data {
        let (cf, cb) = wipe_segment_files(&cache_dir)?;
        let (sf, sb) = wipe_segment_files(&staging_dir)?;
        let (gf, gb) = wipe_gds_cache_files(dir)?;
        crate::fuse_client::METRICS
            .staging_generation_discards
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let found_desc = match &found {
            Some(bytes) => format!(
                "marker {:?}",
                String::from_utf8_lossy(bytes).replace('\n', "\\n")
            ),
            None => "no readable generation marker".to_string(),
        };
        let msg = format!(
            "STAGING GENERATION MISMATCH at {}: discarding staging content from a dead \
             filesystem generation ({found_desc}, mounted generation \"{fs_generation}\") — \
             removed {sf} staging segment file(s) ({sb} bytes), {cf} read-cache segment \
             file(s) ({cb} bytes), {gf} gds cache file(s) ({gb} bytes); staged writes and \
             active blocks stamped by the old generation are gone by design (reformat \
             discards data)",
            dir.display(),
        );
        // Loud at DEFAULT verbosity: the daemon's env_logger default filter
        // is ERROR, so a warn alone is invisible on a stock mount. Console
        // line (stderr → daemon log / terminal) + the structured record.
        eprintln!("{msg}");
        log::warn!("{msg}");
    }

    crate::uring_fs::write_all(&marker_path, expected).await?;
    crate::uring_fs::fdatasync(&marker_path).await?;
    Ok(())
}

/// Marker header line for [`STAGING_FORMAT_MARKER`] (versions the marker
/// format itself, independent of the content version it carries).
const STAGING_FORMAT_HEADER: &str = "squeezefs-staging-format-v1";

/// Full marker file image for staging format `version`.
fn staging_format_marker_content(version: u32) -> Vec<u8> {
    format!("{STAGING_FORMAT_HEADER}\n{version}\n").into_bytes()
}

/// Parse a [`STAGING_FORMAT_MARKER`] image → the content version.
fn parse_staging_format_marker(bytes: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != STAGING_FORMAT_HEADER {
        return None;
    }
    lines.next()?.trim().parse().ok()
}

/// W2 §5.2 forward-only downgrade fence (review Issue 17), dir level —
/// runs AFTER the generation gate (same-generation content only):
///
/// - marker names a version ≤ [`STAGING_FORMAT_VERSION`] ⇒ pass (re-stamp
///   the current version when older);
/// - marker missing/garbled ⇒ pre-RW4 content (or a fresh dir): adopt and
///   STAMP the current version — the below-RW4 downgrade direction stays
///   declared-unsupported with forward detection (the orphan-record sweep);
/// - marker names a FUTURE version ⇒ **refuse the segment as a unit,
///   loudly** (mount construction fails; the content is acked custody of
///   a newer binary and is never wiped or guessed at).
///
/// `scoped` selects the version this mount STAMPS
/// ([`staging_format_write_version`]); a marker naming a version this
/// binary reads but does not write (v3 under an unscoped mount) is left
/// ALONE rather than downgraded — a marker rewrite would destroy the only
/// record that scoped content may be present.
async fn validate_staging_format(dir: &std::path::Path, scoped: bool) -> Result<()> {
    let marker_path = dir.join(STAGING_FORMAT_MARKER);
    let write_version = staging_format_write_version(scoped);
    let found = crate::uring_fs::read_all(&marker_path)
        .await
        .ok()
        .as_deref()
        .and_then(parse_staging_format_marker);
    match found {
        Some(v) if v > STAGING_FORMAT_VERSION => {
            let msg = format!(
                "STAGING FORMAT VERSION FENCE at {}: dir carries staging format v{v} but \
                 this binary reads ≤ v{STAGING_FORMAT_VERSION} — refusing the staging \
                 segment as a unit (dirty staging of a NEWER binary; mount the newer \
                 binary to drain it, or wipe the dir explicitly — forward-only law)",
                dir.display(),
            );
            eprintln!("{msg}");
            log::error!("{msg}");
            Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                msg,
            )))
        }
        Some(v) if v >= write_version => Ok(()),
        _ => {
            crate::uring_fs::write_all(&marker_path, staging_format_marker_content(write_version))
                .await?;
            crate::uring_fs::fdatasync(&marker_path).await?;
            Ok(())
        }
    }
}

pub fn dir_has_segment_data(path: &std::path::Path) -> bool {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if metadata.is_file() && metadata.len() > 0 {
            return true;
        }
    }

    false
}

static SAFEGUARD_CACHE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static LAST_CHECK_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn check_disk_free_safeguard(path: &std::path::Path) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let last = LAST_CHECK_TIME.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 1 {
        return SAFEGUARD_CACHE.load(std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Ok(c_path) = CString::new(abs_path.as_os_str().as_bytes()) {
            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_blocks > 0 {
                    let free_fraction = stat.f_bavail as f64 / stat.f_blocks as f64;
                    let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
                    let safe = !(free_bytes < 100 * 1024 * 1024
                        || (free_fraction < 0.01 && free_bytes < 1024 * 1024 * 1024));
                    SAFEGUARD_CACHE.store(safe, std::sync::atomic::Ordering::Relaxed);
                    LAST_CHECK_TIME.store(now, std::sync::atomic::Ordering::Relaxed);
                    return safe;
                }
            }
        }
    }
    true
}

/// Process-global stage-generation source. Every assignment is unique for
/// the life of the process, so a generation captured under one ring-entry
/// INCARNATION can never match a later one: `remove_staged_if_generation`
/// (the promotion commit/release guard) previously compared per-entry
/// counters that RESTARTED when a removal deleted the ledger entry and a
/// re-stage re-created it — a queued/slow promotion that read the old
/// incarnation's image then passed its commit check against the new
/// incarnation's recycled generation, published the stale image as the
/// durable mapping, and destroyed the ring's only copy of the newer acked
/// bytes (the aged zeros-LOSS class, tests/staged_generation_aba_tests.rs).
/// Starts at 1: generation 0 is reserved for mount-recovered entries, which
/// no new assignment can forge.
static STAGE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_stage_generation() -> u64 {
    STAGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Binary header for staged / active-block payloads on local NVMe staging.
/// Shared with the mount-time staging recovery scan (`NvmeStaging::new`) —
/// must stay stable.
#[derive(Clone, Debug)]
pub struct StagedMetadata {
    pub fencing_token: u64,
    pub original_size: u64,
    pub file_path: String,
}

impl StagedMetadata {
    pub fn serialize(&self) -> Vec<u8> {
        let path_bytes = self.file_path.as_bytes();
        let mut buf = Vec::with_capacity(20 + path_bytes.len());
        buf.extend_from_slice(&self.fencing_token.to_be_bytes());
        buf.extend_from_slice(&self.original_size.to_be_bytes());
        buf.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(path_bytes);
        buf
    }

    /// Zero-alloc header peek (op-economy campaign): `original_size`
    /// with EXACTLY `deserialize`'s validation (length arithmetic + the
    /// path UTF-8 check) and none of its `file_path` String.
    pub fn peek_original_size(bytes: &[u8]) -> Option<u64> {
        if bytes.len() < 20 {
            return None;
        }
        let original_size = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let path_len = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
        if bytes.len() < 20 + path_len {
            return None;
        }
        std::str::from_utf8(&bytes[20..20 + path_len]).ok()?;
        Some(original_size)
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 20 {
            return None;
        }
        let fencing_token = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
        let original_size = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let path_len = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
        if bytes.len() < 20 + path_len {
            return None;
        }
        let file_path = String::from_utf8(bytes[20..20 + path_len].to_vec()).ok()?;
        Some(Self {
            fencing_token,
            original_size,
            file_path,
        })
    }
}

/// Parse a packed staging blob: `[meta_len:u64][meta bytes][payload…]`.
/// Active blocks pad the header to 4 KiB; ordinary staged files do not.
///
/// Returns `None` on malformed / hostile lengths (no panics on overflow — P2-13).
pub fn parse_staged_blob(bytes: &[u8], is_active_block: bool) -> Option<(StagedMetadata, Vec<u8>)> {
    if bytes.len() < 8 {
        return None;
    }
    let meta_len = usize::try_from(u64::from_be_bytes(bytes[0..8].try_into().ok()?)).ok()?;
    let meta_end = 8usize.checked_add(meta_len)?;
    if bytes.len() < meta_end {
        return None;
    }
    let meta = StagedMetadata::deserialize(&bytes[8..meta_end])?;
    let data_start = if is_active_block { 4096usize } else { meta_end };
    let payload_len = usize::try_from(meta.original_size).ok()?;
    let data_end = data_start.checked_add(payload_len)?;
    if bytes.len() < data_end {
        return None;
    }
    Some((meta, bytes[data_start..data_end].to_vec()))
}

/// `true` ⇔ `key` belongs to the staged block-record family — the
/// whole-image `active_block:` form or the W2 `active_block_ext:` extent
/// record — whose ring blobs pad their header to 4 KiB (payloads stay
/// 4 KiB-aligned for the §5.5 zero-copy DMA source) and which never enter
/// the staged-file budget ledger.
pub fn key_is_block_family(key: &str) -> bool {
    key.starts_with("active_block:") || key.starts_with("active_block_ext:")
}

/// Magic prefix of a serialized [`ExtentRecord`].
pub const EXTENT_RECORD_MAGIC: [u8; 8] = *b"SQZEXT01";

/// Record-level format version of [`ExtentRecord`] (belt-and-braces under
/// the dir-level [`STAGING_FORMAT_VERSION`] fence: a record naming a newer
/// version than this binary understands is refused loudly, never parsed).
///
/// **DUR-8a**: v1's digest covered `bytes[40..]` only — the body — so
/// `fencing_token`, `block_idx`, `flags` and `count` were UNCOVERED,
/// contradicting the type's own "checksummed as a unit" doc: a corrupted
/// `block_idx` silently misattributed staged extents to another block.
/// v2 covers the header too (its own digest field zeroed). v1 records are
/// still accepted with the v1 rule — an unclean shutdown under an older
/// binary is exactly when they exist, and refusing them would discard
/// acked custody — but nothing writes v1 again.
pub const EXTENT_RECORD_VERSION_V1: u32 = 1;
pub const EXTENT_RECORD_VERSION: u32 = 2;
/// Header length and digest offset shared by v1 and v2.
const EXTENT_RECORD_HDR_LEN: usize = 40;
const EXTENT_RECORD_SUM_OFF: usize = 32;

/// The DUR-8a v2 digest: the header with its own checksum field zeroed,
/// then the body.
fn extent_record_checksum(header: &[u8], body: &[u8]) -> u64 {
    let mut hdr = [0u8; EXTENT_RECORD_HDR_LEN];
    hdr.copy_from_slice(&header[..EXTENT_RECORD_HDR_LEN]);
    hdr[EXTENT_RECORD_SUM_OFF..EXTENT_RECORD_SUM_OFF + 8].fill(0);
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&hdr);
    h.update(body);
    h.digest()
}

/// Why an extent-record parse refused (both are LOUD at the consumer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtentRecordError {
    /// Structurally invalid — bad magic, short buffer, inconsistent table,
    /// or checksum mismatch (a torn/foreign blob): detected-and-ignored
    /// loudly per the D0 staging contract.
    Torn(String),
    /// The record names a FUTURE format version: refuse it as a unit —
    /// never guess, never wipe (it is acked custody of a newer binary).
    FutureVersion(u32),
}

/// W2 staged **extent record** (design-random-small-writes §5.2): the
/// 4 KiB-class spill form of a parked extent overlay — header
/// {version, fencing token, block idx, extent table} + concatenated
/// payloads, checksummed as a unit so a torn write is detected-and-ignored
/// loudly at parse time (the ring's header-level recovery scan cannot see
/// payload tears).
///
/// Layout (LE): magic 8B · version u32 · fencing_token u64 · block_idx u32
/// · flags u32 (bit0 = `base_deferred`) · extent_count u32 · checksum u64
/// (xxh3 over table+payloads) · table extent_count×(start u32, len u32) ·
/// payloads.
#[derive(Debug, Clone)]
pub struct ExtentRecord {
    pub version: u32,
    /// Fencing token at STAGING time — the FIND-M11-A supersession stamp
    /// (the remount law's discard test), never the fold's merge credential.
    pub fencing_token: u64,
    pub block_idx: u32,
    /// The unwritten complement owes the block's OLD DEVICE BYTES (item B
    /// deferral) — a fold must seed before applying. `false` = the block
    /// had no existing data (hole/fresh): the complement is zeros and the
    /// fold performs **no seed read**.
    pub base_deferred: bool,
    /// `(start, payload)` runs, ascending, pairwise disjoint and
    /// non-abutting — mirror of the overlay's coverage-union invariant.
    pub extents: Vec<(u32, Vec<u8>)>,
}

impl ExtentRecord {
    /// Total payload bytes across the extent runs.
    pub fn payload_bytes(&self) -> u64 {
        self.extents.iter().map(|(_, d)| d.len() as u64).sum()
    }

    pub fn serialize(&self) -> Vec<u8> {
        let table_len = self.extents.len() * 8;
        let payload_len: usize = self.extents.iter().map(|(_, d)| d.len()).sum();
        let mut body = Vec::with_capacity(table_len + payload_len);
        for (start, data) in &self.extents {
            body.extend_from_slice(&start.to_le_bytes());
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        }
        for (_, data) in &self.extents {
            body.extend_from_slice(data);
        }
        let mut out = Vec::with_capacity(EXTENT_RECORD_HDR_LEN + body.len());
        out.extend_from_slice(&EXTENT_RECORD_MAGIC);
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.fencing_token.to_le_bytes());
        out.extend_from_slice(&self.block_idx.to_le_bytes());
        out.extend_from_slice(&u32::from(self.base_deferred).to_le_bytes());
        out.extend_from_slice(&(self.extents.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // digest placeholder
                                                    // DUR-8a: the digest covers the HEADER (its own field zeroed) and
                                                    // the body — the record really is checksummed as a unit now.
        let checksum = if self.version > EXTENT_RECORD_VERSION_V1 {
            extent_record_checksum(&out, &body)
        } else {
            xxh3_64(&body)
        };
        out[EXTENT_RECORD_SUM_OFF..EXTENT_RECORD_SUM_OFF + 8]
            .copy_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub fn deserialize(bytes: &[u8]) -> std::result::Result<Self, ExtentRecordError> {
        let torn = |what: &str| ExtentRecordError::Torn(what.to_string());
        if bytes.len() < 40 {
            return Err(torn("short header"));
        }
        if bytes[0..8] != EXTENT_RECORD_MAGIC {
            return Err(torn("bad magic"));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version > EXTENT_RECORD_VERSION {
            return Err(ExtentRecordError::FutureVersion(version));
        }
        let fencing_token = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let block_idx = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let flags = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
        let checksum = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let table_end = 40usize
            .checked_add(count.checked_mul(8).ok_or_else(|| torn("count overflow"))?)
            .ok_or_else(|| torn("table overflow"))?;
        if bytes.len() < table_end {
            return Err(torn("short table"));
        }
        // DUR-8a: v2 covers the header, v1 only the body (still read so
        // an older binary's crash residue keeps its acked custody).
        let ok = if version > EXTENT_RECORD_VERSION_V1 {
            extent_record_checksum(
                &bytes[..EXTENT_RECORD_HDR_LEN],
                &bytes[EXTENT_RECORD_HDR_LEN..],
            ) == checksum
        } else {
            xxh3_64(&bytes[EXTENT_RECORD_HDR_LEN..]) == checksum
        };
        if !ok {
            return Err(torn("checksum mismatch"));
        }
        let mut extents = Vec::with_capacity(count);
        let mut cursor = table_end;
        for i in 0..count {
            let off = 40 + i * 8;
            let start = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            let end = cursor
                .checked_add(len)
                .ok_or_else(|| torn("len overflow"))?;
            if bytes.len() < end {
                return Err(torn("short payload"));
            }
            extents.push((start, bytes[cursor..end].to_vec()));
            cursor = end;
        }
        Ok(Self {
            version,
            fencing_token,
            block_idx,
            base_deferred: flags & 1 != 0,
            extents,
        })
    }
}

/// §5.5 write-only, guard-backed DMA source over one staged payload
/// (zero-copy write-path design, PR 3).
///
/// Wraps the staging shard's read guard so a flush can DMA the payload
/// straight off the staging mmap (`bytes::Bytes::from_owner`, no copy).
/// Active-block values are packed at a 4 KiB boundary with whole-block
/// length, so the source qualifies for `nvme_dev::write_block`'s zero-copy
/// `WriteData::Aligned` branch by construction (PR 2 contract).
///
/// Two §5.5 rules are enforced structurally:
///
/// - **Consumed by value, deliberately non-`Clone`**: the only sink is
///   [`write_block_from_staging`], which guarantees the guard is dead when
///   it returns — retention past the DMA (and therefore holding the shard
///   read lock into a same-shard write-lock op such as
///   `NvmeStaging::remove_active_block`, a parking_lot read→write
///   self-deadlock) is a compile error, not a review item.
///
/// ```compile_fail
/// # fn keep_a_copy(source: squeezefs::cache::nvme::StagedDmaSource) {
/// // §5.5: the source is deliberately non-Clone — a second guard-backed
/// // handle could outlive the DMA and reach a cache.
/// let _second: squeezefs::cache::nvme::StagedDmaSource = source.clone();
/// # }
/// ```
///
/// ```compile_fail
/// # async fn retain_past_dma(
/// #     crypto: &squeezefs::crypto_compress::CryptoCompressState,
/// #     dev: &squeezefs::nvme_dev::NvmeBlockDev,
/// #     source: squeezefs::cache::nvme::StagedDmaSource,
/// # ) {
/// squeezefs::cache::nvme::write_block_from_staging(crypto, dev, 0, 4 << 20, source)
///     .await
///     .unwrap();
/// // §5.5: retention past the DMA is a compile error (moved value).
/// let _still_alive = source.len();
/// # }
/// ```
///
/// - **A guard-backed `Bytes` never enters any cache**: an LRU entry has
///   unbounded lifetime and would hold the shard read-locked until
///   eviction. Cache-bound consumers (the not-yet-striped promotion put)
///   must use [`StagedDmaSource::detached_copy`] — a bounded real copy on a
///   cold path.
pub struct StagedDmaSource {
    guard: crate::tiering::nvme::NvmeCacheReadGuard,
}

impl StagedDmaSource {
    /// Payload length (the staged entry's `original_size`).
    pub fn len(&self) -> usize {
        self.guard.len
    }

    pub fn is_empty(&self) -> bool {
        self.guard.len == 0
    }

    /// Bounded REAL copy, detached from the guard — the only sanctioned way
    /// for staged bytes to outlive the source (§5.5: read-LRU promotion
    /// seeds; never the guard-backed memory itself).
    pub fn detached_copy(&self) -> Bytes {
        Bytes::copy_from_slice(&self.guard)
    }

    /// Convert into the guard-backed `Bytes` for the DMA submit. Private to
    /// this module on purpose: only [`write_block_from_staging`] may
    /// materialize a clonable guard-backed handle, and it provably drops it
    /// before returning.
    fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for StagedDmaSource {
    fn as_ref(&self) -> &[u8] {
        &self.guard
    }
}

/// Process (crypto/compress) and DMA one staged payload to `offset` on the
/// block backend, consuming the write-only `source` by value (§5.5).
///
/// Normative sequencing, encoded here once so every flush caller inherits
/// it:
///
/// - **Passthrough** (default): the DMA reads straight off the staging mmap
///   — the guard-backed `Bytes` moves into `write_block`, which keeps it
///   alive through completion *and* the sampled `--write-verification`
///   read-back, then drops it before returning. (The uring worker's
///   keep-alive clone is released on its own thread at CQE handling,
///   independent of any shard lock, so it can only delay — never deadlock —
///   a subsequent shard writer.)
/// - **Non-passthrough**: `process_write_async` consumes the guard-backed
///   `Bytes` and returns a fresh transform buffer — the guard is dead
///   *before* the DMA is submitted.
///
/// Either way the staging-shard read guard is provably dead when this
/// returns: callers may then merge metadata and take same-shard write locks
/// (`remove_active_block`) without self-deadlock, and no guard-backed
/// `Bytes` can leak into a cache. The shard read lock is therefore held
/// across exactly one transform-or-DMA (plus the sampled read-back verify
/// on `--write-verification` mounts) — the §5.5 hold bound.
///
/// `chunk_size` is the destination allocator's chunk: the transformed
/// image is guarded against it before the DMA (FIND-RW4-A — an oversized
/// image must fail loud here, never overflow into the neighboring chunk).
pub async fn write_block_from_staging(
    crypto: &crate::crypto_compress::CryptoCompressState,
    writer: &crate::nvme_dev::NvmeBlockDev,
    offset: u64,
    chunk_size: u64,
    source: StagedDmaSource,
) -> Result<()> {
    let processed = crypto.process_write_async(source.into_bytes()).await?;
    crate::block_allocator::ensure_stored_block_image_fits(
        processed.len(),
        chunk_size,
        "staged writeback flush",
    )?;
    writer.write_block(offset, processed).await
}

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dirs: Vec<PathBuf>,
    max_write_bytes: u64,
    max_read_bytes: u64,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    /// WEAK, deliberately (the `set_data_router` discipline: the router
    /// owns the cache, never the reverse). A strong Arc here closed the
    /// cycle `BackendRouter.read_tier_purge` closure → `TieredCache` →
    /// this cell → `BackendRouter`, immortalizing every mount/fixture
    /// graph — measured as ~65 leaked segment-file fds per dropped test
    /// harness (EMFILE by suite end; 2026-07-27 write-pipeline campaign
    /// conviction, pre-existing on dev).
    pub backend_router:
        std::sync::Arc<once_cell::sync::OnceCell<std::sync::Weak<crate::routing::BackendRouter>>>,
    /// Bounded merge-queue sender (P1-1). Full → StorageFull / backpressure.
    write_tx: mpsc::Sender<PendingStagedWrite>,
    pub current_staged_write_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Budget ledger: staged `file_id` → (bytes counted in
    /// `current_staged_write_bytes`, stage generation). Every add to the
    /// gauge records its cost here; every ring-entry removal credits the
    /// gauge through here — the gauge can never ratchet. The generation
    /// lets the merge worker skip crediting an entry that a racing
    /// re-stage has already replaced (the newer stage owns the budget).
    staged_ledger: std::sync::Arc<scc::HashMap<String, (u64, u64)>>,
    /// Latch-free occupancy index of staged `active_block:` keys (RW2 —
    /// design-random-small-writes §5.1 predicate 2 / review Issue 10): the
    /// W1 patch predicate probes staged existence without the
    /// `spawn_blocking` + shard-WRITE-lock hop and without even the shard
    /// `read_recursive` (which parks behind an ACTIVE writer's ms-class
    /// section). Maintained conservative-present: inserted BEFORE the ring
    /// write (removed again on refusal), removed AFTER the ring removal —
    /// so any window in which the entry exists has the key indexed. False
    /// positives are harmless (the caller falls back to the accumulation
    /// path); a false negative would let a patch race a pending writeback
    /// flush of stale staged bytes — the corruption direction, excluded by
    /// construction. Seeded from the recovered ring at startup.
    active_block_index: std::sync::Arc<scc::HashMap<String, ()>>,
    /// Population of `active_block_index` — OUR `active_block[_ext]:`
    /// custody records, i.e. exactly the set the dismount sweep and the
    /// writeback worker retire. Maintained beside every index insert/
    /// remove (O(1); `scc::HashMap::len` walks the bucket array) and
    /// polled by the dismount drain wait. `staged_writes_in_flight` is NOT
    /// that predicate: it counts every ring key, and a resident
    /// staged-LAYOUT `file_id` entry is retired by no teardown WAIT (the
    /// dismount promotion pass that drains that population is a work
    /// step, awaited to completion), so a wait on it ran to the timer on
    /// every unmount of such a mount
    /// (.benchmarks/2026-09-09-dismount-staged-residue.md §1.3).
    active_block_custody: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Router hook for the merge worker: promotion must commit layout through
    /// `DataRouter` (RAM metadata cache + backend coherently, under the
    /// per-inode metadata lock). Weak — the router owns this cache.
    data_router:
        std::sync::Arc<std::sync::OnceLock<std::sync::Weak<crate::routing::DataRouterInner>>>,
    pub space_freed_notify: std::sync::Arc<squeezefs_ipc::sqz_notify::Notify>,
    /// Every ring key (staged-layout `file_id`s AND active-block-family
    /// records; seeded from the recovered ring). A gauge, not the dismount
    /// wait's predicate — see `active_block_custody`.
    pub staged_writes_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Woken when `active_block_custody` reaches 0 — the dismount drain
    /// wait's wake.
    pub staged_drained_notify: std::sync::Arc<squeezefs_ipc::sqz_notify::Notify>,

    // Hypertier NVMe cache instances
    pub read_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub staging_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub crypto: std::sync::Arc<std::sync::OnceLock<crate::crypto_compress::CryptoCompressState>>,
}

#[derive(Debug, Clone)]
pub struct PendingStagedWrite {
    pub file_path: String,
    pub file_id: String,
    pub fencing_token: u64,
    pub padded_size: u64,
}

/// Same-key replace framing slack for [`shard_plan`]: two copies' key
/// frames (4 KiB each), metadata/pad frames (4 KiB each), and 4 KiB
/// placement alignment per copy, rounded up with margin.
const SHARD_REPLACE_SLACK: u64 = 64 * 1024;

/// Plan the per-device shard count for a segment ring of
/// `per_device_capacity` bytes holding entries up to `max_entry_bytes`.
///
/// Invariant (FIND-VS-B): a shard must fit a same-key crash-safe REPLACE
/// of a block-size-class entry. [`crate::tiering::nvme::NvmeShard::reserve_and_write`]
/// keeps the existing copy live while its replacement is placed (torn-write
/// immunity — never overwrite the sole copy in place), so the shard needs
/// `2 × entry + framing` headroom. Below that the refusal is *structural*:
/// every re-stage of a near-block-size staged file fails regardless of how
/// idle the ring is, and the write path degrades to a per-op durable spill
/// / device-RMW-seed alternation — the deterministic
/// `staged_rmw_pooled_seeds` 100-of-200 storm signature, first visible on
/// ≥ 17-core boxes where `next_power_of_two(cores)` doubled the shard
/// count and halved shard capacity to exactly one entry.
///
/// The configured capacity budget is authoritative and never inflated
/// (the pre-fix sizing silently grew the ring to `shards × 4 MiB`);
/// the shard count adapts downward instead — fewer, larger shards trade
/// lock granularity for a structurally functional ring. Halving from a
/// power-of-two default preserves the power-of-two invariant
/// [`crate::tiering::nvme::NvmeCache::new`] asserts; the floor is one
/// shard (a sub-headroom total budget degrades to the designed loud
/// spill path, it does not brick construction).
///
/// Tiny pools (< 10 MiB/device) are one whole-pool shard unconditionally:
/// splitting them gains no lock parallelism worth having and shrinks the
/// largest admissible entry below shapes callers legitimately stage
/// (pre-fix behavior, pinned by the shard-full contract in
/// `tests/staging_budget_tests.rs`).
fn shard_plan(per_device_capacity: u64, default_shards: usize, max_entry_bytes: u64) -> usize {
    if per_device_capacity < 10 * 1024 * 1024 {
        return 1;
    }
    let min_shard = 2 * max_entry_bytes + SHARD_REPLACE_SLACK;
    let mut shards = std::cmp::max(default_shards, 1);
    while shards > 1 && per_device_capacity / (shards as u64) < min_shard {
        shards /= 2;
    }
    shards
}

impl NvmeStaging {
    /// `fs_generation` binds every staging dir to the mounted filesystem
    /// generation (`meta_backend::volume_set_generation`): the generation
    /// gate runs FIRST, so segment recovery, index rebuild, and
    /// budget/ledger seeding below only ever see same-generation content.
    /// `None` is for offline tooling with no metadata volume set (`clone`):
    /// existing staging is adopted untouched and never stamped.
    pub async fn new(
        staging_dirs: Vec<PathBuf>,
        max_write_bytes: u64,
        max_read_bytes: u64,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        fs_generation: Option<&str>,
    ) -> Result<Self> {
        // Cache-less filesystem (format declared NO --disk-cache-paths):
        // the ring objects still exist (anonymous memory shards) so every
        // hot-path type stays non-optional, but nothing is ever admitted —
        // `stage_write` fails loud, `put_active_block` refuses, and
        // `cache_read_block` is a no-op (see those methods). Clamp the
        // never-used anonymous maps to one tiny shard instead of letting
        // the disk-sized budgets reserve gigabytes of address space.
        let (max_write_bytes, max_read_bytes) = if staging_dirs.is_empty() {
            (1024 * 1024, 1024 * 1024)
        } else {
            (max_write_bytes, max_read_bytes)
        };

        // Generation gate (reformat-over-stale-staging fix): validate the
        // marker and discard dead-generation content BEFORE any segment is
        // scanned, mapped, or recovered.
        if let Some(fs_generation) = fs_generation {
            // §6.2 item 10: the generation may carry this node's scope, in
            // which case the root's content format may too (v3).
            let scoped = crate::writer_scope::split_staging_generation(fs_generation)
                .1
                .is_some();
            for dir in &staging_dirs {
                // VAL-7b: owner-only staging root.
                crate::config_ops::create_private_dir_all(dir)?;
                bind_staging_generation(dir, fs_generation).await?;
                // W2 §5.2: the staging content-format fence (future
                // versions refuse the segment as a unit — mount fails).
                validate_staging_format(dir, scoped).await?;
            }
        }

        // Initialize directories for segments.
        //
        // Read-cache segments come up COLD on purpose: the read cache is
        // keyed by bare block keys whose offsets are freed and REUSED
        // across sessions, and there is no cross-session incarnation store
        // to validate a recovered entry against (the fill-time seqlock
        // only guards live fills). A resurrected entry under a reused key
        // serves the PREVIOUS incarnation's bytes — the crash-recovery
        // twin of the reused-key stale-fill family. A cache is
        // reconstructible; stale-poisonable state is not worth recovering:
        // wipe it, never index it. The STAGING segments are the opposite —
        // sole copy of dirty data under identity-stable keys (uuid
        // file_ids, inode-keyed overlays) — and are recovered below.
        let mut read_cache_dirs = Vec::new();
        let mut staging_segment_dirs = Vec::new();
        let mut staging_segment_dirs_have_data = Vec::new();
        for dir in &staging_dirs {
            let rc_dir = dir.join("cache_segment");
            let ss_dir = dir.join("staging_segment");
            let ss_has_data = dir_has_segment_data(&ss_dir);
            let (rc_files, rc_bytes) = wipe_segment_files(&rc_dir)?;
            if rc_files > 0 {
                log::info!(
                    "staging init: discarded {rc_files} read-cache segment file(s) \
                     ({rc_bytes} B) from a previous session — read cache starts cold \
                     (block-key offsets are not incarnation-stable across mounts)"
                );
            }
            // VAL-7b: both segment trees are owner-only.
            crate::config_ops::create_private_dir_all(&rc_dir)?;
            crate::config_ops::create_private_dir_all(&ss_dir)?;
            read_cache_dirs.push(rc_dir);
            staging_segment_dirs.push(ss_dir);
            staging_segment_dirs_have_data.push(ss_has_data);
        }

        // Process parallelism, not the (possibly core-pinned) constructor
        // thread's mask — the Hang-1 sizing poison (see `crate::cpu`).
        let cores = crate::cpu::process_parallelism();
        let default_shards = std::cmp::max(cores.next_power_of_two(), 16);
        let max_entry_bytes = crate::routing::default_block_size();

        let read_cache_dirs_refs: Vec<&std::path::Path> =
            read_cache_dirs.iter().map(|p| p.as_path()).collect();
        let read_device_count = std::cmp::max(staging_dirs.len(), 1);
        let read_shards = shard_plan(
            max_read_bytes / read_device_count as u64,
            default_shards,
            max_entry_bytes,
        );
        let read_capacities =
            vec![(max_read_bytes as usize) / read_device_count; read_device_count];
        let read_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &read_cache_dirs_refs,
            &read_capacities,
            read_shards,
        )?);

        let staging_dirs_refs: Vec<&std::path::Path> =
            staging_segment_dirs.iter().map(|p| p.as_path()).collect();
        let write_device_count = std::cmp::max(staging_dirs.len(), 1);
        let per_device_write_capacity = max_write_bytes / write_device_count as u64;
        let write_shards = shard_plan(per_device_write_capacity, default_shards, max_entry_bytes);
        if !staging_dirs.is_empty()
            && per_device_write_capacity < 2 * max_entry_bytes + SHARD_REPLACE_SLACK
        {
            // Even one whole device ring cannot re-stage a block-size-class
            // entry in place (same-key replace holds two copies): every such
            // re-stage will take the durable spill path. Loud once at
            // construction — the per-op spill warn downstream is rate-limited
            // and easy to misread as transient pressure.
            log::warn!(
                "staging budget {} B/device is below the same-key replace headroom for \
                 {} B block-size-class entries ({} B needed): near-block-size staged \
                 files will spill durably on every rewrite",
                per_device_write_capacity,
                max_entry_bytes,
                2 * max_entry_bytes + SHARD_REPLACE_SLACK,
            );
        }
        let write_capacities =
            vec![(max_write_bytes as usize) / write_device_count; write_device_count];
        let staging_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &staging_dirs_refs,
            &write_capacities,
            write_shards,
        )?);

        // Recover the STAGING index only when segment data existed before
        // this startup (read-cache segments were wiped above — see the
        // init comment: never recover a cache under non-incarnation-stable
        // keys).
        if staging_segment_dirs_have_data
            .iter()
            .any(|has_data| *has_data)
        {
            staging_nvme_cache.recover_index();
        }

        // Seed the staged-write budget from recovered *staged* entries only.
        // Orphan active blocks (crash leftovers; uploaded+removed by later
        // flushes or unlink) must not consume the staged budget, or a mount
        // over a dirty segment starts with the admission gate already pinned.
        // The active-block occupancy index (RW2 predicate-2 probe) seeds
        // from the SAME recovered key list: a recovered `active_block:`
        // entry is a live overlay until recovery/flush/unlink removes it,
        // and the patch path must see it.
        let staged_ledger: std::sync::Arc<scc::HashMap<String, (u64, u64)>> =
            std::sync::Arc::new(scc::HashMap::new());
        let active_block_index: std::sync::Arc<scc::HashMap<String, ()>> =
            std::sync::Arc::new(scc::HashMap::new());
        let mut initial_write_bytes = 0u64;
        let mut initial_active_block_custody = 0usize;
        let mut foreign_scope_records = 0usize;
        for key in staging_nvme_cache.list_keys() {
            // §6.2 item 8 — the record-level classification, applied
            // BEFORE any adoption: a record scoped to another node (or
            // scoped at all while we are unscoped) is never indexed as our
            // active-block custody, never budget-counted, and never
            // flushed or freed. It is left in place, counted, and logged
            // loud once: its payload belongs to that node's ring.
            // Unscoped (legacy) records are ours by grandfathering — the
            // D0 single-writer guard governed the mount that wrote them.
            if let Ok(k) = std::str::from_utf8(&key) {
                if !crate::writer_scope::key_is_mine(k) {
                    foreign_scope_records += 1;
                    continue;
                }
            }
            if std::str::from_utf8(&key).is_ok_and(key_is_block_family) {
                // Whole-image `active_block:` records AND `active_block_ext:`
                // extent records: occupancy-indexed (the lock-free probes),
                // never budget-counted (custody overlays, not staged files).
                if let Ok(k) = std::str::from_utf8(&key) {
                    if active_block_index.insert_sync(k.to_string(), ()).is_ok() {
                        initial_active_block_custody += 1;
                    }
                }
                continue;
            }
            let Ok(file_id) = std::str::from_utf8(&key) else {
                continue;
            };
            if let Some(guard) = staging_nvme_cache.get(&key) {
                let cost = guard.len as u64;
                initial_write_bytes += cost;
                let _ = staged_ledger.insert_sync(file_id.to_string(), (cost, 0));
            }
        }
        if foreign_scope_records > 0 {
            // The forward-detection loud line for §6.2 item 8, same class
            // as the extent-record sweep's: a record we cannot claim is
            // left intact and untouched, and something is wrong with the
            // staging-root layout (a peer's ring reachable from here).
            crate::fuse_client::METRICS
                .staging_foreign_scope_records
                .fetch_add(
                    foreign_scope_records as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            let msg = format!(
                "STAGING RECORDS OF ANOTHER WRITER: {foreign_scope_records} recovered \
                 staging record(s) carry a writer scope this mount cannot claim — NOT \
                 adopted, NOT budget-counted, left intact (their payload rings belong to \
                 the writing node). Counted in staging_foreign_scope_records"
            );
            eprintln!("{msg}");
            log::warn!("{msg}");
        }

        // P1-1: bound the merge worker queue to avoid unbounded RAM growth under write storms.
        const STAGING_MERGE_QUEUE_CAP: usize = 1024;
        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(STAGING_MERGE_QUEUE_CAP);

        let backend_router = std::sync::Arc::new(once_cell::sync::OnceCell::new());

        let staging = Self {
            staging_dirs: staging_dirs.clone(),
            max_write_bytes,
            max_read_bytes,
            block_allocator: block_allocator.clone(),
            nvme_writer: nvme_writer.clone(),
            backend_router,
            write_tx,
            current_staged_write_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_write_bytes,
            )),
            staged_ledger,
            active_block_index,
            active_block_custody: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                initial_active_block_custody,
            )),
            data_router: std::sync::Arc::new(std::sync::OnceLock::new()),
            space_freed_notify: std::sync::Arc::new(squeezefs_ipc::sqz_notify::Notify::new()),
            staged_writes_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                staging_nvme_cache.list_keys().len(),
            )),
            staged_drained_notify: std::sync::Arc::new(squeezefs_ipc::sqz_notify::Notify::new()),
            read_nvme_cache,
            staging_nvme_cache,
            crypto: std::sync::Arc::new(std::sync::OnceLock::new()),
        };

        // Spawn background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
    }

    pub fn set_backend_router(&self, router: std::sync::Arc<crate::routing::BackendRouter>) {
        let _ = self.backend_router.set(std::sync::Arc::downgrade(&router));
    }

    /// Late-bind the owning `DataRouter` (weak) for merge-worker promotion.
    pub(crate) fn set_data_router(&self, router: std::sync::Weak<crate::routing::DataRouterInner>) {
        let _ = self.data_router.set(router);
    }

    pub fn get_staged_path(&self, _file_id: &str) -> PathBuf {
        self.staging_dirs.first().cloned().unwrap_or_default()
    }

    /// Stage a write locally into staging_nvme_cache using zero-copy memory-mapped segments.
    ///
    /// `data` is owned (`Bytes`, refcounted — no payload copy) because the
    /// ring write takes the staging shard WRITE lock, which must run on the
    /// blocking pool (shard-lock invariant rule 2, `tiering::nvme::NvmeShard`):
    /// a shard writer legitimately waits for §5.5 read guards held across
    /// awaits, so acquiring it on an async executor thread can park the very
    /// thread that must poll the guard holder (the Hang-1 wedge shape).
    pub async fn stage_write(
        &self,
        file_path: &str,
        file_id: &str,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: file_path.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;
        let unpadded_len = 8 + meta_bytes.len() + data.len();
        let padded_size = unpadded_len as u64;

        let target_dir = self.staging_dirs.first().ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "no staging directories declared at format (cache-less filesystem)",
            ))
        })?;

        if !check_disk_free_safeguard(target_dir) {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "Local NVMe staging disk free space safeguard triggered (< 1% or < 100MB free)"
                    .to_string(),
            )));
        }

        // Re-staging the same file_id replaces its ring entry: the gate must
        // charge the *delta*, not the sum of every intermediate payload.
        //
        // LEDGER-LOCK INVARIANT (the VL8 generic/464 writes-only wedge —
        // tests/staging_shard_deadlock_tests.rs): every executor-side
        // ledger access must be an scc `*_async` op, never `*_sync`. A
        // concurrent re-stage's blocking-pool closure holds this file_id's
        // ledger ENTRY lock across `reserve_and_write`'s shard-WRITE-lock
        // wait, and that wait is unbounded while a §5.5 read guard is
        // parked on an await — a `read_sync` here then BLOCKS the very
        // executor thread that must poll the guard holder (the captured
        // four-edge cycle: executor → bucket → shard write → guard →
        // executor). `read_async` parks the TASK instead; the guard holder
        // stays pollable and the cycle cannot close.
        let prior_cost = self
            .staged_ledger
            .read_async(file_id, |_, (cost, _)| *cost)
            .await
            .unwrap_or(0);

        let over_cap =
            |cur: u64| cur.saturating_sub(prior_cost) + padded_size > self.max_write_bytes;

        let mut total_staged_bytes = self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        if over_cap(total_staged_bytes) {
            // Capacity pressure: promote resident staged entries to durable
            // backend blocks so the pool drains, then wait (bounded) for the
            // merge worker to credit freed space. Never a fixed futile stall.
            self.kick_promotion(file_id, 64).await;
            let deadline = std::time::Instant::now() + Duration::from_millis(2000);
            loop {
                if squeezefs_ipc::sqz_time::timeout_at(deadline, self.space_freed_notify.notified())
                    .await
                    .is_err()
                {
                    break;
                }
                total_staged_bytes = self
                    .current_staged_write_bytes
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !over_cap(total_staged_bytes) {
                    break;
                }
                self.kick_promotion(file_id, 64).await;
            }
            total_staged_bytes = self
                .current_staged_write_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
        }

        if over_cap(total_staged_bytes) {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging cache capacity exceeded: current {} bytes, writing {} bytes, max capacity {} bytes",
                    total_staged_bytes, padded_size, self.max_write_bytes
                )
            )));
        }

        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());

        // Ring write + ledger charge under this file_id's ledger entry lock:
        // a concurrent promotion/unlink of the same id either completes fully
        // before (and sees the old generation) or after (and sees the bumped
        // generation) — it can never remove the ring entry we just wrote.
        // spawn_blocking: the ring write is a staging shard WRITE lock
        // (shard-lock invariant rule 2 — never park an async executor on it).
        let (admitted, replaced_cost, is_new) = {
            let ledger = self.staged_ledger.clone();
            let staging = self.staging_nvme_cache.clone();
            let file_id_owned = file_id.to_string();
            let key_clone = key_bytes.clone();
            let data_clone = data.clone();
            squeezefs_ipc::sqz_blocking::run_blocking(move || {
                let mut entry = ledger.entry_sync(file_id_owned).or_insert((0, 0));
                let is_new = staging.get(&key_clone).is_none();
                // Memory-mapped copy directly (lock-free, zero disk syscall wait).
                // The segment never destroys live entries: refusal here is loud
                // backpressure and the caller escalates to a durable spill.
                let admitted = staging.reserve_and_write(
                    key_clone.clone(),
                    meta_len,
                    &meta_bytes,
                    &data_clone,
                    None,
                );
                if admitted {
                    let (cost, _) = *entry.get();
                    *entry.get_mut() = (padded_size, next_stage_generation());
                    (true, cost, is_new)
                } else {
                    // Drop a placeholder created for this refused stage.
                    if entry.get().0 == 0 {
                        let _ = entry.remove();
                    }
                    (false, 0, false)
                }
            })
            .await
        };
        if !admitted {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging segment cannot admit {} bytes without destroying live staged entries",
                    padded_size
                ),
            )));
        }
        self.current_staged_write_bytes
            .fetch_add(padded_size, std::sync::atomic::Ordering::Relaxed);
        if replaced_cost > 0 {
            Self::sub_saturating(&self.current_staged_write_bytes, replaced_cost);
            self.space_freed_notify.notify_waiters();
        }

        if is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        log::debug!(
            "NVMe Staging: Staged write for file {} (ID: {}) size = {} bytes",
            file_path,
            file_id,
            data.len()
        );

        // Keep the hot path enqueue-free (promoting every small stage to the
        // backend competed with create/fsync), but arm the drain *before* the
        // pool hard-fills: past the high-water mark, ask the merge worker to
        // promote this entry in the background.
        let high_water = self.max_write_bytes - self.max_write_bytes / 4;
        if self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            > high_water
        {
            self.try_enqueue_staged_merge(file_path, file_id, fencing_token, padded_size);
        }
        Ok(())
    }

    /// Saturating subtract on the staged-budget gauge. Protocol core:
    /// [`crate::gauge_core`] (loom-model-checked).
    fn sub_saturating(gauge: &std::sync::atomic::AtomicU64, amount: u64) {
        crate::gauge_core::sub_saturating(gauge, amount);
    }

    /// Ask the merge worker to promote up to `max_items` resident staged
    /// entries (excluding `exclude_file_id`, whose newest payload is the one
    /// being staged right now). Best-effort: a full queue means promotion is
    /// already in flight.
    /// Async by the ledger-lock invariant (see `stage_write`'s prior-cost
    /// read): the walk visits every bucket, and a bucket whose entry lock
    /// is held across a shard-write wait must park this TASK, never the
    /// executor thread. Both callers are `stage_write` (async context).
    async fn kick_promotion(&self, exclude_file_id: &str, max_items: usize) {
        for item in self
            .collect_resident_staged(Some(exclude_file_id), max_items)
            .await
        {
            if self.write_tx.try_send(item).is_err() {
                break;
            }
        }
    }

    /// Up to `max_items` budget-counted staged-layout entries (minus
    /// `exclude_file_id`), each with its ring header's path and stage-time
    /// fencing token — the promotion work list. An entry whose header is
    /// unreadable is left out (the ring is authoritative; the ledger row
    /// alone cannot name the inode). Async by the ledger-lock invariant
    /// (`stage_write`'s prior-cost read).
    async fn collect_resident_staged(
        &self,
        exclude_file_id: Option<&str>,
        max_items: usize,
    ) -> Vec<PendingStagedWrite> {
        let mut pending: Vec<(String, u64)> = Vec::new();
        self.staged_ledger
            .iter_async(|file_id, (cost, _)| {
                if exclude_file_id != Some(file_id.as_str()) {
                    pending.push((file_id.clone(), *cost));
                }
                pending.len() < max_items
            })
            .await;
        pending
            .into_iter()
            .filter_map(|(file_id, cost)| {
                let meta = self.staged_meta_of(&file_id)?;
                Some(PendingStagedWrite {
                    file_path: meta.file_path,
                    file_id,
                    fencing_token: meta.fencing_token,
                    padded_size: cost,
                })
            })
            .collect()
    }

    /// Every resident staged-layout entry — the dismount promotion pass's
    /// work list (`SqueezefsFilesystem::promote_all_staged_files_at_dismount`).
    pub(crate) async fn resident_staged_files(&self) -> Vec<PendingStagedWrite> {
        self.collect_resident_staged(None, usize::MAX).await
    }

    /// Read the staged header for `file_id` from the ring (no payload copy).
    fn staged_meta_of(&self, file_id: &str) -> Option<StagedMetadata> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() < 8 {
            return None;
        }
        let meta_len = u64::from_be_bytes(bytes[0..8].try_into().ok()?) as usize;
        if bytes.len() < 8 + meta_len {
            return None;
        }
        StagedMetadata::deserialize(&bytes[8..8 + meta_len])
    }

    /// Current stage generation of `file_id`, if it is budget-counted.
    /// Async by the ledger-lock invariant (see `stage_write`'s prior-cost
    /// read): every caller is async-context (promotion commit, the read
    /// path's in-flight-identity retry), and the bucket's entry lock is
    /// legitimately held across shard-write waits — park the task, never
    /// the executor thread.
    pub async fn staged_generation(&self, file_id: &str) -> Option<u64> {
        self.staged_ledger
            .read_async(file_id, |_, (_, gen)| *gen)
            .await
    }

    /// Remove `file_id`'s ring entry and return its budget **iff** its stage
    /// generation still equals `gen`. A racing re-stage bumps the generation
    /// first (under the same ledger entry lock), so its fresh ring entry and
    /// budget are never destroyed by a promotion that raced it.
    pub fn remove_staged_if_generation(&self, file_id: &str, gen: u64) -> bool {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let removed_cost = {
            let scc::hash_map::Entry::Occupied(entry) =
                self.staged_ledger.entry_sync(file_id.to_string())
            else {
                return false;
            };
            let (cost, cur_gen) = *entry.get();
            if cur_gen != gen {
                return false;
            }
            if self.staging_nvme_cache.remove(&key_bytes[..]).is_some() {
                self.staged_writes_in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = entry.remove();
            cost
        };
        Self::sub_saturating(&self.current_staged_write_bytes, removed_cost);
        self.space_freed_notify.notify_waiters();
        true
    }

    /// Payload length of `file_id`'s ring entry, if resident — a header
    /// peek (no image copy), for truncate's shrink decision.
    pub fn staged_len(&self, file_id: &str) -> Option<u64> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() < 8 {
            return None;
        }
        let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
        if bytes.len() < 8 + meta_len {
            return None;
        }
        StagedMetadata::deserialize(&bytes[8..8 + meta_len]).map(|m| m.original_size)
    }

    /// Logically shrink `file_id`'s ring blob to `new_size` IN PLACE — the
    /// truncate-shrink path. Unlike a `stage_write` re-stage this needs no
    /// segment placement (an 8-byte header patch of the live extent, see
    /// `NvmeShard::shrink_staged_value`), so ring pressure can never refuse
    /// it — refusal is exactly how pre-truncate bytes used to survive and
    /// resurface through the next truncate-up.
    ///
    /// Runs under this file_id's ledger entry lock and bumps the stage
    /// generation, so an IN-FLIGHT promotion that read the pre-shrink image
    /// fails its commit-time generation check (`remove_staged_if_generation`
    /// / `promote_staged_file`) instead of publishing a stale-long durable
    /// copy over the clip. Blocking-pool hop: the shard patch takes the
    /// staging shard WRITE lock (shard-lock invariant rule 2).
    ///
    /// Returns whether a live ring entry was clipped (`false` = no entry —
    /// the promoted/spilled case, handled by the caller's durable clip).
    pub async fn shrink_staged(&self, file_id: &str, new_size: u64) -> bool {
        let ledger = self.staged_ledger.clone();
        let staging = self.staging_nvme_cache.clone();
        let file_id_owned = file_id.to_string();
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            let scc::hash_map::Entry::Occupied(mut entry) = ledger.entry_sync(file_id_owned) else {
                return false;
            };
            if !staging.shrink_staged_value(&key_bytes, new_size) {
                return false;
            }
            let (cost, _) = *entry.get();
            // Cost stays the extent's charge (the slot doesn't move);
            // conservative in the safe direction for the admission gate.
            *entry.get_mut() = (cost, next_stage_generation());
            true
        })
        .await
    }

    /// Bump `file_id`'s stage generation under its ledger entry lock (the
    /// W2 rider-write fence): an IN-FLIGHT promotion that read the image
    /// before this rider write's record merge fails its commit-time
    /// generation check (`remove_staged_if_generation`) instead of
    /// publishing an image that lost the record's extents. Blocking-pool
    /// hop for async callers.
    pub async fn bump_staged_generation(&self, file_id: &str) {
        let ledger = self.staged_ledger.clone();
        let fid = file_id.to_string();
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            if let scc::hash_map::Entry::Occupied(mut entry) = ledger.entry_sync(fid) {
                let (cost, _) = *entry.get();
                *entry.get_mut() = (cost, next_stage_generation());
            }
        })
        .await;
    }

    /// Ask the background merge worker to promote a staged file_id (best-effort).
    pub fn try_enqueue_staged_merge(
        &self,
        file_path: &str,
        file_id: &str,
        fencing_token: u64,
        padded_size: u64,
    ) {
        let pending = PendingStagedWrite {
            file_path: file_path.to_string(),
            file_id: file_id.to_string(),
            fencing_token,
            padded_size,
        };
        let _ = self.write_tx.try_send(pending);
    }

    /// Put a packed active block write to staging_nvme_cache.
    ///
    /// Returns `false` when the segment cannot admit the block without
    /// destroying live entries; the caller must keep the data (RAM buffer)
    /// or upload it durably — never drop it.
    #[must_use]
    pub fn put_active_block(&self, key: &str, data: &[u8], fencing_token: u64) -> bool {
        // Cache-less filesystem: refuse admission. Every caller already
        // implements the never-lossy `admitted == false` path (keep the
        // RAM buffer / escalate to a durable upload), which IS the
        // cache-less design: RAM tiers + direct block I/O only.
        if self.staging_dirs.is_empty() {
            return false;
        }
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: key.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;

        let key_bytes = Bytes::copy_from_slice(key.as_bytes());

        // Conservative-present occupancy index (see the field doc): indexed
        // BEFORE the ring write so the patch predicate can never miss a
        // just-admitted overlay; un-indexed on refusal (nothing staged).
        let indexed_here = self
            .active_block_index
            .insert_sync(key.to_string(), ())
            .is_ok();
        if indexed_here {
            self.active_block_custody
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let is_new = self.staging_nvme_cache.get(&key_bytes).is_none();
        let admitted = self.staging_nvme_cache.reserve_and_write(
            key_bytes,
            meta_len,
            &meta_bytes,
            data,
            Some(4096),
        );
        if admitted && is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if !admitted && indexed_here {
            // Refused and WE created the index entry: retract it. (Same-key
            // put/remove callers hold the block's BLOCK_FLUSH_LOCKS stripe,
            // so a pre-existing entry — insert refused — belongs to a live
            // staged sibling and must survive this refusal.)
            if self.active_block_index.remove_sync(key).is_some() {
                self.release_active_block_custody();
            }
        }
        admitted
    }

    /// Stage a W2 extent record (design-random-small-writes §5.2) under an
    /// `active_block_ext:` key — the 4 KiB-class spill of a parked extent
    /// overlay. **No seed read at spill, ever** (the record carries only
    /// the app-written runs); same never-lossy admission contract as
    /// [`Self::put_active_block`]: `false` = the ring refused, the caller
    /// keeps the RAM overlay. Same-key re-spills replace crash-safely
    /// (`reserve_and_write` keeps the old copy live until the replacement
    /// is fully written — the FIND-VS-B never-shrink/replace-headroom
    /// invariants apply to this record kind verbatim). Sync (staging shard
    /// WRITE lock): call on the blocking pool per invariant rule 2.
    #[must_use]
    pub fn put_extent_record(&self, key: &str, record: &ExtentRecord) -> bool {
        debug_assert!(key.starts_with("active_block_ext:"));
        self.put_active_block(key, &record.serialize(), record.fencing_token)
    }

    /// Read + parse the staged extent record under `key`. `None` = no
    /// record; `Some(Err(_))` = a record blob exists but refuses to parse
    /// (torn/foreign ⇒ [`ExtentRecordError::Torn`], newer-binary content ⇒
    /// [`ExtentRecordError::FutureVersion`]) — consumers dispose LOUDLY per
    /// the §5.2 contract, never silently.
    pub fn read_extent_record(
        &self,
        key: &str,
    ) -> Option<std::result::Result<ExtentRecord, ExtentRecordError>> {
        let raw = self.read_staged(key)?;
        Some(ExtentRecord::deserialize(&raw))
    }

    /// In-place REWRITE of a staged extent record with a shorter one (the
    /// truncate-clip form): an in-extent patch that ring pressure can
    /// never refuse (see `NvmeShard::patch_block_family_value`). Sync
    /// (shard WRITE lock) — call on the blocking pool.
    #[must_use]
    pub fn rewrite_extent_record_in_place(&self, key: &str, record: &ExtentRecord) -> bool {
        debug_assert!(key.starts_with("active_block_ext:"));
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        self.staging_nvme_cache
            .patch_block_family_value(&key_bytes, &record.serialize())
    }

    /// Lock-free staged extent-record existence probe (the read/write hot
    /// paths' zero-cost gate: one latch-free occupancy-index read; when the
    /// map is empty the cost is exactly today's miss — R6).
    pub fn has_staged_extent_record(&self, key: &str) -> bool {
        debug_assert!(key.starts_with("active_block_ext:"));
        self.active_block_index.read_sync(key, |_, _| ()).is_some()
    }

    /// Keys of every staged entry (`active_block:` AND
    /// `active_block_ext:`) whose key starts with `prefix`. Served from
    /// the latch-free occupancy index — the delete/reclaim sweep
    /// (O(present), the fix for the O(logical-size) teardown linger —
    /// fstests generic/294/306/452 family), never a hot path.
    pub fn staged_keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.active_block_index.iter_sync(|k, _| {
            if k.starts_with(prefix) {
                out.push(k.clone());
            }
            true
        });
        out
    }

    /// Keys of every staged extent record whose key starts with `prefix`
    /// (`""` = all). Served from the occupancy index — recovery sweeps and
    /// fsync drains, never a hot path.
    pub fn extent_record_keys(&self, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.active_block_index.iter_sync(|k, _| {
            if k.starts_with("active_block_ext:") && k.starts_with(prefix) {
                out.push(k.clone());
            }
            true
        });
        out
    }

    /// **Lock-free staged-existence probe** (design-random-small-writes
    /// §5.1 predicate 2 / review Issue 10): does a staged `active_block:`
    /// entry exist for `key`? Served from a latch-free occupancy index —
    /// the W1 patch hot path must NOT inherit the per-write
    /// `spawn_blocking` + staging-shard-WRITE-lock hop (the H1 class), and
    /// `NvmeCache::contains`'s `read_recursive` still parks behind an
    /// ACTIVE writer's ms-class critical section.
    ///
    /// Coherence contract (conservative-present — the corruption-safe
    /// direction): any window in which a staged entry EXISTS for `key` has
    /// the key present in the index (false positives are harmless — the
    /// caller falls back to the accumulation path; a false NEGATIVE would
    /// let a patch race a pending writeback flush of stale staged bytes).
    pub fn has_staged_active_block(&self, key: &str) -> bool {
        self.active_block_index.read_sync(key, |_, _| ()).is_some()
    }

    /// One `active_block_index` entry left: credit the custody population
    /// and wake the dismount drain wait on the last one.
    fn release_active_block_custody(&self) {
        let prev = self
            .active_block_custody
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if prev == 1 {
            self.staged_drained_notify.notify_waiters();
        }
    }

    /// Our `active_block[_ext]:` custody records still staged — the
    /// population the dismount sweep and the writeback worker retire, and
    /// therefore the dismount drain wait's predicate.
    pub fn active_block_custody_count(&self) -> usize {
        self.active_block_custody
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Remove a packed active block write from staging_nvme_cache.
    pub fn remove_active_block(&self, key: &str) -> Option<Vec<u8>> {
        let val = self.read_staged(key);
        if self.staging_nvme_cache.remove(key.as_bytes()).is_some() {
            self.staged_writes_in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        // Occupancy index: un-index strictly AFTER the ring removal
        // (conservative-present — see the field doc). `remove_staged`
        // routes plain file_id keys here too; those were never indexed and
        // the remove is a no-op for them.
        if self.active_block_index.remove_sync(key).is_some() {
            self.release_active_block_custody();
        }
        // Return the budget of a counted staged entry (unlink, spill purge,
        // layout transition). Active-block keys are never in the ledger.
        if let Some((_, (cost, _))) = self.staged_ledger.remove_sync(key) {
            Self::sub_saturating(&self.current_staged_write_bytes, cost);
            self.space_freed_notify.notify_waiters();
        }
        // Moving-custody read protocol (generic/795): a staged sibling /
        // extent record retire is a custody transfer a lock-free reader can
        // be inverted by — bump the block's custody epoch so in-window
        // readers re-run (see `fuse_client::BLOCK_CUSTODY_EPOCHS`).
        if let Some((ino, b)) = parse_block_custody_key(key) {
            crate::fuse_client::bump_block_custody_epoch(ino, b);
        }
        val
    }

    /// Remove a staged write from staging_nvme_cache.
    pub fn remove_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        self.remove_active_block(file_id)
    }

    // ---- blocking-pool variants (shard-lock invariant rule 2) ----
    //
    // The staging shard WRITE lock legitimately waits for §5.5 read guards
    // held across awaits (`tiering::nvme::NvmeShard` doc), so acquiring it
    // on an async executor thread can park the very thread that must poll
    // the guard holder — the Hang-1 total-daemon wedge. Async contexts use
    // these wrappers; the sync originals remain for blocking contexts
    // (existing `spawn_blocking` closures, the merge worker, tests).

    /// [`Self::remove_active_block`] on the blocking pool, for async callers.
    pub async fn remove_active_block_async(&self, key: String) -> Result<Option<Vec<u8>>> {
        let nvme = self.clone();
        Ok(squeezefs_ipc::sqz_blocking::run_blocking(move || nvme.remove_active_block(&key)).await)
    }

    /// Remove many active blocks in ONE blocking-pool hop (delete/reclaim
    /// paths sweep every block index of an inode).
    pub async fn remove_active_blocks_async(&self, keys: Vec<String>) -> Result<()> {
        let nvme = self.clone();
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            for key in keys {
                nvme.remove_active_block(&key);
            }
        })
        .await;
        Ok(())
    }

    /// [`Self::remove_staged`] on the blocking pool, for async callers.
    pub async fn remove_staged_async(&self, file_id: String) -> Result<Option<Vec<u8>>> {
        self.remove_active_block_async(file_id).await
    }

    /// [`Self::remove_staged_if_generation`] on the blocking pool, for async callers.
    pub async fn remove_staged_if_generation_async(
        &self,
        file_id: String,
        gen: u64,
    ) -> Result<bool> {
        let nvme = self.clone();
        Ok(squeezefs_ipc::sqz_blocking::run_blocking(move || {
            nvme.remove_staged_if_generation(&file_id, gen)
        })
        .await)
    }

    /// [`Self::put_active_block`] on the blocking pool, for async callers.
    /// `data` is `Bytes` (refcounted) — no payload copy.
    pub async fn put_active_block_async(
        &self,
        key: String,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<bool> {
        let nvme = self.clone();
        Ok(squeezefs_ipc::sqz_blocking::run_blocking(move || {
            nvme.put_active_block(&key, &data, fencing_token)
        })
        .await)
    }

    /// [`Self::read_staged`] into a caller-provided pooled buffer — the
    /// staged-RMW seed path (follow-up C): the whole-image copy lands in a
    /// recycled `BUFFER_POOL` backing instead of a fresh `Vec` per
    /// sub-block write (dhat-measured as the aged-daemon allocation flood:
    /// ~4 MiB malloc/free per op at storm rates). Same guard discipline as
    /// `read_staged`; returns whether the image was found (the buffer is
    /// resized to the image length on success, contents exact).
    pub fn read_staged_into(&self, file_id: &str, buf: &mut crate::cache::pool::PooledBuf) -> bool {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let Some(guard) = self.staging_nvme_cache.get(&key_bytes) else {
            return false;
        };
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() < 8 {
            return false;
        }
        let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
        if bytes.len() < 8 + meta_len {
            return false;
        }
        let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) else {
            return false;
        };
        let data_start = if key_is_block_family(file_id) {
            4096
        } else {
            8 + meta_len
        };
        let data_end = data_start + meta.original_size as usize;
        if bytes.len() < data_end {
            return false;
        }
        buf.resize(meta.original_size as usize, 0);
        buf.copy_from_slice(&bytes[data_start..data_end]);
        true
    }

    /// Read staged data directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    let data_start = if key_is_block_family(file_id) {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + meta.original_size as usize;
                    if bytes.len() >= data_end {
                        return Some(bytes[data_start..data_end].to_vec());
                    }
                }
            }
        }
        None
    }

    /// Read staged fencing token directly from staging_nvme_cache memory-mapped segments without copying data.
    pub fn get_staged_fencing_token(&self, file_id: &str) -> Option<u64> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    return Some(meta.fencing_token);
                }
            }
        }
        None
    }

    /// Take the §5.5 write-only DMA source for a staged entry: a guard-backed,
    /// zero-copy view of the payload for [`write_block_from_staging`].
    /// Holding it pins the entry's shard against writers/evictors, so take it
    /// immediately before the upload and let the helper consume it.
    pub fn staged_dma_source(&self, key: &str) -> Option<StagedDmaSource> {
        self.read_staged_zero_copy(key)
            .map(|guard| StagedDmaSource { guard })
    }

    /// Read staged data zero-copy directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged_zero_copy(
        &self,
        file_id: &str,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let mut guard = self.staging_nvme_cache.get_static(file_id.as_bytes())?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                // Zero-alloc header peek (op-economy campaign): this leg
                // only needs `original_size` — same field/UTF-8
                // validation as `deserialize`, no `file_path` String.
                if let Some(original_size) =
                    StagedMetadata::peek_original_size(&bytes[8..8 + meta_len])
                {
                    let data_start = if key_is_block_family(file_id) {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + original_size as usize;
                    if bytes.len() >= data_end {
                        guard.offset += data_start;
                        guard.len = original_size as usize;
                        return Some(guard);
                    }
                }
            }
        }
        None
    }

    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let data_router = self.data_router.clone();
        let gauge = self.current_staged_write_bytes.clone();
        let high_water = self.max_write_bytes - self.max_write_bytes / 4;

        // Sweep 2026-08-13: the staging merge LOOP is write-custody-
        // critical — sqz-meta venue.
        crate::meta_exec::spawn_meta("staging_merge_worker", async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024;
            let flush_timeout = Duration::from_millis(500);

            loop {
                // Recv-vs-flush-deadline (the retired two-arm select):
                // a fresh deadline per iteration, exactly like the
                // per-iteration `sleep` it replaces.
                match squeezefs_ipc::sqz_time::timeout(flush_timeout, write_rx.recv()).await {
                    Ok(Some(pending)) => {
                        current_bytes += pending.padded_size;
                        batch.push(pending);

                        // Promote immediately under capacity pressure
                        // (writers may be gate-blocked on freed space);
                        // otherwise batch up to amortize.
                        if current_bytes >= max_batch_bytes
                            || gauge.load(std::sync::atomic::Ordering::Relaxed) > high_water
                        {
                            Self::promote_batch(&data_router, &mut batch).await;
                            current_bytes = 0;
                        }
                    }
                    Ok(None) => {
                        if !batch.is_empty() {
                            info!(
                                "NVMe Staging: Channel closed. Promoting remaining {} staged writes.",
                                batch.len()
                            );
                            Self::promote_batch(&data_router, &mut batch).await;
                        }
                        break;
                    }
                    Err(_) => {
                        if !batch.is_empty() {
                            Self::promote_batch(&data_router, &mut batch).await;
                            current_bytes = 0;
                        }
                    }
                }
            }
        });
    }

    /// Promote every distinct pending staged file out of the ring through
    /// the owning router (coherent RAM + backend layout commit; the router
    /// dispatches on size — inline record or durable block). Conservative
    /// on any miss: the entry stays resident and budget-counted.
    async fn promote_batch(
        data_router: &std::sync::OnceLock<std::sync::Weak<crate::routing::DataRouterInner>>,
        batch: &mut Vec<PendingStagedWrite>,
    ) {
        let Some(router) = data_router.get().and_then(std::sync::Weak::upgrade) else {
            // Router not wired (shutdown or partially constructed cache):
            // drop the notices, entries stay resident + counted.
            batch.clear();
            return;
        };
        let router = crate::routing::DataRouter::from_inner(router);
        // A promotion BATCH (design-small-file-packing OQ-1): re-arm the
        // pack arm after a StorageFull stop; a stop inside this batch
        // leaves its remaining entries resident-and-counted.
        router.packer.begin_promotion_batch();

        let mut seen = std::collections::HashSet::new();
        for item in batch.drain(..) {
            if !seen.insert(item.file_id.clone()) {
                continue;
            }
            crate::coz_progress!("nvme_staged_promotion");
            match router
                .promote_staged_file(&item.file_path, &item.file_id, item.fencing_token)
                .await
            {
                Ok(Some(into)) => info!(
                    "NVMe Staging: promoted staged file {} (ID: {}) {}",
                    item.file_path,
                    item.file_id,
                    match into {
                        crate::routing::PromotedInto::Inline => "inline (layout record)",
                        crate::routing::PromotedInto::Packed => "into the open pack block",
                        crate::routing::PromotedInto::Block => "to durable block",
                    }
                ),
                Ok(None) => {}
                Err(e) => error!(
                    "NVMe Staging: promotion failed for {} (ID: {}): {:?} — entry stays resident",
                    item.file_path, item.file_id, e
                ),
            }
        }
    }

    pub fn staging_dirs(&self) -> &[PathBuf] {
        &self.staging_dirs
    }

    /// Cache a block of read data on local NVMe using read_nvme_cache.
    pub fn cache_read_block(&self, block_key: &str, data: Bytes) -> Result<()> {
        // Cache-less filesystem: no NVMe read-cache tier — dehydration of
        // RAM-LRU evictions is a clean no-op (the block stays readable
        // from the backend; only the local cache tier is absent).
        if self.staging_dirs.is_empty() {
            return Ok(());
        }
        // R5 finding-#2 escalation (§5.7): while the authority's
        // unreclaimable arm rides the Red band, tier publishes are
        // PAUSED — this is the single funnel every producer (fill path,
        // dehydration) routes through, so one gate covers the
        // population by construction. Never-lossy: the tier is a read
        // cache; absence means the next reader goes to the device.
        // Measured pre-fix: 5.06 GiB of tier writes in ~10 s against a
        // 5 GiB budget while every sheddable component already sat at its
        // floor.
        if crate::mem_budget::tier_publish_paused() {
            crate::fuse_client::METRICS
                .read_tier_publishes_paused
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let val_bytes = data;
        // Evictions are discarded here, so use the non-materializing put:
        // the materializing flavor pays an mmap page-in + memcpy of every
        // victim payload under the shard write lock (measured 44% daemon
        // CPU in memcpy + ~6x spurious device reads on the elbencho
        // O_DIRECT sequential-read row) for bytes nothing consumes.
        self.read_nvme_cache
            .put_discard_evicted(key_bytes, val_bytes);

        Ok(())
    }

    /// [`Self::cache_read_block`] with the finding-17 ATOMIC insert-time
    /// validation: `validate` runs under the target shard's write lock
    /// immediately before the entry becomes index-visible, so a deposit
    /// whose incarnation moved can never be observed by a reader — the
    /// put-then-revalidate-then-undo shape this replaces exposed its undo
    /// window (the data_path_correctness ~5 %/run stale-serve flake).
    /// Returns whether the entry is published; a refusal is a plain cache
    /// miss for the next reader.
    pub fn cache_read_block_if(
        &self,
        block_key: &str,
        data: Bytes,
        validate: &dyn Fn() -> bool,
    ) -> bool {
        if self.staging_dirs.is_empty() {
            return false;
        }
        if crate::mem_budget::tier_publish_paused() {
            crate::fuse_client::METRICS
                .read_tier_publishes_paused
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        self.read_nvme_cache
            .put_discard_evicted_validated(key_bytes, data, validate)
    }

    /// INCARNATION-VALIDATED read-cache publish — the only legal route for
    /// non-owner publishes (RAM-LRU dehydration). Their
    /// payloads can be arbitrarily stale (an evicted entry parked in the
    /// dehydration channel, a peer store in flight), so an unconditional
    /// `cache_read_block` could stick a DEAD incarnation's bytes under a
    /// freed-and-reallocated key — served for the key's next owner (the
    /// generic/074 fstest.3 stale-fill family). Same discipline as the
    /// routing validated fill: snapshot the key's incarnation and publish
    /// through the ATOMIC insert-time validation (finding 17 — the former
    /// publish-then-undo shape exposed its undo window to readers).
    /// Untracked legacy keys (never freed/reallocated) are vacuously
    /// valid.
    ///
    /// Returns whether the entry is published.
    pub fn cache_read_block_validated(
        &self,
        block_key: &str,
        data: Bytes,
        backend_router: &crate::routing::BackendRouter,
    ) -> bool {
        if !backend_router.key_incarnation_tracked(block_key) {
            return self.cache_read_block(block_key, data).is_ok();
        }
        let Some(before) = backend_router.fill_incarnation(block_key) else {
            // Unstable (mid-write or retired): never publish.
            return false;
        };
        self.cache_read_block_if(block_key, data, &|| {
            backend_router.fill_incarnation_still(block_key, before)
        })
    }

    /// [`Self::cache_read_block_validated`] against this staging's own wired
    /// router (dehydration worker call sites). Unwired routers
    /// (bare tooling) refuse the publish — a cache entry is never worth an
    /// unvalidated stick.
    pub fn cache_read_block_validated_self(&self, block_key: &str, data: Bytes) -> bool {
        match self.backend_router.get().and_then(|w| w.upgrade()) {
            Some(router) => self.cache_read_block_validated(block_key, data, &router),
            None => false,
        }
    }

    pub fn current_staged_write_bytes(&self) -> u64 {
        self.current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn current_read_cache_bytes(&self) -> u64 {
        self.read_nvme_cache.current_bytes() as u64
    }

    pub fn read_cached_block(&self, block_key: &str) -> Option<Vec<u8>> {
        self.get_cached_read_block(block_key)
    }

    /// Cheap tier residency probe — index membership only, no payload
    /// copy (§5.5 consume-time detector).
    pub fn has_cached_read_block(&self, block_key: &str) -> bool {
        self.read_nvme_cache
            .contains(&Bytes::copy_from_slice(block_key.as_bytes()))
    }

    /// Drop a read-cache entry for a block key. Called when the physical block
    /// behind the key is freed: block keys are offset strings, so the next
    /// allocation of that offset reuses the same key string and must never be
    /// served this incarnation's bytes.
    pub fn remove_cached_read_block(&self, block_key: &str) {
        let _ = self.read_nvme_cache.remove(block_key.as_bytes());
    }

    pub fn get_cached_read_block(&self, block_key: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        Some(guard.guard.mmap[guard.offset..guard.offset + guard.len].to_vec())
    }

    pub fn get_cached_read_block_range(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            return Some(Vec::new());
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        let end = start + read_len;
        Some(guard.guard.mmap[start..end].to_vec())
    }

    pub fn get_cached_read_block_range_zero_copy(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let mut guard = self.read_nvme_cache.get_static(block_key.as_bytes())?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            guard.offset += guard.len;
            guard.len = 0;
            return Some(guard);
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        guard.offset = start;
        guard.len = read_len;
        Some(guard)
    }

    pub fn list_staged_files(&self) -> Vec<String> {
        self.staging_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    /// VAL-7a: live read-cache entry count — the census-free gauge that
    /// replaces [`Self::list_cached_blocks`] in the default `.stats`
    /// payload (a count names no blocks).
    pub fn cached_block_count(&self) -> usize {
        self.read_nvme_cache.entry_count()
    }

    pub fn list_cached_blocks(&self) -> Vec<String> {
        self.read_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn max_write_bytes(&self) -> u64 {
        self.max_write_bytes
    }

    pub fn max_read_bytes(&self) -> u64 {
        self.max_read_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{shard_plan, SHARD_REPLACE_SLACK};

    const MIB: u64 = 1024 * 1024;
    const ENTRY: u64 = 4 * MIB; // default block size class

    /// FIND-VS-B geometry: the storm fixture (128 MiB staging, 4 MiB
    /// blocks) must never be planned into structural same-key-replace
    /// refusal, no matter how wide the box is. Pre-fix,
    /// `default_shards = next_power_of_two(25 cores) = 32` yielded 4 MiB
    /// shards — exactly one entry, zero replace headroom — and every
    /// re-stage spilled durably (the deterministic pooled_seeds 100/200
    /// signature).
    #[test]
    fn shard_plan_replace_headroom_invariant_across_box_widths() {
        let min_shard = 2 * ENTRY + SHARD_REPLACE_SLACK;
        for default_shards in [16usize, 32, 64, 128, 512] {
            for capacity_mib in [10u64, 64, 100, 128, 256, 500, 1024, 4096, 65536] {
                let capacity = capacity_mib * MIB;
                let shards = shard_plan(capacity, default_shards, ENTRY);
                assert!(shards >= 1, "never zero shards");
                assert!(
                    shards.is_power_of_two(),
                    "NvmeCache::new asserts power-of-two shard counts, got {shards}"
                );
                assert!(
                    shards == 1 || capacity / shards as u64 >= min_shard,
                    "structural replace refusal planned back in: {capacity} B / \
                     {shards} shards (default {default_shards}) < {min_shard} B floor"
                );
            }
        }
    }

    /// The exact pre-fix failure geometry: 128 MiB budget on a 32-shard
    /// (≥ 17-core) box must plan shards large enough to hold two live
    /// copies of a 4 MiB-class entry.
    #[test]
    fn shard_plan_find_vs_b_geometry_holds_two_copies() {
        let shards = shard_plan(128 * MIB, 32, ENTRY);
        assert_eq!(
            shards, 8,
            "128 MiB / 8 = 16 MiB shards (two copies + slack)"
        );
        assert!(128 * MIB / shards as u64 >= 2 * ENTRY + SHARD_REPLACE_SLACK);
    }

    /// Production-scale budgets keep the full default shard fan-out — the
    /// plan only narrows when the budget cannot carry it.
    #[test]
    fn shard_plan_large_budgets_keep_default_fanout() {
        assert_eq!(shard_plan(500 * MIB, 32, ENTRY), 32);
        assert_eq!(shard_plan(64 * 1024 * MIB, 512, ENTRY), 512);
    }

    /// The configured budget is authoritative: sub-headroom totals floor at
    /// one shard (degrading to the loud spill path) instead of inflating
    /// the operator's disk budget the way the pre-fix sizing did.
    #[test]
    fn shard_plan_tiny_budgets_floor_at_one_shard_never_inflate() {
        assert_eq!(shard_plan(MIB, 16, ENTRY), 1);
        assert_eq!(shard_plan(0, 16, ENTRY), 1);
        assert_eq!(shard_plan(6 * MIB, 32, ENTRY), 1);
    }

    /// Tiny pools stay one whole-pool shard even when the block-size class
    /// is small enough that the replace-headroom floor alone would permit
    /// splitting: the largest admissible entry must remain the whole pool
    /// (the `tests/staging_budget_tests.rs` shard-full contract stages
    /// 300 KiB entries into a 1 MiB pool under a 64 KiB block class).
    #[test]
    fn shard_plan_tiny_pools_stay_whole_even_for_small_entries() {
        assert_eq!(shard_plan(MIB, 32, 64 * 1024), 1);
        // At/above the tiny-pool bound the headroom floor governs again.
        assert_eq!(shard_plan(10 * MIB, 32, 64 * 1024), 32);
    }
}
