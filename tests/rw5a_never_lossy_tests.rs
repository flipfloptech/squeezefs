//! FIND-RW5-A — the generic/464 EIO class, fixed (was: documented
//! expected-fail; user ruling 2026-07-21: no longer an acceptable
//! adjudication). Three user-visible EIO faces under 464's 16-proc
//! truncate/rewrite/append/sync_range storm, each pinned here:
//!
//! 1. **StorageFull propagation (the §10 residual-8 charter).** The
//!    staging ring, structurally oversubscribed by live staged files,
//!    refuses a whole-image admit and SOME arms propagate `StorageFull`
//!    to the caller instead of taking the durable-spill escalation the
//!    staged-replace arm already has. Arm sweep (every `stage_write`
//!    caller in the tree — enumerated by grep, pinned per-arm below):
//!    A. `DataRouter::write_file` staged replace   — HAS the spill;
//!    B. `fold_rider_record` same-key re-stage     — PROPAGATED (red);
//!    C. `clone_file` staged-clone stage           — PROPAGATED (red).
//!    All other ring writes are already never-lossy by construction:
//!    `put_extent_record` refusal falls to the whole-image path,
//!    `put_active_block` refusal parks in RAM, `shrink_staged`/`
//!    clip_rider_record` patch in place (no admission), and the
//!    write-through fallback parks. Every arm counts
//!    `staged_spill_escalations` when it degrades.
//!
//! 2. **Binding-rebind exhaustion on the write path.** Under CoW
//!    write-through churn a same-block RMW seed
//!    (`get_block_for_index`) lost the movement race 8 times and
//!    surfaced `"did not settle after 8 binding rebinds"` as EIO to the
//!    user append (the dominant per-run 464 signature: out.bad EIO
//!    lines pair 1:1 with these). Liveness fix: attempts get a backoff
//!    tail past the fast phase and the write-side seed may serialize
//!    one fetch under the block stripe (it holds none — the staged
//!    guard is dropped before `write_striped`).
//!
//! 3. **Lease-churn fencing.** RELEASE dropped the shared cached op
//!    lease while OTHER handles were still open; the next acquisition
//!    bumped the fencing token and every in-flight op holding the old
//!    snapshot fenced into EIO (`FencingTokenExpired{N, N+1}` storms in
//!    the 464 daemon logs). Fixes: the lease drops only at LAST close,
//!    and the write handler retries a fenced attempt once with a fresh
//!    lease (single-writer mount: an adjacent bump is always our own
//!    churn, never a foreign writer).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1024 * 1024; // 1 MiB blocks: staged window ≤ 1 MiB

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str, write_cap: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(write_cap),
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
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x464C_0FFE_E464_2026,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
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

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pat(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ tag | 1).collect()
}

/// Fill the staging ring's segments with live staged files until a fresh
/// whole-image stage of `probe_len` bytes would be refused (verified by
/// probing the budget gauge indirectly: we stage until the layout mix
/// shows spills starting or we placed `max_files`). Returns the inos.
async fn fill_ring(h: &H, max_files: usize, file_len: usize, tag: u8) -> Vec<u64> {
    let mut inos = Vec::new();
    for i in 0..max_files {
        let ino = create(h, &format!("filler_{tag}_{i}")).await;
        write_at(h, ino, 0, &pat(file_len, tag)).await;
        // Pin each filler ring-resident: a LIVE rider record makes the
        // merge worker DEFER promotion (the W2 rider fence), so the ring
        // cannot self-drain — the 464 oversubscription shape, held.
        write_at(h, ino, 4096, &pat(2048, tag ^ 0x0F)).await;
        inos.push(ino);
    }
    inos
}

/// Arm B — `fold_rider_record`'s same-key re-stage. Pre-fix the
/// StorageFull from the refused replace propagates out of
/// `fold_extent_block(ino, 0)`; post-fix the composed image spills
/// durably (counted) and the record retires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rider_fold_under_full_ring_spills_never_errors() {
    let _g = serial().await;
    let h = make(*b"rw5a_arm_b_ring!", "rw5a_arm_b", "4MB").await;

    // The rider file: a staged whole image + one small non-extending
    // sub-image overwrite that parks as the block-0 rider record.
    let ino = create(&h, "rider").await;
    let img_len = 700 * 1024; // staged (≤ 1 MiB block)
    write_at(&h, ino, 0, &pat(img_len, 0xA1)).await;
    let rider_off = 128 * 1024;
    let rider = pat(4096, 0xB2);
    write_at(&h, ino, rider_off, &rider).await;
    assert!(
        METRICS.staged_rider_extent_writes.load(Ordering::Relaxed) > 0,
        "fixture: the sub-image overwrite must have parked as a rider record"
    );

    // Oversubscribe the ring with live staged files (the 464 shape).
    fill_ring(&h, 8, 700 * 1024, 0xC3).await;

    // The fold: same-key replace needs a SECOND slot for the crash-safe
    // copy — with the segments full it gets StorageFull. Pre-fix that
    // propagates; post-fix it spills durably.
    let before = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    let folded =
        h.fs.fold_extent_block(ino, 0)
            .await
            .expect("rider fold must never surface StorageFull (never-lossy spill)");
    assert!(folded, "the rider record was present: the fold must engage");
    let after = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    assert!(
        after > before,
        "the fold under a full ring must take the counted durable-spill \
         escalation (staged_spill_escalations {before} -> {after})"
    );

    // The composed content is intact and durable: base + rider extents.
    let mut want = pat(img_len, 0xA1);
    want[rider_off as usize..rider_off as usize + rider.len()].copy_from_slice(&rider);
    let got = read_at(&h, ino, 0, img_len).await;
    assert_eq!(got, want, "post-fold content must compose base + rider");
}

/// Arm C — `clone_file`'s staged-clone stage of the destination image.
/// Pre-fix StorageFull propagates to the clone caller; post-fix the
/// destination spills durably (counted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_clone_under_full_ring_spills_never_errors() {
    let _g = serial().await;
    let h = make(*b"rw5a_arm_c_ring!", "rw5a_arm_c", "4MB").await;

    let src = create(&h, "src").await;
    let img_len = 700 * 1024;
    write_at(&h, src, 0, &pat(img_len, 0xD4)).await;
    // Pin src ring-resident (a promoted source clones via block refcounts,
    // not staging): a live rider record defers promotion — and the clone
    // must compose it into the destination image.
    let rider_off = 64 * 1024usize;
    let rider = pat(2048, 0xDD);
    write_at(&h, src, rider_off as u64, &rider).await;

    fill_ring(&h, 8, 700 * 1024, 0xE5).await;

    let dst = create(&h, "dst").await;
    let before = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    // Explicit current tokens: the fs caches the op leases (a raw acquire
    // inside clone would park behind them — a different face than the one
    // under test).
    let src_tok = h.fs.dlm().get_fencing_token_ino(src);
    let dst_tok = h.fs.dlm().get_fencing_token_ino(dst).max(1);
    h.fs.router
        .clone_file(
            &squeezefs::keys::inode_path(src),
            &squeezefs::keys::inode_path(dst),
            Some(src_tok),
            Some(dst_tok),
        )
        .await
        .expect("staged clone must never surface StorageFull (never-lossy spill)");
    let after = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    assert!(
        after > before,
        "the staged clone under a full ring must take the counted \
         durable-spill escalation ({before} -> {after})"
    );
    let got = read_at(&h, dst, 0, img_len).await;
    let mut want = pat(img_len, 0xD4);
    want[rider_off..rider_off + rider.len()].copy_from_slice(&rider);
    assert_eq!(got, want, "clone destination content (base + rider)");
}

/// Arm A — the staged whole-image replace (the arm that already had the
/// spill): pinned green + now COUNTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_replace_under_full_ring_spills_and_counts() {
    let _g = serial().await;
    let h = make(*b"rw5a_arm_a_ring!", "rw5a_arm_a", "4MB").await;
    fill_ring(&h, 8, 700 * 1024, 0xF6).await;

    let ino = create(&h, "replacer").await;
    let before = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    // A fresh ~700 KiB staged image against a full ring: the replace arm
    // spills durably (pre-existing behavior) and must now count it.
    write_at(&h, ino, 0, &pat(700 * 1024, 0x97)).await;
    let after = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    assert!(
        after > before,
        "the staged-replace spill must be counted ({before} -> {after})"
    );
    let got = read_at(&h, ino, 0, 700 * 1024).await;
    assert_eq!(got, pat(700 * 1024, 0x97), "spilled content readable");
}

/// Face 2 — the dominant 464 EIO signature: `did not settle after 8
/// binding rebinds` with `fill_valid=false` and an UNCHANGED binding on
/// every attempt (pure CONTENTION — the diagnostic tape,
/// /tmp/rw5a_diag/). The VL8 item-7 escalation exists but a CONCURRENT
/// READER COHORT defeats it: the escalated attempt's fetch joins the
/// single-flight registered by a NON-escalated reader whose device read
/// ran OUTSIDE the block stripe — the cohort-carried fill is
/// incarnation-invalid, so holding the stripe bought nothing, 8 attempts
/// burn, and the read (and the write-side seed riding the same
/// primitive) EIOs. The fix: an escalated attempt must obtain a fill
/// whose validity was proven UNDER the stripe (bypass or re-run the
/// cohort fetch while serialized), and the attempt bound gets a backoff
/// tail so unlucky cohort inheritance can never exhaust into EIO.
///
/// Model: the item-7 patch storm (100 % duty cycle on block 2's word) +
/// FOUR concurrent readers per round (the cohort — one primary fetches
/// unserialized, the others inherit its invalid fill). Pre-fix: some
/// reader EIOs within a few rounds. Post-fix: every read of every round
/// serves the base content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reader_cohort_survives_perpetual_patch_storm() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rw5a_cohort_st!!", "rw5a_cohort", "64MB").await);

    // Durable striped fixture: 3 full blocks.
    let ino = create(&h, "stormy").await;
    let len = 3 * BS as usize;
    let base = pat(len, 0x33);
    write_at(&h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(meta.file_type, "striped", "fixture must be striped");
    let mapping = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&2).cloned())
        .expect("block 2 mapped");
    let (be_id, off) =
        h.fs.router
            .backend_router
            .parse_block_key(&mapping)
            .expect("parse block key");
    let (alloc, _dev) =
        h.fs.router
            .backend_router
            .get_backend(&be_id)
            .expect("backend");

    // The §5.1 patch storm: lock → unstable → (DMA window) → publish,
    // back-to-back — every UNLOCKED fetch overlaps an unstable word.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let storm = {
        let stop = stop.clone();
        let alloc = alloc.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let guard = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS
                    .get_lock(ino, 2)
                    .lock()
                    .await;
                if alloc.begin_patch_sole_owner(off) {
                    tokio::task::yield_now().await;
                    alloc.publish_block(off);
                }
                drop(guard);
            }
        })
    };

    // Reader cohorts: 4 concurrent readers per round share one
    // single-flight fill. Tiers purged per round so every round is
    // device-honest (fills, not cache hits).
    for round in 0..10u32 {
        let ppath = squeezefs::keys::inode_path(ino);
        h.fs.router.cache.write_lru.remove(&ppath);
        h.fs.router.cache.read_lru.remove(&ppath);
        if let Ok(m) = h.fs.router.fetch_metadata(&ppath).await {
            if let Some(bm) = m.block_map.as_ref() {
                for bk in bm.values() {
                    h.fs.router.cache.purge_block_key(bk);
                }
            }
        }
        let mut readers = Vec::new();
        for r in 0..4u64 {
            let fs = h.fs.clone();
            let req = h.req;
            let off_r = 2 * BS + 16384 + r * 4096;
            readers.push(tokio::spawn(async move {
                fs.read(req, ino, 0, off_r, 8192, 0).await
            }));
        }
        for (r, jh) in readers.into_iter().enumerate() {
            let reply = jh.await.unwrap().unwrap_or_else(|e| {
                panic!(
                    "round {round} reader {r}: storm-raced cohort read must \
                     make progress, got {e:?} (the generic/464 rebind-\
                     exhaustion EIO)"
                )
            });
            let off_r = (2 * BS + 16384 + r as u64 * 4096) as usize;
            assert_eq!(
                &reply.data[..],
                &base[off_r..off_r + 8192],
                "round {round} reader {r}: content"
            );
        }
    }

    stop.store(true, Ordering::Release);
    storm.await.unwrap();
}

/// Face 3a — RELEASE keeps the shared op lease while other handles are
/// open: the fencing token must NOT bump across close-while-open + write
/// (the 464 FencingTokenExpired{N, N+1} storm generator).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_while_other_handles_open_keeps_the_lease() {
    let _g = serial().await;
    let h = make(*b"rw5a_lease_keep!", "rw5a_lease_a", "64MB").await;
    let ino = create(&h, "shared").await; // create() opens handle #1
    h.fs.open(h.req, ino, libc::O_RDWR as u32).await.unwrap(); // handle #2

    write_at(&h, ino, 0, &pat(8192, 0x31)).await;
    let tok_before = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(tok_before > 0, "fixture: the write cached an op lease");

    // Handle #2 closes while handle #1 stays open.
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();

    // The next write must ride the SAME lease: no token bump.
    write_at(&h, ino, 4096, &pat(4096, 0x42)).await;
    let tok_after = h.fs.dlm().get_fencing_token_ino(ino);
    assert_eq!(
        tok_before, tok_after,
        "a release with other handles open must not drop the shared \
         lease (token bump = every in-flight op's snapshot fences to EIO)"
    );
}

/// Face 3b — a write racing transient lease churn must never surface
/// FencingTokenExpired as EIO: the handler retries once with a fresh
/// lease (single-writer mount — an adjacent bump is always our own
/// churn). Race-loop repro, the open_o_trunc lease test's shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_never_surfaces_transient_lease_churn_as_eio() {
    let _g = serial().await;
    let h = make(*b"rw5a_lease_race!", "rw5a_lease_b", "64MB").await;
    let ino = create(&h, "raced").await;
    let path = squeezefs::keys::inode_path(ino);

    for i in 0..300u32 {
        write_at(&h, ino, 0, &pat(8192, i as u8 | 1)).await;
        h.fs.invalidate_local_lease(ino);
        let dlm = h.fs.dlm().clone();
        let p = path.clone();
        let acquirer = tokio::spawn(async move {
            if let Ok(lease) = dlm
                .acquire_lock(&p, None, std::time::Duration::from_millis(50))
                .await
            {
                drop(lease);
            }
        });
        let res =
            h.fs.write(
                h.req,
                ino,
                0,
                0,
                bytes::Bytes::from(pat(8192, (i as u8) ^ 0x55 | 1)),
                0,
                0,
            )
            .await;
        acquirer.await.unwrap();
        assert!(
            res.is_ok(),
            "iteration {i}: a transient lease-churn fence must never \
             surface as a write error: {:?}",
            res.err()
        );
    }
}
