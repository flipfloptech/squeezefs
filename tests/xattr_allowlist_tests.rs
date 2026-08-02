//! VAL-2 (pre-RC engineering spec §3) — the reserved-xattr screen must be
//! a positive ALLOWLIST, mirrored inside the volume backend.
//!
//! The verified exposure: `reserved_xattr_name` was a DENYLIST covering
//! exactly `job:` and `user.squeezefs.`, so four internal records were
//! readable AND writable straight through the FUSE xattr surface —
//!
//! | record | what a write/removal buys the caller |
//! |---|---|
//! | `system.symlink` | every symlink's target (the VFS deliberately performs NO permission check for `system.*` — `xattr_permission()` returns 0 early, so the daemon is the only enforcement point) |
//! | `layout` | the per-inode block map + wrapped data-key material; a write of another file's decodable value redirects reads to that file's blocks |
//! | `writer_claim` | the D0 single-writer guard — removing it presents the volume set as unclaimed to another host's takeover ladder |
//! | `client:{id}` | the live-client registrations guarding `config set-cache-paths` |
//!
//! The law now: permit `user.*` (minus `user.squeezefs.`), `security.*`
//! and `trusted.*`; refuse everything else — `EPERM` on set/remove
//! (counted in `fuse_reserved_xattr_refusals`), `ENODATA` on get
//! (a name filtered out of `listxattr` must read as ABSENT, not as a
//! name whose existence the surface confirms), and filtered from
//! `listxattr`. Enforced at BOTH the FUSE boundary and the volume
//! backend's `Metadata` entry points; the daemon's own internal record
//! writers ride the `*_internal` path.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::{xattr_name_allowed, KvMetaBackend};
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::reply::ReplyXAttr;
use fuse3::raw::Request;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

const ROOT: u64 = 1;

/// Every internal record the spec enumerated, plus the two the pre-RC
/// denylist already covered.
const INTERNAL_RECORDS: &[&str] = &[
    "system.symlink",
    "layout",
    "writer_claim",
    "client:11111111-2222-3333-4444-555555555555",
    "job:deadbeef",
    "job:deadbeef:shard:0",
    "user.squeezefs.format_config",
    "user.squeezefs.future_record",
];

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

async fn open_v3_meta(path: &std::path::Path, len: u64) -> Arc<KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(b"{\"name\":\"val2\"}".to_vec()),
        },
    )
    .await
    .expect("format v3 meta volume");
    KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    kv: Arc<KvMetaBackend>,
    _meta_file: NamedTempFile,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn fixture(tag: &str) -> Fx {
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(tag).await.expect("allocator"));
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let meta_file = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(meta_file.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        kv.clone()
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    Fx {
        fs: Arc::new(fs),
        kv,
        _meta_file: meta_file,
        _backing: backing,
        _staging: staging,
    }
}

fn refusals() -> u64 {
    METRICS.fuse_reserved_xattr_refusals.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The predicate itself
// ---------------------------------------------------------------------------

#[test]
fn the_screen_is_a_positive_allowlist() {
    for allowed in [
        "user.foo",
        "user.squeezefs",     // NOT the reserved `user.squeezefs.` prefix
        "user.squeezefsjunk", // ditto — prefix boundary
        "security.capability",
        "security.selinux",
        "trusted.overlay.opaque",
    ] {
        assert!(xattr_name_allowed(allowed), "{allowed} must cross");
    }
    for refused in [
        // Every internal record.
        "system.symlink",
        "layout",
        "writer_claim",
        "client:abcd",
        "job:abcd",
        "user.squeezefs.format_config",
        "user.squeezefs.",
        // ... and everything else outside the three permitted namespaces:
        // an allowlist refuses names nobody has invented yet.
        "",
        "system.foo",
        "os2.attr",
        "USER.foo",
        "useruser.foo",
        "inline_data:1",
        "block_map:1",
        "active_block:1",
        "mapping:1",
        "metadata:1",
    ] {
        assert!(!xattr_name_allowed(refused), "{refused:?} must be refused");
    }
}

// ---------------------------------------------------------------------------
// Through the FUSE surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_records_are_eperm_on_set_and_remove_through_fuse() {
    let fx = fixture("val2-fuse-write").await;
    for name in INTERNAL_RECORDS {
        let before = refusals();
        let e = fx
            .fs
            .setxattr(req(), ROOT, OsStr::new(name), b"forged", 0, 0)
            .await
            .expect_err("internal-record setxattr must refuse");
        assert_eq!(e, libc::EPERM.into(), "setxattr({name}) must be EPERM");

        let e = fx
            .fs
            .removexattr(req(), ROOT, OsStr::new(name))
            .await
            .expect_err("internal-record removexattr must refuse");
        assert_eq!(e, libc::EPERM.into(), "removexattr({name}) must be EPERM");
        assert!(
            refusals() >= before + 2,
            "every refusal is counted in fuse_reserved_xattr_refusals ({name})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_records_read_as_absent_through_fuse() {
    let fx = fixture("val2-fuse-read").await;

    // Plant every record on disk the way the daemon itself writes them
    // (the internal path — the screen must not block the daemon).
    for name in INTERNAL_RECORDS {
        fx.kv
            .setxattr_internal(ROOT, name, b"internal-bytes")
            .await
            .expect("the daemon's own internal writer is never screened");
    }
    fx.fs
        .setxattr(req(), ROOT, OsStr::new("user.visible"), b"v", 0, 0)
        .await
        .expect("plain user xattrs still work");

    for name in INTERNAL_RECORDS {
        let before = refusals();
        let e = fx
            .fs
            .getxattr(req(), ROOT, OsStr::new(name), 4096)
            .await
            .expect_err("internal-record getxattr must refuse");
        assert_eq!(
            e,
            libc::ENODATA.into(),
            "getxattr({name}) must read as absent (ENODATA) — the name is \
             filtered out of listxattr, so it must not exist to the surface"
        );
        assert!(refusals() > before, "the refusal is counted ({name})");
    }

    // ... and none of them is listed.
    let reply = fx
        .fs
        .listxattr(req(), ROOT, 65536)
        .await
        .expect("listxattr");
    let names = match reply {
        ReplyXAttr::Data(d) => String::from_utf8_lossy(&d).into_owned(),
        other => panic!("expected Data, got {other:?}"),
    };
    assert!(names.contains("user.visible"), "plain names stay: {names}");
    for name in INTERNAL_RECORDS {
        assert!(
            !names.split('\0').any(|n| n == *name),
            "{name} must be invisible through listxattr: {names}"
        );
    }

    // The records survive the refusals — nothing was destroyed, and the
    // daemon's own read path still sees them.
    for name in INTERNAL_RECORDS {
        let v = fx
            .kv
            .getxattr(ROOT, name)
            .await
            .expect("internal read")
            .unwrap_or_else(|| panic!("{name} must still be on disk"));
        assert_eq!(v, b"internal-bytes");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permitted_namespaces_still_work_through_fuse() {
    let fx = fixture("val2-permitted").await;
    for (name, val) in [
        ("user.plain", &b"a"[..]),
        ("security.capability", &b"\x01\x00\x00\x02"[..]),
        ("trusted.thing", &b"t"[..]),
    ] {
        fx.fs
            .setxattr(req(), ROOT, OsStr::new(name), val, 0, 0)
            .await
            .unwrap_or_else(|e| panic!("{name} must be permitted, got {e:?}"));
        let reply = fx
            .fs
            .getxattr(req(), ROOT, OsStr::new(name), 4096)
            .await
            .unwrap_or_else(|e| panic!("{name} must read back, got {e:?}"));
        match reply {
            ReplyXAttr::Data(d) => assert_eq!(&d[..], val),
            other => panic!("expected Data, got {other:?}"),
        }
        fx.fs
            .removexattr(req(), ROOT, OsStr::new(name))
            .await
            .unwrap_or_else(|e| panic!("{name} must be removable, got {e:?}"));
    }

    // POSIX ACL names keep their own (older, load-bearing) answer:
    // ENOTSUP, not the allowlist's EPERM — fstests generic/099/319.
    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        let e = fx
            .fs
            .setxattr(req(), ROOT, OsStr::new(name), b"x", 0, 0)
            .await
            .expect_err("ACL xattrs refuse");
        assert_eq!(e, libc::EOPNOTSUPP.into(), "{name} must stay ENOTSUP");
    }
}

/// The symlink surface is the load-bearing consumer of `system.symlink`:
/// the record must keep working for the daemon while being invisible and
/// immutable through FUSE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_targets_still_resolve_while_the_record_is_screened() {
    let fx = fixture("val2-symlink").await;
    let entry = fx
        .fs
        .symlink(req(), ROOT, OsStr::new("link"), OsStr::new("/target/path"))
        .await
        .expect("symlink creation");
    let ino = entry.attr.ino;

    let target = fx.fs.readlink(req(), ino).await.expect("readlink");
    assert_eq!(target.data.as_ref(), b"/target/path");

    // The record itself is unreachable through the xattr surface.
    let e = fx
        .fs
        .setxattr(
            req(),
            ino,
            OsStr::new("system.symlink"),
            b"/etc/shadow",
            0,
            0,
        )
        .await
        .expect_err("retargeting a symlink through setxattr must refuse");
    assert_eq!(e, libc::EPERM.into());
    let e = fx
        .fs
        .removexattr(req(), ino, OsStr::new("system.symlink"))
        .await
        .expect_err("destroying a symlink target through removexattr must refuse");
    assert_eq!(e, libc::EPERM.into());

    let target = fx.fs.readlink(req(), ino).await.expect("readlink after");
    assert_eq!(
        target.data.as_ref(),
        b"/target/path",
        "the refusals must not have touched the record"
    );
}

// ---------------------------------------------------------------------------
// Directly against the backend (the mirror — the FUSE layer is not the
// only enforcement point)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backends_metadata_entry_points_mirror_the_screen() {
    let fx = fixture("val2-backend").await;
    let be: &dyn Metadata = &*fx.kv;

    for name in INTERNAL_RECORDS {
        // Plant it through the internal path first, so the refusals
        // below cannot be confused with "the record does not exist".
        fx.kv
            .setxattr_internal(ROOT, name, b"internal-bytes")
            .await
            .expect("internal writer");

        let e = be
            .setxattr(ROOT, name, b"forged")
            .await
            .expect_err("a generic Metadata setxattr must refuse internal records");
        assert_eq!(e.to_errno(), libc::EPERM, "setxattr({name})");

        let e = be
            .removexattr(ROOT, name)
            .await
            .expect_err("a generic Metadata removexattr must refuse internal records");
        assert_eq!(e.to_errno(), libc::EPERM, "removexattr({name})");

        assert!(
            be.getxattr(ROOT, name).await.expect("get").is_none(),
            "a generic Metadata getxattr must read {name} as absent"
        );

        // ... and the record is still there for the daemon.
        assert_eq!(
            fx.kv.getxattr(ROOT, name).await.expect("internal read"),
            Some(b"internal-bytes".to_vec()),
            "{name} must survive the refusals"
        );
    }

    // listxattr through the trait filters; the internal listing does not.
    be.setxattr(ROOT, "user.seen", b"1")
        .await
        .expect("plain set");
    let listed = be.listxattr(ROOT).await.expect("trait listxattr");
    assert!(listed.iter().any(|n| n == "user.seen"));
    for name in INTERNAL_RECORDS {
        assert!(
            !listed.iter().any(|n| n == name),
            "{name} must be filtered from the trait listxattr: {listed:?}"
        );
    }
    let internal = fx.kv.listxattr(ROOT).await.expect("internal listxattr");
    for name in INTERNAL_RECORDS {
        assert!(
            internal.iter().any(|n| n == name),
            "{name} must stay visible to the daemon's own listing"
        );
    }
}
