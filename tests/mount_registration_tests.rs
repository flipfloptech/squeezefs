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

// ===========================================================================
// PR M1 (design-metadata-throughput §5.0): the `writer_claim` record joins
// the `client:{id}` registrations in the format-preflight live sweep —
// the filter extends from `client:*` to `client:* ∪ writer_claim`.
// ===========================================================================

use squeezefs::meta_backend::kv::backend::{WriterClaim, WRITER_CLAIM_XATTR};

/// Plant a `writer_claim` value the way a crashed writer leaves one: set
/// through a live backend, then shut down — but re-add after the shutdown
/// path so the claim survives (a clean shutdown deletes the mount's OWN
/// claim, not a value written afterward through a fresh handle).
async fn set_claim(meta: &NamedTempFile, ts: u64) {
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let claim = WriterClaim {
        id: "preflight-claimant".into(),
        ts,
        pid: 4_100_000,
        boot: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    };
    be.setxattr(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .unwrap();
    be.sync_device().await.unwrap();
    // Crash-shaped exit: no clean shutdown, so the forged claim survives.
    drop(be);
}

/// A fresh `writer_claim` marks a live mount: format is refused even with
/// `--force` — exactly the `client:*` semantics, one staleness law.
#[tokio::test]
async fn test_fresh_writer_claim_blocks_format() {
    let meta = formatted_volume().await;
    set_claim(&meta, now_secs()).await;

    let blocked = format_v3(meta.path(), VOL_LEN, &opts(true)).await;
    assert!(
        blocked.is_err(),
        "a fresh writer_claim (live mount) must block format even with --force"
    );
}

/// A stale `writer_claim` (crashed writer) never blocks format.
#[tokio::test]
async fn test_stale_writer_claim_does_not_block_format() {
    let meta = formatted_volume().await;
    set_claim(&meta, now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 90)).await;

    let allowed = format_v3(meta.path(), VOL_LEN, &opts(true)).await;
    assert!(
        allowed.is_ok(),
        "a stale writer_claim must not block format: {allowed:?}"
    );
}

/// Claim and client registrations coexist on ino 1 (independent records,
/// one staleness law): a stale claim beside a fresh registration still
/// refuses format (the registration is live), and a fresh claim beside a
/// stale registration still refuses (the claim is live).
#[tokio::test]
async fn test_claim_and_registrations_coexist_in_preflight() {
    let now = now_secs();
    let stale_ts = now.saturating_sub(CLIENT_STALE_TTL_SECS + 90);

    // Stale claim + fresh registration => refused (registration live).
    // (Registration first: set_claim crash-exits and leaves its claim, and
    // a later guarded open would refuse the stale-foreign residue.)
    let meta = formatted_volume().await;
    set_registration(&meta, "live", Some(&reg_value(now))).await;
    set_claim(&meta, stale_ts).await;
    assert!(
        format_v3(meta.path(), VOL_LEN, &opts(true)).await.is_err(),
        "a fresh client registration must refuse format regardless of claim staleness"
    );

    // Fresh claim + stale registration => refused (claim live).
    let meta2 = formatted_volume().await;
    set_registration(&meta2, "dead", Some(&reg_value(stale_ts))).await;
    set_claim(&meta2, now).await;
    assert!(
        format_v3(meta2.path(), VOL_LEN, &opts(true)).await.is_err(),
        "a fresh writer_claim must refuse format regardless of registration staleness"
    );

    // Both stale => format proceeds.
    let meta3 = formatted_volume().await;
    set_registration(&meta3, "dead", Some(&reg_value(stale_ts))).await;
    set_claim(&meta3, stale_ts).await;
    assert!(
        format_v3(meta3.path(), VOL_LEN, &opts(true)).await.is_ok(),
        "stale claim + stale registration must not block format"
    );
}
