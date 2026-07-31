//! Default in-place full-block overwrite — the write-wall iteration-1
//! rewrite fix (`.benchmarks/2026-07-31-write-wall.md` §iteration-1).
//!
//! Field conviction (4-node cluster, f629b46, settled brackets): rewrite
//! walls at 7.5–7.8 GB/s while fresh does 12.2 (the saturated-target
//! ceiling). The phase table names the terms: `publish` 21–29 ms/block
//! (vs 0.9 fresh) and `displaced_free` spills — every CoW rewrite block
//! displaces its predecessor into a reclaim stream whose TARGET-side
//! deallocate service is ~2,700 commands/s AT ANY CLIENT WIDTH (the
//! width-128 experiment moved nothing), while displacement arrives at
//! ~1,900/s and would need 3,750/s at the 15 GB/s bar. Conservation
//! makes CoW-rewrite structurally dealloc-bound on this fabric.
//!
//! The machinery: a full-block overwrite of a sole-owned, undecorated,
//! passthrough, whole-block-mapped striped block on an Active volume
//! can land **IN PLACE** — the W1 sole-owner patch law's whole-block
//! face, and exactly the machinery contract 9 shipped for the brim
//! (`try_inplace_rewrite`), generalized to an OPT-IN default-eligible
//! path. No allocation, no displacement, no discard, same-key merge
//! (no displaced purge). Crash/concurrency class UNCHANGED from the W1
//! precedent: the §5.1 incarnation fence (`begin_patch_sole_owner`)
//! covers racing validated fills; only app-written sectors are ever
//! rewritten (here: every sector of the block, written by THIS write);
//! clone-shared / transformed / decorated / non-Active shapes keep the
//! CoW path verbatim.
//!
//! **Default posture (iteration-2 field verdict): OFF —
//! `SQUEEZEFS_INPLACE_OVERWRITE=1` opts in.** The A-B-B-A ×3 field
//! bracket measured the in-place path −20 % on rewrite with engagement
//! EXACT (32,768/32,768 in place, zero displacement): on the field's
//! zram-lz4 targets a slot-replace write costs ≈ 2× a fresh-slot write
//! (rewrite ≈ fresh ÷ 2, exactly), so CoW + deferred discard + the
//! reclaim manners law wins there. Substrates whose in-place rewrite
//! is cheap (real-SSD DSM fleets) opt in and shed the whole
//! displacement/dealloc stream.
//!
//! Contracts:
//! 1. **Engagement**: a full-block rewrite of an eligible striped block
//!    through the REAL fs.write path lands in place — mapping
//!    byte-identical, `write_through_inplace_overwrites` counts it,
//!    ZERO reclaim enqueues, bytes on the device at the mapped offset,
//!    read-back exact.
//! 2. **Clone-shared blocks keep CoW**: after `increment_refcount`, the
//!    rewrite displaces (mapping changes, reclaim enqueued, in-place
//!    counter still) — never scribbles a shared block.
//! 3. **The lever**: `set_inplace_overwrite(false)` (= the unset-env
//!    default) is the CoW-always posture, verbatim;
//!    `SQUEEZEFS_INPLACE_OVERWRITE=1` opts a substrate in.
//! 4. **The brim arm is unchanged**: at genuine StorageFull the brim
//!    rewrite still engages (its own counter) — the two counters split
//!    default-path vs space-pressure engagements.
//!
//! RED against dev f629b46: no default in-place arm exists — every
//! eligible full-block rewrite displaces into the reclaim queue — and
//! `write_through_inplace_overwrites` does not exist.

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

/// Restore the default-OFF posture on scope exit (knob hygiene).
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_inplace_overwrite(false);
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    backing: NamedTempFile,
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
        backing,
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

async fn block_map_of(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    (*h.fs
        .router
        .fetch_metadata(&path)
        .await
        .expect("meta")
        .block_map
        .expect("mapped"))
    .clone()
}

fn inplace_defaults() -> u64 {
    METRICS
        .write_through_inplace_overwrites
        .load(Ordering::Relaxed)
}
fn inplace_brim() -> u64 {
    METRICS
        .write_through_inplace_rewrites
        .load(Ordering::Relaxed)
}
fn reclaim_queued() -> u64 {
    METRICS.block_free_reclaim_queued.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Contract 1 — engagement: eligible full-block rewrites land in place.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eligible_full_block_rewrite_lands_in_place_with_zero_displacement() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_inplace_overwrite(true);
    let h = make_harness("inplace_default_engagement").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 11).await;
    let map_before = block_map_of(&h, ino).await;

    let (ip0, q0, br0) = (inplace_defaults(), reclaim_queued(), inplace_brim());

    // Full rewrite (whole-block writes through the real path).
    write_at(&h, ino, 0, &pattern((blocks * FBS) as usize, 99)).await;
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "rewrite pipeline must drain"
    );

    assert_eq!(
        inplace_defaults() - ip0,
        blocks,
        "every eligible full-block rewrite must land in place \
         (write_through_inplace_overwrites is the engagement counter)"
    );
    assert_eq!(
        inplace_brim() - br0,
        0,
        "the brim counter stays space-pressure-only"
    );
    assert_eq!(
        reclaim_queued() - q0,
        0,
        "an in-place overwrite displaces NOTHING — no reclaim enqueue, \
         no discard, no dealloc coupling (the rewrite-wall law)"
    );

    let map_after = block_map_of(&h, ino).await;
    assert_eq!(
        map_before, map_after,
        "in-place rewrite must not move the mapping"
    );

    // Bytes are ON THE DEVICE at the mapped offsets (passthrough volume).
    use std::os::unix::fs::FileExt;
    let dev = std::fs::File::open(h.backing.path()).expect("open backing");
    let mut buf = vec![0u8; FBS as usize];
    let want = pattern((blocks * FBS) as usize, 99);
    for b in 0..blocks as u32 {
        let key = map_after.get(&b).expect("mapped");
        let (_, off) = h.fs.router.backend_router.parse_block_key(key).unwrap();
        dev.read_exact_at(&mut buf, off).expect("pread device");
        assert_eq!(
            buf,
            want[(b as usize * FBS as usize)..((b as usize + 1) * FBS as usize)],
            "block {b}: new bytes at the UNMOVED mapped offset"
        );
    }

    // Read-back through the FUSE path.
    let got =
        h.fs.read(h.req, ino, 0, 0, (blocks * FBS) as u32, 0)
            .await
            .expect("read")
            .data
            .to_vec();
    assert_eq!(got, want, "read-back exact");
}

// ---------------------------------------------------------------------------
// Contract 2 — clone-shared blocks keep CoW (never scribble shared data).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_shared_blocks_keep_cow_displacement() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_inplace_overwrite(true);
    let h = make_harness("inplace_shared_cow").await;
    let ino = striped_fixture(&h, "f1", 2, 21).await;
    let map_before = block_map_of(&h, ino).await;

    // Pin every block clone-shared (the sole-owner predicate must fail).
    for key in map_before.values() {
        assert!(
            h.fs.router.backend_router.increment_refcount(key),
            "clone pin"
        );
    }

    let (ip0, q0) = (inplace_defaults(), reclaim_queued());
    write_at(&h, ino, 0, &pattern((2 * FBS) as usize, 77)).await;
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );

    assert_eq!(
        inplace_defaults() - ip0,
        0,
        "a clone-shared block must NEVER rewrite in place"
    );
    let map_after = block_map_of(&h, ino).await;
    for (b, old_key) in &map_before {
        assert_ne!(
            map_after.get(b),
            Some(old_key),
            "block {b}: shared block must displace (CoW)"
        );
    }
    assert_eq!(
        reclaim_queued() - q0,
        0,
        "displaced keys were still clone-pinned: refcount decrement, no \
         terminal free (CoW correctness face)"
    );

    // The clone's view (the original bytes) is intact on the device.
    use std::os::unix::fs::FileExt;
    let dev = std::fs::File::open(h.backing.path()).expect("open backing");
    let mut buf = vec![0u8; FBS as usize];
    let orig = pattern((2 * FBS) as usize, 21);
    for (b, old_key) in &map_before {
        let (_, off) = h.fs.router.backend_router.parse_block_key(old_key).unwrap();
        dev.read_exact_at(&mut buf, off).expect("pread device");
        assert_eq!(
            buf,
            orig[(*b as usize * FBS as usize)..((*b as usize + 1) * FBS as usize)],
            "block {b}: the shared original must be untouched"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — the A/B lever restores CoW-always verbatim.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_restores_cow_always() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_inplace_overwrite(false);
    let h = make_harness("inplace_lever_off").await;
    let ino = striped_fixture(&h, "f1", 2, 31).await;
    let map_before = block_map_of(&h, ino).await;

    let (ip0, q0) = (inplace_defaults(), reclaim_queued());
    write_at(&h, ino, 0, &pattern((2 * FBS) as usize, 88)).await;
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );

    assert_eq!(inplace_defaults() - ip0, 0, "lever off ⇒ no in-place arm");
    assert_eq!(
        reclaim_queued() - q0,
        2,
        "lever off ⇒ the pre-campaign CoW displacement, verbatim"
    );
    let map_after = block_map_of(&h, ino).await;
    for (b, old_key) in &map_before {
        assert_ne!(map_after.get(b), Some(old_key), "block {b} must displace");
    }
}
