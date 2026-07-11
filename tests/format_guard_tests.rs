//! Contract tests for the format guard
//! (`kv::builder::format_preflight` / `kv::builder::format_v3`) —
//! re-pinned against the v3 formatter when v2 support was removed (the
//! contracts predate the format and survive it).
//!
//! Policy:
//! - A volume with a **valid superblock** (already formatted) is refused
//!   without `--force` — even when idle — so a fat-fingered format cannot
//!   destroy a filesystem silently.
//! - A **live** client registration (fresh heartbeat, read from the v3
//!   xattr tree of the root ino) blocks format even WITH `--force`:
//!   reformatting under an active mount is never safe.
//! - **Stale** registrations (crashed clients) never block.
//! - A blank (never formatted) volume formats without `--force`.
//! - Preflight alone has no side effects (it is the CLI's all-volumes
//!   gate run before ANY volume is wiped) — it opens a read-only probe
//!   mount and writes nothing.

use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_preflight, format_v3, FormatV3Options};
use squeezefs::meta_backend::Metadata;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;

fn opts(force: bool) -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force,
        full_wipe: false,
        format_config_xattr: None,
    }
}

fn reg_value(ts: u64) -> Vec<u8> {
    format!("{{\"ts\":{},\"pid\":{}}}", ts, 4242).into_bytes()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn blank_volume() -> NamedTempFile {
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(VOL_LEN).unwrap();
    meta
}

async fn formatted_volume() -> NamedTempFile {
    let meta = blank_volume();
    format_v3(meta.path(), VOL_LEN, &opts(true)).await.unwrap();
    meta
}

/// Write a `client:{id}` registration into the volume's v3 xattr tree the
/// way a mounted client does (setxattr on the root ino), then shut the
/// backend down cleanly.
async fn register_client(meta: &NamedTempFile, id: &str, ts: u64) {
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    be.setxattr(1, &format!("client:{id}"), &reg_value(ts))
        .await
        .unwrap();
    be.shutdown().await.unwrap();
}

#[tokio::test]
async fn blank_volume_formats_without_force() {
    let meta = blank_volume();
    format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect("a never-formatted volume must format without --force");
}

#[tokio::test]
async fn formatted_volume_without_force_is_refused() {
    let meta = formatted_volume().await;
    let err = format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect_err("an already-formatted volume must be refused without --force")
        .to_string();
    assert!(
        err.contains("already formatted") && err.contains("--force"),
        "error must say the volume is formatted and mention --force, got: {err}"
    );
}

#[tokio::test]
async fn formatted_volume_with_force_is_reformatted() {
    let meta = formatted_volume().await;
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat an idle formatted volume");
}

#[tokio::test]
async fn live_client_blocks_format_even_with_force() {
    let meta = formatted_volume().await;
    register_client(&meta, "live", now_secs()).await;

    let err = format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect_err("a live mounted client must block format even with --force")
        .to_string();
    assert!(
        err.contains("mounted"),
        "error must call out the live mount, got: {err}"
    );
}

#[tokio::test]
async fn stale_client_does_not_block_forced_format() {
    let meta = formatted_volume().await;
    let stale_ts = now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 60);
    register_client(&meta, "dead", stale_ts).await;

    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("a stale (crashed) client must not block a forced format");
}

/// Preflight alone must not change anything: it is the CLI's
/// no-side-effect gate run across ALL volumes before ANY volume is wiped,
/// so a refused multi-volume format leaves every volume untouched.
/// Byte-level pin: the probe mount spawns no checkpoint task and writes
/// nothing, so the whole volume image is bit-identical after a refused
/// preflight — a stronger guarantee than the retired v2 gate (which
/// reaped stale registrations in place) ever gave.
#[tokio::test]
async fn preflight_refuses_without_side_effects() {
    let meta = formatted_volume().await;
    // Plant a marker xattr to prove the namespace survives preflight.
    {
        let be = KvMetaBackend::open(meta.path()).await.unwrap();
        be.setxattr(1, "user.marker", b"survives").await.unwrap();
        be.shutdown().await.unwrap();
    }
    let image_before = std::fs::read(meta.path()).unwrap();

    format_preflight(meta.path(), false)
        .await
        .expect_err("preflight must refuse an already-formatted volume without force");

    let image_after = std::fs::read(meta.path()).unwrap();
    assert_eq!(
        image_before, image_after,
        "a refused preflight must leave the volume byte-identical (read-only probe)"
    );

    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let marker = be
        .getxattr(1, "user.marker")
        .await
        .unwrap()
        .expect("volume must be untouched after a refused preflight");
    assert_eq!(marker, b"survives");
    be.shutdown().await.unwrap();
}

#[tokio::test]
async fn preflight_allows_forced_idle_reformat() {
    let meta = formatted_volume().await;
    format_preflight(meta.path(), true)
        .await
        .expect("preflight with force on an idle formatted volume must pass");
}

/// A legacy v2 superblock (crafted bytes — no v2 writer exists) is
/// protected by the same already-formatted guard: refused without
/// `--force`, reformatted to v3 with it.
#[tokio::test]
async fn legacy_v2_volume_guarded_and_reformattable() {
    let meta = blank_volume();
    let mut legacy_sb = Vec::with_capacity(12);
    legacy_sb.extend_from_slice(b"METALV01");
    legacy_sb.extend_from_slice(&2u32.to_le_bytes());
    squeezefs::uring_fs::write_at(meta.path(), 0, bytes::Bytes::from(legacy_sb))
        .await
        .unwrap();

    format_preflight(meta.path(), false)
        .await
        .expect_err("a legacy v2 volume must be refused without --force");
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat a legacy v2 volume to v3");
    assert!(
        squeezefs::meta_backend::open_volume_for_mount(meta.path().to_str().unwrap())
            .await
            .is_ok(),
        "the reformatted volume mounts as v3"
    );
}
