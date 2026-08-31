//! fstests generic/451 repro-port — the DIO-write page-coherence law.
//!
//! The found bug (dev @ a01b69bc, release gate 2026-08-03, full `-g auto`
//! fail-fast at test 445/787): `aio-dio-cycle-write` acked an async
//! O_DIRECT whole-file rewrite (io_getevents returned), then a buffered
//! pread returned the PREVIOUS cycle's bytes (`0x55` where `0xaa` was
//! acked). The daemon's tiers were coherent the whole time — the same
//! checker's O_DIRECT read verify passed, the reader-free probe never
//! fails, and the stats deltas show the failing pread never even reached
//! the daemon: the stale bytes lived in the KERNEL PAGE CACHE, where
//! racing buffered readers had re-instantiated pre-write pages MID-write
//! (legal serves at the time) that then outlived the write's completion.
//!
//! Why the kernel didn't drop them: fuse invalidates the written range
//! BEFORE issuing DIO WRITEs (`fuse_direct_io`) but cannot order its
//! post-write invalidation before `io_getevents` returns on the async
//! path (absent or workqueue-deferred, kernel-line-dependent). Why this
//! surfaced now: PERF-6 (`f0f2a5bf`, FOPEN_KEEP_CACHE) stopped the
//! kernel's invalidate-on-every-open, which had been masking the ghosts —
//! `file_check` re-opens the file before its buffered pread.
//!
//! THE LAW (one enforcement point — the write handler's reply edge,
//! `SqueezefsFilesystem::post_dio_write_coherence`): **an acked O_DIRECT
//! WRITE must be preceded by a kernel page-cache invalidation of its
//! written range whenever the ino has buffered-open history.** The
//! userspace mirror of the VFS's `kiocb_invalidate_post_direct_write`,
//! awaited before the reply so the ack itself is the coherence barrier.
//! The sink is injectable (the `ipc_service::Invalidator` hook
//! precedent): production pushes a ranged FUSE_NOTIFY_INVAL_INODE through
//! the fuse3 Notify handle; this suite records the pushes in-process.
//!
//! Counter deltas are exact under the sanctioned serial gate
//! (`--test-threads=1`).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{DioInvalSink, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    /// Every `(ino, offset, len)` the DIO-coherence sink pushed. The sink
    /// is awaited INSIDE the write handler, so membership observed AFTER
    /// `write()` resolved proves the invalidation preceded the ack.
    invals: Arc<Mutex<Vec<(u64, u64, u32)>>>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new("dio451_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x0D10_4510_C0FF_EE00,
            uuid: *b"dio451-coherence",
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

    // The injectable coherence sink (the Invalidator-hook precedent):
    // record every push; production wires FUSE_NOTIFY_INVAL_INODE here.
    let invals: Arc<Mutex<Vec<(u64, u64, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = invals.clone();
    let sink: DioInvalSink = Arc::new(move |ino, off, len| {
        log.lock().unwrap().push((ino, off, len));
        Box::pin(std::future::ready(())) as futures::future::BoxFuture<'static, ()>
    });
    fs.dio_inval_sink.store(Arc::new(Some(sink)));

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        req,
        invals,
        _b: b,
        _m: m,
        _s: s,
    }
}

const O_DIRECT: u32 = libc::O_DIRECT as u32;

/// Create a regular file under root with the given CREATE open flags
/// (buffered `0` vs `O_DIRECT` — the history the gate keys on).
async fn create_file(h: &H, name: &str, flags: u32) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, flags)
        .await
        .unwrap()
        .attr
        .ino
}

/// One WRITE through the handler with the given `fuse_write_in.flags`
/// (the writing fd's open flags — the kernel echoes O_DIRECT here) and
/// `write_flags` (FUSE_WRITE_CACHE et al).
async fn write_at(h: &H, ino: u64, offset: u64, len: usize, flags: u32, write_flags: u32) {
    let payload = bytes::Bytes::from(vec![0xAAu8; len]);
    h.fs.write(h.req, ino, ino, offset, payload, write_flags, flags)
        .await
        .unwrap();
}

fn invals(h: &H) -> Vec<(u64, u64, u32)> {
    h.invals.lock().unwrap().clone()
}

fn inval_counter() -> u64 {
    METRICS.fuse_dio_write_invals.load(Ordering::Relaxed)
}

/// THE generic/451 repro: an ino with buffered-open history takes an
/// O_DIRECT write — the ack must be preceded by a kernel page-cache
/// invalidation covering exactly the written range. Red against the
/// found bug: nothing fired, so pages a racing buffered reader
/// re-instantiated mid-write served pre-write bytes forever after the
/// ack (the 0x55-after-0xaa verdict diff).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn odirect_write_after_buffered_open_invalidates_before_ack() {
    let h = make().await;
    let c0 = inval_counter();
    // Buffered lineage: the create IS a cacheable open (flags carry no
    // O_DIRECT), exactly like generic/451's xfs_io readers.
    let ino = create_file(&h, "t451", 0).await;

    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;

    assert_eq!(
        invals(&h),
        vec![(ino, 0, 16 * 1024)],
        "an acked O_DIRECT write on a buffered-open ino MUST be preceded \
         by a ranged kernel page invalidation (generic/451: stale pages a \
         racing buffered reader faulted in mid-write outlive the ack \
         otherwise — the kernel's own async-DIO invalidation cannot be \
         ordered before io_getevents returns)"
    );
    assert!(
        inval_counter() > c0,
        "fuse_dio_write_invals must count the engagement"
    );
}

/// The perf gate: a pure-DIO lineage (no buffered open since the kernel
/// last held the ino) pays ZERO — no sideband round trip on device-true
/// O_DIRECT write rows. The first buffered open arms the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pure_dio_lineage_pays_no_invalidation_until_a_buffered_open() {
    let h = make().await;
    // aio-dio-cycle-write's init shape: O_DIRECT|O_CREAT.
    let ino = create_file(&h, "puredio", O_DIRECT).await;
    h.fs.open(h.req, ino, O_DIRECT, 0).await.unwrap();

    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;
    assert!(
        invals(&h).is_empty(),
        "a pure-DIO lineage must not pay the coherence notify (no kernel \
         pages can exist for it)"
    );

    // The generic/451 reader arrives: one buffered open arms the gate.
    h.fs.open(h.req, ino, 0, 0).await.unwrap();
    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;
    assert_eq!(
        invals(&h),
        vec![(ino, 0, 16 * 1024)],
        "the first buffered open must arm the DIO-write coherence gate"
    );
}

/// Buffered writes never fire the sink (they go THROUGH the kernel page
/// cache), and neither do writeback-origin writes (FUSE_WRITE_CACHE) even
/// when the kernel picked an O_DIRECT ff to carry them — writeback IS the
/// kernel cache writing itself out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_and_writeback_writes_never_fire_the_sink() {
    let h = make().await;
    let ino = create_file(&h, "buffered", 0).await;

    write_at(&h, ino, 0, 16 * 1024, 0, 0).await;
    write_at(
        &h,
        ino,
        0,
        16 * 1024,
        O_DIRECT,
        fuse3::raw::flags::FUSE_WRITE_CACHE,
    )
    .await;

    assert!(
        invals(&h).is_empty(),
        "only kernel-path O_DIRECT writes owe the post-write invalidation"
    );
}

/// The RES-13 bound: the final FORGET sweeps the buffered-open history —
/// the kernel cannot hold pages for an inode it evicted, so a later
/// pure-DIO writer pays nothing until a fresh buffered open re-arms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_forget_sweeps_the_buffered_history() {
    let h = make().await;
    let ino = create_file(&h, "swept", 0).await;
    // Return the CREATE's handle and its one lookup ref — the kernel
    // evicting the inode (drop_caches / memory pressure).
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();
    h.fs.forget(h.req, ino, 1).await;

    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;
    assert!(
        invals(&h).is_empty(),
        "final FORGET must sweep the buffered-open history (no kernel \
         inode ⇒ no kernel pages ⇒ no notify owed)"
    );

    h.fs.open(h.req, ino, 0, 0).await.unwrap();
    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;
    assert_eq!(
        invals(&h),
        vec![(ino, 0, 16 * 1024)],
        "a fresh buffered open after the sweep must re-arm the gate"
    );
}

/// The QUIESCENT-LINEAGE elision (write-wall campaign, 2026-08-13 — the
/// per-inode write conviction: `fuse_dio_write_invals` = every op of a
/// 367k-op rand-4k row, each an AWAITED kernel invalidate round trip
/// serialized on the kernel's per-inode invalidate mutex — the measured
/// ~4.5-effective-concurrency cap on one file): once the buffered
/// lineage is QUIESCENT — zero live buffered handles AND no
/// page-instantiating op since the last invalidation — the range is
/// PROVEN page-free and the notify is owed nothing. One inval latches
/// the clean state; the storm elides the rest. The generic/451 law is
/// untouched: any live buffered handle (a racing reader) keeps
/// per-segment invalidation, and any READ serve / buffered open /
/// WRITE_CACHE write un-latches (the gen bump orders BEFORE the reply
/// that lets the kernel instantiate the page — a mid-inval racer can
/// never be latched over).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiescent_dio_storm_invalidates_once_not_per_write() {
    let h = make().await;
    let c0 = inval_counter();
    // Buffered lineage that goes QUIESCENT: create buffered (arms),
    // then release the one buffered handle (fio's layout-pass shape).
    let ino = create_file(&h, "quiescent", 0).await;
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();

    // The storm: 64 O_DIRECT writes. The FIRST owes the invalidation
    // (pages may predate the release); the rest are proven page-free.
    for i in 0..64u64 {
        write_at(&h, ino, i * 4096, 4096, O_DIRECT, 0).await;
    }
    assert_eq!(
        invals(&h).len(),
        1,
        "a quiescent buffered lineage pays ONE invalidation, not one per \
         write (the per-op awaited kernel round trip was the measured \
         single-file write-concurrency cap)"
    );
    assert_eq!(
        inval_counter() - c0,
        1,
        "the engagement counter mirrors the single paid notify"
    );

    // A buffered reader arriving UN-latches: its open bumps the gen.
    h.fs.open(h.req, ino, 0, 0).await.unwrap();
    write_at(&h, ino, 0, 4096, O_DIRECT, 0).await;
    assert_eq!(
        invals(&h).len(),
        2,
        "a fresh buffered open must un-latch the clean state (the \
         generic/451 racing-reader law is untouched)"
    );
}

/// The un-latch vector the storm cannot see: a READ serve on the
/// (handle-free) ino — the kernel re-instantiating pages via readahead
/// on a still-referenced mapping — must break the clean latch so the
/// NEXT O_DIRECT write invalidates again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_serve_unlatches_the_clean_state() {
    let h = make().await;
    let ino = create_file(&h, "readunlatch", 0).await;
    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();

    // Latch: first storm write invals, second elides.
    write_at(&h, ino, 0, 4096, O_DIRECT, 0).await;
    write_at(&h, ino, 4096, 4096, O_DIRECT, 0).await;
    let after_latch = invals(&h).len();

    // A buffered READ serve (flags without O_DIRECT) — page-instantiating.
    let _ = h.fs.read(h.req, ino, ino, 0, 4096, 0).await.unwrap();

    write_at(&h, ino, 8192, 4096, O_DIRECT, 0).await;
    assert_eq!(
        invals(&h).len(),
        after_latch + 1,
        "a READ serve must un-latch (the served bytes let the kernel \
         instantiate a page the next DIO write must kill)"
    );
}

/// Kernel-split parallel DIO segments (FOPEN_PARALLEL_DIRECT_WRITES —
/// splits are the NORMAL case) each invalidate their OWN range: the AIO
/// completes only when every segment acked, so the union of the ranged
/// invalidations covers the user's write at io_getevents time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_segments_each_invalidate_their_own_range() {
    let h = make().await;
    let ino = create_file(&h, "segments", 0).await;

    write_at(&h, ino, 64 * 1024, 16 * 1024, O_DIRECT, 0).await;
    write_at(&h, ino, 0, 16 * 1024, O_DIRECT, 0).await;

    assert_eq!(
        invals(&h),
        vec![(ino, 64 * 1024, 16 * 1024), (ino, 0, 16 * 1024)],
        "every acked O_DIRECT segment must invalidate exactly its own \
         written range (out-of-order segments are the normal case)"
    );
}

/// Finding 37 (RED pre-fix: the write future never completes — the
/// bounded outer wait trips): the reply-edge coherence notify must
/// never park the reply UNBOUNDEDLY. The from-zero fstests acceptance
/// wedged 9 hours in generic/208: every ring slot's WRITE parked in
/// this await while the kernel's invalidation waited on folio
/// laundering whose writeback WRITEs needed a free ring slot — a
/// delivery-capacity deadlock only a reply can break (the zc serve's
/// wider dirty-folio window makes it near-deterministic; classical
/// rides the same cliff edge). The law: bounded park, then DETACH the
/// notify (it completes as soon as the acks free the ring — the
/// self-healing order) and degrade THAT write's ordering to the
/// kernel's own best-effort posture (`dio_warn_stale_pagecache`
/// parity), loud and counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_coherence_notify_never_parks_the_reply_forever() {
    let h = make().await;
    let ino = create_file(&h, "t37", 0).await;

    // The wedged-kernel analog: a notify that never completes until
    // released — exactly the folio-laundering cycle's shape.
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let entered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let entered = entered.clone();
        let sink: DioInvalSink = std::sync::Arc::new(move |_ino, _off, _len| {
            let entered = entered.clone();
            let mut rx = release_rx.clone();
            Box::pin(async move {
                entered.fetch_add(1, Ordering::Relaxed);
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            }) as futures::future::BoxFuture<'static, ()>
        });
        h.fs.dio_inval_sink.store(std::sync::Arc::new(Some(sink)));
    }

    // The write must ACK within a bound even though its notify is
    // wedged (pre-fix: parks forever alongside generic/208's 32).
    let write = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        h.fs.write(
            h.req,
            ino,
            ino,
            0,
            bytes::Bytes::from(vec![0x37u8; 16 * 1024]),
            0,
            O_DIRECT,
        ),
    )
    .await;
    assert!(
        write.is_ok(),
        "a wedged coherence notify must never park the WRITE reply \
         forever (finding 37: the ring-capacity deadlock — the reply is \
         what frees the slots the kernel's laundering needs)"
    );
    write.unwrap().expect("the write itself succeeds");
    assert_eq!(
        entered.load(Ordering::Relaxed),
        1,
        "the notify was entered (the ordering attempt is real; only the \
         unbounded park is forbidden)"
    );

    // The detached notify completes once released — nothing leaks.
    release_tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}
