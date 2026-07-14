//! PR M5 (design-metadata-throughput §5.2 D2.b + the survey P1-C rider):
//! negative-entry lookup replies and per-class kernel cache TTL knobs.
//!
//! D2.b contract: a lookup MISS (the ENOENT class — the normal grammar of
//! POSIX probes) is answered with a **cacheable negative entry** (nodeid 0
//! with entry TTL > 0) instead of a bare `Errno(ENOENT)`, so the kernel
//! can cache the negative dentry and absorb repeated-miss round trips
//! (PATH walks, stat retries, rename-dest probes). Honest scope (mandated
//! by the design): unique-name create storms look up each name once —
//! this moves repeated-miss traffic, NOT the mdstorm create row.
//!
//! Correctness pins:
//! - create-after-miss must observe the created file immediately (no
//!   daemon-side stale negative state — a stale negative dentry hiding a
//!   created file is the generic/001/013 corruption class; the kernel-side
//!   dcache conversion is exercised by those fstests at acceptance).
//! - non-ENOENT lookup failures keep their error shape.
//! - `negative_timeout=0` disables negative replies entirely (bare ENOENT,
//!   bit-exact pre-M5 behavior).
//!
//! TTL knobs (survey P1-C, DAOS per-class split / JuiceFS flags): attr /
//! entry / dir-entry / negative TTLs are per-mount knobs, defaulting to
//! the historical 1 s, and NON-DEFAULT VALUES MUST REACH THE REPLIES.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{KernelCacheTtls, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "negent_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_5678_1234,
            uuid: *b"negent-lookup-v3",
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

fn negative_replies() -> u64 {
    METRICS.fuse_lookup_negative_replies.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// D2.b: negative-entry replies
// ---------------------------------------------------------------------------

/// A lookup miss is a cacheable negative entry: Ok(entry) with nodeid 0
/// (attr.ino == 0) and the negative TTL — never Err(ENOENT).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_miss_replies_cacheable_negative_entry() {
    let h = make().await;

    let n0 = negative_replies();
    let entry =
        h.fs.lookup(h.req, 1, OsStr::new("does-not-exist"))
            .await
            .expect("miss must be a NEGATIVE ENTRY reply (nodeid 0), not an errno");
    assert_eq!(
        entry.attr.ino, 0,
        "negative entry encodes as nodeid 0 (uapi: 'Zero nodeid is same as \
         -ENOENT, but with valid timeout')"
    );
    assert_eq!(
        entry.ttl,
        Duration::from_secs(1),
        "negative TTL defaults to 1 s (matches positive TTLs)"
    );
    assert_eq!(entry.generation, 0, "negative entries carry generation 0");
    assert_eq!(
        negative_replies() - n0,
        1,
        "fuse_lookup_negative_replies must count each negative reply"
    );
}

/// create-after-miss: the created file is immediately visible to lookup
/// (a stale negative hiding a created file is a correctness bug).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_after_negative_lookup_sees_the_file() {
    let h = make().await;

    let miss = h.fs.lookup(h.req, 1, OsStr::new("soon")).await.unwrap();
    assert_eq!(miss.attr.ino, 0, "pre-create probe must be negative");

    let created =
        h.fs.create(h.req, 1, OsStr::new("soon"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    assert_ne!(created.attr.ino, 0);

    let hit = h.fs.lookup(h.req, 1, OsStr::new("soon")).await.unwrap();
    assert_eq!(
        hit.attr.ino, created.attr.ino,
        "lookup after create must return the created inode, not a stale negative"
    );
    assert_ne!(hit.attr.ino, 0);
}

/// unlink-after-hit: the next lookup is a negative entry again (the
/// unlinked name is re-cacheable as a miss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_then_lookup_is_negative_again() {
    let h = make().await;

    let created =
        h.fs.create(h.req, 1, OsStr::new("gone"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    // Close the create handle so unlink -> reclaim lifecycle stays clean.
    h.fs.release(h.req, created.attr.ino, created.attr.ino, 0, 0, false)
        .await
        .unwrap();
    h.fs.unlink(h.req, 1, OsStr::new("gone")).await.unwrap();

    let entry = h.fs.lookup(h.req, 1, OsStr::new("gone")).await.unwrap();
    assert_eq!(entry.attr.ino, 0, "post-unlink probe must be negative");
}

/// negative_timeout=0 disables negative replies: the miss is a bare
/// Errno(ENOENT) exactly as pre-M5 (kernel caches nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_ttl_zero_disables_negative_replies() {
    let mut h = make().await;
    h.fs.kernel_ttls.negative = Duration::ZERO;

    let n0 = negative_replies();
    let err =
        h.fs.lookup(h.req, 1, OsStr::new("no-cache"))
            .await
            .expect_err("negative TTL 0 must reply bare ENOENT");
    // fuse3's Errno -> c_int is the (negative) wire form.
    assert_eq!(libc::c_int::from(err), -libc::ENOENT);
    assert_eq!(
        negative_replies() - n0,
        0,
        "disabled negative caching must not count negative replies"
    );
}

/// Non-ENOENT failures keep their error shape (never converted into a
/// negative entry): an oversize name is ENAMETOOLONG.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_enoent_lookup_errors_stay_errors() {
    let h = make().await;
    let long = "x".repeat(256);
    let err =
        h.fs.lookup(h.req, 1, OsStr::new(&long))
            .await
            .expect_err("oversize name must fail");
    assert_eq!(
        libc::c_int::from(err),
        -libc::ENAMETOOLONG,
        "only the ENOENT class may become a negative entry"
    );
}

// ---------------------------------------------------------------------------
// Survey P1-C: per-class kernel TTL knobs reach the replies
// ---------------------------------------------------------------------------

/// Non-default TTLs must reach every reply surface: lookup (file vs dir),
/// create, mkdir, getattr, setattr and the negative reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttl_knobs_reach_reply_surfaces() {
    let mut h = make().await;
    h.fs.kernel_ttls = KernelCacheTtls {
        attr: Duration::from_secs(3),
        entry: Duration::from_secs(5),
        dir_entry: Duration::from_secs(7),
        negative: Duration::from_millis(900),
    };

    // create → entry TTL (file class).
    let created =
        h.fs.create(h.req, 1, OsStr::new("f"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    assert_eq!(
        created.ttl,
        Duration::from_secs(5),
        "create reply = entry TTL"
    );

    // mkdir → dir-entry TTL.
    let mk =
        h.fs.mkdir(h.req, 1, OsStr::new("d"), 0o755, 0)
            .await
            .unwrap();
    assert_eq!(
        mk.ttl,
        Duration::from_secs(7),
        "mkdir reply = dir-entry TTL"
    );

    // lookup of a file → entry TTL; of a dir → dir-entry TTL.
    let lf = h.fs.lookup(h.req, 1, OsStr::new("f")).await.unwrap();
    assert_eq!(lf.ttl, Duration::from_secs(5), "file lookup = entry TTL");
    let ld = h.fs.lookup(h.req, 1, OsStr::new("d")).await.unwrap();
    assert_eq!(ld.ttl, Duration::from_secs(7), "dir lookup = dir-entry TTL");

    // getattr / setattr → attr TTL.
    let ga =
        h.fs.getattr(h.req, created.attr.ino, None, 0)
            .await
            .unwrap();
    assert_eq!(ga.ttl, Duration::from_secs(3), "getattr reply = attr TTL");
    let sa =
        h.fs.setattr(
            h.req,
            created.attr.ino,
            None,
            fuse3::SetAttr {
                mode: Some(0o600),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(sa.ttl, Duration::from_secs(3), "setattr reply = attr TTL");

    // negative reply → negative TTL.
    let neg = h.fs.lookup(h.req, 1, OsStr::new("nope")).await.unwrap();
    assert_eq!(neg.attr.ino, 0);
    assert_eq!(
        neg.ttl,
        Duration::from_millis(900),
        "negative reply = negative TTL"
    );
}

/// Mount-option parsing: libfuse-style float seconds, unknown keys and
/// garbage ignored, non-TTL options untouched.
#[test]
fn ttl_mount_option_parsing() {
    let ttls = KernelCacheTtls::default().with_mount_options(
        "rw,attr_timeout=2.5,entry_timeout=3,dir_entry_timeout=10,\
         negative_timeout=0.5,max_read=131072,unknown_thing=9",
    );
    assert_eq!(ttls.attr, Duration::from_secs_f64(2.5));
    assert_eq!(ttls.entry, Duration::from_secs(3));
    assert_eq!(ttls.dir_entry, Duration::from_secs(10));
    assert_eq!(ttls.negative, Duration::from_secs_f64(0.5));

    // Garbage / negative / non-finite values keep the default.
    let bad = KernelCacheTtls::default()
        .with_mount_options("attr_timeout=abc,entry_timeout=-1,negative_timeout=inf");
    assert_eq!(bad, KernelCacheTtls::default());
}

/// The env knobs seed the per-mount defaults (launch-time convention:
/// read once at filesystem construction, never per-op).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttl_env_knobs_seed_construction() {
    std::env::set_var("SQUEEZEFS_FUSE_ATTR_TTL_MS", "250");
    std::env::set_var("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", "0");
    let ttls = KernelCacheTtls::from_env();
    std::env::remove_var("SQUEEZEFS_FUSE_ATTR_TTL_MS");
    std::env::remove_var("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS");

    assert_eq!(ttls.attr, Duration::from_millis(250));
    assert_eq!(ttls.negative, Duration::ZERO);
    assert_eq!(
        ttls.entry,
        Duration::from_secs(1),
        "unset keys keep defaults"
    );
    assert_eq!(ttls.dir_entry, Duration::from_secs(1));
}

/// The kernel-option filter keeps stripping TTL keys from the mount(2)
/// string (they are daemon-level), including the new dir_entry_timeout.
#[test]
fn ttl_options_stay_out_of_kernel_mount_string() {
    let parsed = squeezefs::fuse_client::parse_custom_options(
        "entry_timeout=5,attr_timeout=5,negative_timeout=5,dir_entry_timeout=5,max_read=65536",
    );
    let s = parsed.to_string_lossy();
    assert!(
        !s.contains("timeout"),
        "TTL options must never reach the kernel mount string: {s}"
    );
    assert!(
        s.contains("max_read=65536"),
        "real kernel options pass through"
    );
}
