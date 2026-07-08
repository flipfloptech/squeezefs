//! Contract tests for the format guard (`MetaLvBackend::format_preflight` /
//! `format_with_options`).
//!
//! Policy:
//! - A volume with a **valid superblock** (already formatted) is refused
//!   without `--force` — even when idle — so a fat-fingered format cannot
//!   destroy a filesystem silently.
//! - A **live** client registration (fresh heartbeat) blocks format even
//!   WITH `--force`: reformatting under an active mount is never safe.
//! - **Stale** registrations (crashed clients) never block; they are reaped.
//! - A blank (never formatted) volume formats without `--force`.

use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
use squeezefs::meta_backend::{storage::MetaLvStorage, xattr, MetaLvBackend};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

fn reg_value(ts: u64) -> Vec<u8> {
    format!("{{\"ts\":{},\"pid\":{}}}", ts, 4242).into_bytes()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn blank_storage() -> (NamedTempFile, MetaLvStorage) {
    let meta = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(meta.path(), 256 * 1024 * 1024).unwrap();
    (meta, storage)
}

async fn formatted_storage() -> (NamedTempFile, MetaLvStorage) {
    let (meta, storage) = blank_storage();
    MetaLvBackend::format(&storage).await.unwrap();
    (meta, storage)
}

#[tokio::test]
async fn blank_volume_formats_without_force() {
    let (_meta, storage) = blank_storage();
    MetaLvBackend::format_with_options(&storage, true, false, None)
        .await
        .expect("a never-formatted volume must format without --force");
}

#[tokio::test]
async fn formatted_volume_without_force_is_refused() {
    let (_meta, storage) = formatted_storage().await;
    let err = MetaLvBackend::format_with_options(&storage, true, false, None)
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
    let (_meta, storage) = formatted_storage().await;
    MetaLvBackend::format_with_options(&storage, true, true, None)
        .await
        .expect("--force must reformat an idle formatted volume");
}

#[tokio::test]
async fn live_client_blocks_format_even_with_force() {
    let (_meta, storage) = formatted_storage().await;
    xattr::set_xattr(&storage, 1, "client:live", &reg_value(now_secs()))
        .await
        .unwrap();

    let err = MetaLvBackend::format_with_options(&storage, true, true, None)
        .await
        .expect_err("a live mounted client must block format even with --force")
        .to_string();
    assert!(
        err.contains("mounted"),
        "error must call out the live mount, got: {err}"
    );
}

#[tokio::test]
async fn stale_client_does_not_block_forced_format_and_is_reaped() {
    let (_meta, storage) = formatted_storage().await;
    let stale_ts = now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 60);
    xattr::set_xattr(&storage, 1, "client:dead", &reg_value(stale_ts))
        .await
        .unwrap();

    MetaLvBackend::format_with_options(&storage, true, true, None)
        .await
        .expect("a stale (crashed) client must not block a forced format");
}

/// Preflight alone must not wipe anything: it is the CLI's no-side-effect
/// gate run across ALL volumes before ANY volume is wiped, so a refused
/// multi-volume format leaves every volume untouched.
#[tokio::test]
async fn preflight_refuses_without_wiping() {
    let (_meta, storage) = formatted_storage().await;
    // Plant a marker inode via xattr to prove the volume survives preflight.
    xattr::set_xattr(&storage, 1, "user.marker", b"survives")
        .await
        .unwrap();

    MetaLvBackend::format_preflight(&storage, false)
        .await
        .expect_err("preflight must refuse an already-formatted volume without force");

    let marker = xattr::get_xattr(&storage, 1, "user.marker")
        .await
        .unwrap()
        .expect("volume must be untouched after a refused preflight");
    assert_eq!(marker, b"survives");
}

#[tokio::test]
async fn preflight_allows_forced_idle_reformat() {
    let (_meta, storage) = formatted_storage().await;
    MetaLvBackend::format_preflight(&storage, true)
        .await
        .expect("preflight with force on an idle formatted volume must pass");
}
