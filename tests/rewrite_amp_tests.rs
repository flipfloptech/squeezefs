//! Idea 17 — the `rewrite_amp` SLO attribution counters
//! (`docs/design-rewrite-program.md` §2; the acceptance instrument the
//! rewrite program builds FIRST).
//!
//! The SLO itself is a per-row device-byte ratio measured by
//! `tests/write_amp_rig.sh` (diskstats ÷ instrument user bytes, the
//! standing amplification instrument). These contracts pin the
//! DAEMON-side attribution that makes any measured row honest without
//! diskstats access:
//!
//! * `rewrite_user_bytes` — user WRITE bytes that landed on
//!   already-mapped striped ranges (the SLO denominator's attribution).
//! * `rewrite_device_write_bytes` — device write bytes submitted for
//!   rewrite-class blocks (displacing CoW uploads, in-place rewrites,
//!   shadow-epoch records).
//! * `rewrite_blocks` — blocks displaced-or-replaced.
//!
//! Contracts:
//! 1. **Fresh ingest attributes nothing**: a fresh striped write leaves
//!    every `rewrite_*` counter untouched (a fresh row's rewrite deltas
//!    must be 0 or the attribution lies).
//! 2. **A full CoW rewrite attributes exactly**: blocks, user bytes and
//!    device bytes account for the whole overwrite (passthrough volume:
//!    device bytes ≡ user bytes ≡ blocks × block_size).
//! 3. **The in-place vehicle counts the same class**: with
//!    `SQUEEZEFS_INPLACE_OVERWRITE` on, an eligible full-block rewrite
//!    still attributes (the SLO is vehicle-blind — CoW, in-place and
//!    shadow rows reconcile against the same family).
//!
//! RED against `a0f38a4`: the `rewrite_*` counters do not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the default-OFF in-place posture on scope exit (knob hygiene).
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_inplace_overwrite(false);
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
            .await
            .expect("open v3 meta volume"),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs: Arc::new(fs),
        req,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

/// Fresh striped fixture of `blocks` full blocks, drained + verified.
async fn striped_fixture(h: &H, name: &str, blocks: u64, seed: u8) -> u64 {
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    write_at(h, ino, 0, &pattern((blocks * FBS) as usize, seed)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "fixture pipeline must drain"
    );
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "fixture premise: every block mapped"
    );
    ino
}

struct RwSnap {
    blocks: u64,
    user: u64,
    dev: u64,
}

fn rw_snap() -> RwSnap {
    RwSnap {
        blocks: METRICS.rewrite_blocks.load(Ordering::Relaxed),
        user: METRICS.rewrite_user_bytes.load(Ordering::Relaxed),
        dev: METRICS.rewrite_device_write_bytes.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — fresh ingest attributes nothing to the rewrite family.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_ingest_attributes_zero_rewrite_bytes() {
    let _g = serial().await;
    let h = make_harness("rwamp_fresh_zero").await;
    let s0 = rw_snap();
    let _ino = striped_fixture(&h, "f1", 4, 5).await;
    let s1 = rw_snap();
    assert_eq!(
        (s1.blocks - s0.blocks, s1.user - s0.user, s1.dev - s0.dev),
        (0, 0, 0),
        "a fresh striped write must attribute NOTHING to the rewrite \
         family — a fresh row whose rewrite deltas move is an attribution \
         lie (design-rewrite-program §2.1)"
    );
}

// ---------------------------------------------------------------------------
// Contract 2 — a full CoW rewrite attributes exactly.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cow_rewrite_attributes_blocks_user_and_device_bytes_exactly() {
    let _g = serial().await;
    let h = make_harness("rwamp_cow_exact").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 9).await;

    let s0 = rw_snap();
    write_at(&h, ino, 0, &pattern((blocks * FBS) as usize, 42)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "rewrite pipeline must drain"
    );
    let s1 = rw_snap();

    assert_eq!(
        s1.blocks - s0.blocks,
        blocks,
        "every displaced block is one rewrite-class block"
    );
    assert_eq!(
        s1.user - s0.user,
        blocks * FBS,
        "user overwrite bytes attribute the whole rewrite"
    );
    assert_eq!(
        s1.dev - s0.dev,
        blocks * FBS,
        "passthrough volume: rewrite-class device write bytes ≡ user bytes \
         (the daemon-side rewrite_amp numerator attribution)"
    );
}

// ---------------------------------------------------------------------------
// Contract 3 — the in-place vehicle attributes the same class.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inplace_rewrite_attributes_the_same_family() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_inplace_overwrite(true);
    let h = make_harness("rwamp_inplace").await;
    let blocks = 2u64;
    let ino = striped_fixture(&h, "f1", blocks, 13).await;

    let s0 = rw_snap();
    let ip0 = METRICS
        .write_through_inplace_overwrites
        .load(Ordering::Relaxed);
    write_at(&h, ino, 0, &pattern((blocks * FBS) as usize, 77)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
    let s1 = rw_snap();

    assert_eq!(
        METRICS
            .write_through_inplace_overwrites
            .load(Ordering::Relaxed)
            - ip0,
        blocks,
        "premise: the in-place arm engaged"
    );
    assert_eq!(
        (s1.blocks - s0.blocks, s1.user - s0.user, s1.dev - s0.dev),
        (blocks, blocks * FBS, blocks * FBS),
        "the SLO attribution is vehicle-blind: an in-place rewrite \
         attributes exactly like the CoW arm"
    );
}
