//! Task: a client that dies ungracefully (kill -9 / crash) must not wedge the
//! volume. Mount registrations (`client:{id}`) are heartbeat-timestamped; a stale
//! one (expired heartbeat) must NOT block `format` and should be reaped, while a
//! fresh one still protects a live mount.

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

async fn formatted_storage() -> (NamedTempFile, MetaLvStorage) {
    let meta = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(meta.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    (meta, storage)
}

/// A fresh (recently heartbeated) registration blocks format; a stale one does
/// not and is reaped.
#[tokio::test]
async fn test_stale_registration_does_not_block_format() {
    let (_meta, storage) = formatted_storage().await;
    let now = now_secs();

    // Fresh registration -> format must be refused.
    xattr::set_xattr(&storage, 1, "client:live", &reg_value(now))
        .await
        .unwrap();
    let blocked = MetaLvBackend::format_with_options(&storage, true, false, None).await;
    assert!(
        blocked.is_err(),
        "a live (freshly-heartbeated) client must block format"
    );

    // Swap in a stale registration (heartbeat long expired == crashed client).
    xattr::remove_xattr(&storage, 1, "client:live")
        .await
        .unwrap();
    let stale_ts = now.saturating_sub(CLIENT_STALE_TTL_SECS + 60);
    xattr::set_xattr(&storage, 1, "client:dead", &reg_value(stale_ts))
        .await
        .unwrap();

    let allowed = MetaLvBackend::format_with_options(&storage, true, false, None).await;
    assert!(
        allowed.is_ok(),
        "a stale (crashed) client registration must not block format: {allowed:?}"
    );
}

/// A legacy/unparseable registration value (no timestamp) is treated as stale so
/// it can never permanently wedge the volume.
#[tokio::test]
async fn test_legacy_registration_value_treated_as_stale() {
    let (_meta, storage) = formatted_storage().await;

    xattr::set_xattr(&storage, 1, "client:legacy", b"mounted")
        .await
        .unwrap();

    let allowed = MetaLvBackend::format_with_options(&storage, true, false, None).await;
    assert!(
        allowed.is_ok(),
        "legacy timestamp-less registration must not block format: {allowed:?}"
    );
}
