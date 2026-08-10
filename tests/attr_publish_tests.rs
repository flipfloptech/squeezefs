//! PR 2 of `docs/design-write-inode-convoy.md` — the §4.4 audit rows for
//! attr publication (rows 3/13 = KD-5, row 22 = KD-6, row 1 pin), red-first.
//!
//! The defect class (KD-5): every attr publisher today is a bare
//! `attr_cache.insert` — the write postlude is a get→overwrite→insert RMW,
//! and the refetch-class publishers (lookup/link/refresh/expiry) republish
//! the LAGGING durable inode verbatim. Under MetaPrepOnly the postlude
//! already runs guard-free (the guard drops before data I/O), so these
//! races are live TODAY and become load-bearing the moment PR 3 admits
//! Shared writers. The migration is ONE atomic merge domain
//! (`attr_publish_locks` + the `publish_attr` door): times = signed max
//! for ambient publishers / explicit-set for setattr; size = explicit-set
//! (truncate) else floor-max; the write postlude's size claim publishes
//! only when the entry-time claim was genuine growth (KD-6 — a
//! size-neutral completion must never resurrect a stale-high size over a
//! newer truncate).
//!
//! RED (PR 2 seam commit): T1 (refetch regression), T2 (blind-stamp vs
//! the durable fold), T3 (concurrent postlude mtime regression), T4
//! (KD-6 truncate resurrection), and the P2 one-door scan fail; T5 and
//! P1 are green pins of contracts the migration must not break.
//!
//! Schedules are DETERMINISTIC via the `SQUEEZEFS_TEST_ATTR_PUBLISH_STALL_MS`
//! seam (the postlude-edge park: data landed, no guards held, publish not
//! yet run) — load selects these interleavings, the seam selects them
//! deterministically.

use fuse3::raw::prelude::Filesystem;
use fuse3::Timestamp;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    set_test_attr_publish_stall_ms, test_attr_publish_stall_entries, SqueezefsFilesystem, METRICS,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 65536;

/// One suite-wide serializer: the seam cells, the env-selected cache arm
/// and the global metric counters are process-wide — tests in this file
/// must not interleave (the PR 1 bench SERIAL precedent). Async (tokio)
/// so holding it across the tests' awaits is legal.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII disarm: a panicking test must not leave the seam armed for the
/// next one.
struct SeamOff;
impl Drop for SeamOff {
    fn drop(&mut self) {
        set_test_attr_publish_stall_ms(0);
    }
}

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3")
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

/// `read_mostly`: the KD-5 law must hold identically over BOTH cache
/// backings (`SQUEEZEFS_READ_MOSTLY_CACHE` is read at construction).
async fn make(tag: &str, read_mostly: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    std::env::set_var(
        "SQUEEZEFS_READ_MOSTLY_CACHE",
        if read_mostly { "1" } else { "0" },
    );
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    // Restore the default so an unrelated concurrent construction (none
    // in this serialized suite, but hygiene) sees the shipped posture.
    std::env::set_var("SQUEEZEFS_READ_MOSTLY_CACHE", "1");
    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
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

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) -> u32 {
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
    .unwrap_or_else(|e| panic!("write off {off} failed: {e:?}"))
    .written
}

/// Grow a file striped to `blocks` whole blocks, make it durable, and
/// drain the pipeline so the layout is published and quiet.
async fn grow_striped(h: &H, ino: u64, blocks: u64) {
    let base = vec![0x11u8; (blocks * BS) as usize];
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline drains"
    );
    // Warm both floor caches the way a real writer is warmed.
    let _ = h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");
}

fn cached_size(h: &H, ino: u64) -> Option<u64> {
    h.fs.attr_cache.peek_with(&ino, |(a, _)| a.size)
}

fn cached_mtime(h: &H, ino: u64) -> Option<Timestamp> {
    h.fs.attr_cache.peek_with(&ino, |(a, _)| a.mtime)
}

async fn wait_stall_entries(base: u64, want: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while test_attr_publish_stall_entries() - base < want {
        assert!(
            std::time::Instant::now() < deadline,
            "writer never reached the postlude seam"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// T1 — §4.4 row 3, the refetch-class publishers (KD-5), both cache arms.
// ---------------------------------------------------------------------------

/// A LOOKUP after an acked-but-not-yet-persisted write republishes the
/// lagging durable inode verbatim today: the reply carries the stale
/// size and the insert clobbers the freshest acked floor out of the attr
/// cache (killing PR 3's Shared size floor with it). The door's
/// refetch-floor merge must keep both at the acked size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookup_republish_never_regresses_acked_size() {
    let _s = SERIAL.lock().await;
    for (arm, rm) in [("rm", true), ("moka", false)] {
        let h = make(&format!("ap_lookup_{arm}"), rm).await;
        let ino = create(&h, "f").await;
        // Staged write: size persist is DEFERRED to the flush cadence, so
        // the durable inode still says 0 while 32 KiB are acked.
        let data = vec![0x5Au8; 32 * 1024];
        assert_eq!(write_at(&h, ino, 0, &data).await as usize, data.len());
        assert_eq!(
            cached_size(&h, ino),
            Some(32 * 1024),
            "postlude published the acked size ({arm} arm)"
        );

        let reply =
            h.fs.lookup(h.req, 1, OsStr::new("f"))
                .await
                .expect("lookup");
        assert_eq!(
            reply.attr.size,
            32 * 1024,
            "LOOKUP must reply the freshest acked size, not the lagging \
             durable inode ({arm} arm — KD-5 refetch-floor)"
        );
        assert_eq!(
            cached_size(&h, ino),
            Some(32 * 1024),
            "the LOOKUP publication must not regress the cached floor \
             ({arm} arm — KD-5)"
        );
    }
}

// ---------------------------------------------------------------------------
// T2 — §4.4 rows 3/12: the postlude blind stamp vs the durable fold.
// ---------------------------------------------------------------------------

/// The durable times plane is signed-max monotone (`park_times_refinement`
/// + the fold), but the postlude BLIND-stamps the attr cache — so after
/// utimes(future)-then-write the served mtime and the refetched mtime
/// disagree (stat jumps forward when the cache expires: the generic/003
/// divergence class, daemon-side). The door's ambient merge must make the
/// cached value equal what a cold refetch serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postlude_stamp_matches_the_durable_fold() {
    let _s = SERIAL.lock().await;
    let h = make("ap_fold", true).await;
    let ino = create(&h, "t").await;
    write_at(&h, ino, 0, &vec![1u8; 8192]).await;

    // Explicit utimes into the future — an authoritative commit.
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let future = Timestamp::new(now_sec + 100_000, 0);
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mtime: Some(future),
            ..Default::default()
        },
    )
    .await
    .expect("utimes(future)");

    // A write AFTER the explicit future stamp.
    write_at(&h, ino, 0, &vec![2u8; 8192]).await;
    let served = cached_mtime(&h, ino).expect("cached attr present");

    // The cold truth: what the backend fold serves.
    h.fs.attr_cache.invalidate(&ino);
    let refetched =
        h.fs.getattr(h.req, ino, None, 0)
            .await
            .expect("getattr refetch")
            .attr
            .mtime;

    assert_eq!(
        served, refetched,
        "the cached mtime after a write must equal what a cold refetch \
         serves — the postlude's blind stamp diverges the served view \
         from the durable fold (KD-5 signed-max merge law)"
    );
}

// ---------------------------------------------------------------------------
// T3 — §4.4 row 3's named red: two concurrent writes cannot regress mtime.
// ---------------------------------------------------------------------------

/// The design's row-3 red, made deterministic by the postlude seam:
/// writer A parks at the postlude edge, writer B (a later stamp) fully
/// publishes, then A wakes and blind-stamps its OLDER time over B's.
/// Guard modes are irrelevant — MetaPrepOnly already runs this window
/// guard-free today, and PR 3's Shared class widens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_write_postludes_cannot_regress_mtime() {
    let _s = SERIAL.lock().await;
    for (arm, rm) in [("rm", true), ("moka", false)] {
        let h = make(&format!("ap_race_{arm}"), rm).await;
        let ino = create(&h, "r").await;
        write_at(&h, ino, 0, &vec![0u8; 8192]).await;

        let _off = SeamOff;
        let base = test_attr_publish_stall_entries();
        set_test_attr_publish_stall_ms(1500);
        let fs_a = h.fs.clone();
        let req = h.req;
        let w_a = tokio::spawn(async move {
            fs_a.write(
                req,
                ino,
                0,
                0,
                bytes::Bytes::from_static(&[0xAAu8; 8192]),
                0,
                0,
            )
            .await
        });
        // A is parked at the postlude edge (stamp t1 captured).
        wait_stall_entries(base, 1).await;
        set_test_attr_publish_stall_ms(0);
        // A strictly later coarse tick for B's stamp.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        write_at(&h, ino, 0, &[0xBBu8; 8192]).await;
        let after_b = cached_mtime(&h, ino).expect("B published");

        w_a.await.unwrap().expect("writer A completes");
        let final_mtime = cached_mtime(&h, ino).expect("attr present");
        assert!(
            final_mtime >= after_b,
            "writer A's postlude regressed mtime below writer B's \
             ({final_mtime:?} < {after_b:?}, {arm} arm) — the KD-5 merge \
             domain must be signed-max monotone"
        );
    }
}

// ---------------------------------------------------------------------------
// T4 — §4.4 row 22 (KD-6): the size-neutral postlude resurrection.
// ---------------------------------------------------------------------------

/// KD-6's dangerous direction, deterministic: a within-EOF write parks at
/// the postlude edge, a truncate fully lands (both caches + durable say
/// BS), then the writer wakes and publishes `max(cached, entry-time
/// old_size)` — resurrecting a size NO legal serialization of the two ops
/// produces (write-then-truncate ⇒ BS; truncate-then-write ⇒ 2·BS+8 KiB).
/// A size-neutral completion must publish times only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_is_never_resurrected_by_a_size_neutral_postlude() {
    let _s = SERIAL.lock().await;
    let h = make("ap_trunc", true).await;
    let ino = create(&h, "k").await;
    grow_striped(&h, ino, 4).await;

    let _off = SeamOff;
    let base = test_attr_publish_stall_entries();
    set_test_attr_publish_stall_ms(2000);
    let fs_w = h.fs.clone();
    let req = h.req;
    // Within-EOF sub-block overwrite: entry-time old_size = 4·BS,
    // expected_new_size = 4·BS — a size-NEUTRAL completion.
    let w = tokio::spawn(async move {
        fs_w.write(
            req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::from_static(&[0x77u8; 8192]),
            0,
            0,
        )
        .await
    });
    wait_stall_entries(base, 1).await;

    // The truncate lands COMPLETELY inside the writer's postlude window.
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(BS),
            ..Default::default()
        },
    )
    .await
    .expect("truncate to BS");
    assert_eq!(cached_size(&h, ino), Some(BS), "truncate published BS");

    set_test_attr_publish_stall_ms(0);
    let written = w.await.unwrap().expect("write completes").written;
    assert_eq!(written, 8192, "the acked write stays acked");

    assert_eq!(
        cached_size(&h, ino),
        Some(BS),
        "a size-neutral postlude resurrected the pre-truncate size — \
         KD-6: it must publish times only (legal outcomes here are BS \
         alone; 4·BS is a size no serialization of the two ops produces)"
    );
}

// ---------------------------------------------------------------------------
// T5 — §4.4 row 22 (KD-6): the fencing-retry contract (green pin).
// ---------------------------------------------------------------------------

/// A stale cached lease fences at the data path's entry check; the
/// handler's one-retry arm must re-acquire fresh and RE-RUN the write —
/// and the postlude must still publish coherently. Pins the behavior
/// KD-6's PR 3 protocol re-run extends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_write_retries_rerun_and_publish_coherently() {
    let _s = SERIAL.lock().await;
    let h = make("ap_fence", true).await;
    let ino = create(&h, "z").await;
    grow_striped(&h, ino, 4).await;

    // Bump the generation while the cached lease stays held — the
    // FIND-M11-A transient-bump shape.
    let path = squeezefs::keys::inode_path(ino);
    squeezefs::dlm::test_bump_fencing_generation(&path);

    let ok0 = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let data = vec![0x3Cu8; 8192];
    assert_eq!(write_at(&h, ino, 2 * BS, &data).await as usize, data.len());
    assert!(
        METRICS.lease_acquire_ok.load(Ordering::Relaxed) > ok0,
        "the retry must re-acquire a FRESH lease, not resume the stale one"
    );
    let back =
        h.fs.read(h.req, ino, 0, 2 * BS, 8192, 0)
            .await
            .expect("read back")
            .data
            .to_vec();
    assert_eq!(back, data, "the retried write's bytes serve");
    assert_eq!(
        cached_size(&h, ino),
        Some(4 * BS),
        "a within-EOF retried write publishes a coherent (unchanged) size"
    );
}

// ---------------------------------------------------------------------------
// P1 — §4.4 row 1 (green pin): no lease double-mint under a writer storm.
// ---------------------------------------------------------------------------

/// 32 concurrent first-touch writers (no cached lease) mint exactly ONE
/// lease: `get_or_acquire_lease_bounded`'s double-check under
/// `lease_locks` is the (a)-row machinery PR 3's Shared class leans on.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn first_touch_writer_storm_mints_one_lease() {
    let _s = SERIAL.lock().await;
    let h = make("ap_storm", true).await;
    let ino = create(&h, "s").await;
    grow_striped(&h, ino, 33).await;
    // Drop the cached lease so every writer is genuinely first-touch.
    h.fs.invalidate_local_lease(ino);

    let ok0 = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let mut js = tokio::task::JoinSet::new();
    for i in 0..32u64 {
        let fs = h.fs.clone();
        let req = h.req;
        js.spawn(async move {
            fs.write(
                req,
                ino,
                0,
                i * BS,
                bytes::Bytes::from_static(&[0x44u8; 4096]),
                0,
                0,
            )
            .await
        });
    }
    while let Some(r) = js.join_next().await {
        r.unwrap().expect("storm write");
    }
    assert_eq!(
        METRICS.lease_acquire_ok.load(Ordering::Relaxed) - ok0,
        1,
        "32 first-touch writers must mint exactly one lease (§4.4 row 1)"
    );
}

// ---------------------------------------------------------------------------
// P2 — the ONE-door scan: every attr publication routes through the domain.
// ---------------------------------------------------------------------------

/// The env-knob-convention pattern for the attr merge domain: a raw
/// `attr_cache.insert` outside the `publish_attr` door is a NEW unmerged
/// publisher — drift is red. (Today: 12 raw publishers — the defect.)
#[test]
fn attr_publications_route_through_the_one_door() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut raw_sites = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).expect("read src dir") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).expect("read source");
            let normalized: String = src.split_whitespace().collect::<Vec<_>>().join("");
            let count = normalized.matches("attr_cache.insert(").count();
            if count > 0 {
                raw_sites.push(format!("{}: {count}", p.display()));
            }
        }
    }
    assert_eq!(
        raw_sites,
        vec![format!(
            "{}: 1",
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/fuse_client.rs")
                .display()
        )],
        "every attr publication must route through the ONE `publish_attr` \
         door (KD-5 — the single allowed raw insert is the door's own); \
         raw publishers found: {raw_sites:?}"
    );
}
