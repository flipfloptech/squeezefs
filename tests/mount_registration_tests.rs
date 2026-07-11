//! Task: a client that dies ungracefully (kill -9 / crash) must not wedge the
//! volume. Mount registrations (`client:{id}`, heartbeat-timestamped xattrs
//! on the root ino) are read by the format preflight through a read-only
//! probe mount of the v3 xattr tree: a stale one (expired heartbeat) must
//! NOT block `format`, while a fresh one still protects a live mount.
//! (Re-pinned against the v3 backend when v2 support was removed — the
//! heartbeat semantics predate the format and survive it.)

use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
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

async fn formatted_volume() -> NamedTempFile {
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(VOL_LEN).unwrap();
    format_v3(meta.path(), VOL_LEN, &opts(true)).await.unwrap();
    meta
}

/// Set (or remove) a `client:{id}` registration through the live backend,
/// the way a mounted client's heartbeat does, then shut down cleanly.
async fn set_registration(meta: &NamedTempFile, id: &str, value: Option<&[u8]>) {
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let name = format!("client:{id}");
    match value {
        Some(v) => be.setxattr(1, &name, v).await.unwrap(),
        None => be.removexattr(1, &name).await.unwrap(),
    }
    be.shutdown().await.unwrap();
}

/// A fresh (recently heartbeated) registration blocks format; a stale one
/// does not. (Reformatting a formatted volume requires `force`; the
/// heartbeat semantics under test are orthogonal to that flag.)
#[tokio::test]
async fn test_stale_registration_does_not_block_format() {
    let meta = formatted_volume().await;
    let now = now_secs();

    // Fresh registration -> format must be refused even when forced.
    set_registration(&meta, "live", Some(&reg_value(now))).await;
    let blocked = format_v3(meta.path(), VOL_LEN, &opts(true)).await;
    assert!(
        blocked.is_err(),
        "a live (freshly-heartbeated) client must block format"
    );

    // Swap in a stale registration (heartbeat long expired == crashed client).
    set_registration(&meta, "live", None).await;
    let stale_ts = now.saturating_sub(CLIENT_STALE_TTL_SECS + 60);
    set_registration(&meta, "dead", Some(&reg_value(stale_ts))).await;

    let allowed = format_v3(meta.path(), VOL_LEN, &opts(true)).await;
    assert!(
        allowed.is_ok(),
        "a stale (crashed) client registration must not block format: {allowed:?}"
    );
}

/// A legacy/unparseable registration value (no timestamp) is treated as stale so
/// it can never permanently wedge the volume.
#[tokio::test]
async fn test_legacy_registration_value_treated_as_stale() {
    let meta = formatted_volume().await;

    set_registration(&meta, "legacy", Some(b"mounted")).await;

    let allowed = format_v3(meta.path(), VOL_LEN, &opts(true)).await;
    assert!(
        allowed.is_ok(),
        "legacy timestamp-less registration must not block format: {allowed:?}"
    );
}
