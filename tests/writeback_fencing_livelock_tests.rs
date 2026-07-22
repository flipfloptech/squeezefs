//! FIND-M11-A regression contracts: the fencing-token constant-writeback
//! livelock (2026-07-15 metadata-throughput closing gate, incident_013/
//! incident_476).
//!
//! The never-lossy writeback retry ladder (`requeue_or_hard_fail`) documents
//! that "genuinely superseded units (fencing/NotFound) never reach this
//! ladder: the flush unit itself resolves them as clean no-ops" — but a
//! `WritebackRequest` snapshots its fencing token at staging time and the
//! flush unit re-presents that SAME token forever. Any later lease
//! acquisition on the ino (open/close churn — fsstress shape) bumps the DLM
//! fencing generation, so the unit's merge fails `FencingTokenExpired`
//! deterministically on every retry: a livelock, not a retry. Teardown's
//! authoritative sweep flushed with the CURRENT token but keyed the staged-
//! entry removal on that same presented token, so entries staged under an
//! older generation leaked in the ring forever (`staged_writes_in_flight`
//! never drained — the kill-9 unmount wedge).
//!
//! Contracts pinned here (the incident's three faces):
//! 1. A unit that is STILL the staging owner (staged token == unit token)
//!    but whose token lags the DLM generation must reach durability — the
//!    bytes are acked custody, never-lossy — instead of retrying forever.
//! 2. A unit whose staging was re-staged under a newer token (staged token
//!    != unit token) is SUPERSEDED and must resolve as a clean no-op; the
//!    newest staging's own unit carries the durability promise, and the
//!    newest bytes win.
//! 3. Teardown's force-flush must fully DRAIN stale-token staged entries
//!    (flush + remove + `staged_writes_in_flight` → 0) so unmount teardown
//!    is bounded — never a drain-wait spin on entries nothing can remove.
//! 4. fsstress-shaped epoch churn (write → stage → lease invalidate/
//!    re-acquire storm racing the writeback worker) must CONVERGE once the
//!    churn quiesces: staged active blocks drain, newest bytes durable and
//!    readable, teardown summary clean.
//! 5. A unit whose inode was unlinked + reclaimed after staging is
//!    superseded by the delete: verify-then-discard the orphan staged
//!    entry (the recovery contract's "missing inode meta discards orphan
//!    active blocks", applied live) — never a NotFound retry storm, never
//!    a resurrected dead inode.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Process-global env (`SQUEEZEFS_DEFAULT_BLOCK_SIZE`) + process-global
/// METRICS: serialize the tests in this binary (house pattern).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// 4 KiB blocks: the smallest striped shape — partial-block writes park in
/// RAM, complete blocks write through, staging carries `active_block:` keys.
const BLOCK_SIZE: u64 = 4096;

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
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
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
    nvme: squeezefs::cache::nvme::NvmeStaging,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(test_id: &str) -> H {
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK_SIZE.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
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
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let nvme = cache.nvme.clone();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
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
        fs,
        dlm,
        req,
        nvme,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap_or_else(|e| panic!("create {name} failed: {e:?}"))
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_range(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

/// Bump the ino's DLM fencing generation `n` times the way open/close churn
/// does: drop the mount's cached lease, re-acquire (INCR), release.
async fn bump_fencing(h: &H, ino: u64, n: u64) -> u64 {
    let path = format!("inode_{ino}");
    for _ in 0..n {
        h.fs.invalidate_local_lease(ino);
        let lease = h
            .dlm
            .acquire_lock(&path, None, Duration::from_secs(5))
            .await
            .expect("bump acquire_lock");
        drop(lease); // release: the next acquisition INCRs again
    }
    h.dlm.get_fencing_token_ino(ino)
}

/// Poll `cond` every 100 ms until it holds or `deadline` elapses.
async fn wait_until<F: FnMut() -> bool>(mut cond: F, deadline: Duration) -> bool {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn staged_active_keys(nvme: &squeezefs::cache::nvme::NvmeStaging) -> Vec<String> {
    nvme.list_staged_files()
        .into_iter()
        .filter(|k| k.starts_with("active_block:"))
        .collect()
}

/// Contract 1 (the incident's spinning-unit face, S == T < DLM generation):
/// a writeback unit that is STILL the staging owner but whose captured
/// fencing token lags the DLM generation (later open/close churn bumped it)
/// must flush its acked custody bytes to durability — the staged entry
/// drains and the bytes read back — instead of cycling
/// `FencingTokenExpired` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_token_unit_still_owner_flushes_to_durability() {
    let _serial = serial().await;
    let h = make("m11a_stale_owner").await;
    let ino = create(&h, "stale_owner.bin").await;

    // Promote to striped (block 0 write-through) and park a partial block 1.
    write_at(&h, ino, 0, &vec![0x11u8; 4097]).await;
    let payload = vec![0xAAu8; 2048];
    write_at(&h, ino, BLOCK_SIZE, &payload).await;

    // Stage block 1 under the CURRENT token, then age it: three open/close
    // churn cycles bump the DLM generation past the staged stamp — the
    // exact incident state (token 24/28/31 vs expected 35).
    let stale_token = h.dlm.get_fencing_token_ino(ino);
    h.fs.flush_memory_buffers_for_inode(ino, stale_token)
        .await
        .expect("stage block 1");
    let key = squeezefs::keys::active_block(ino, 1).to_string();
    assert_eq!(
        h.nvme.get_staged_fencing_token(&key),
        Some(stale_token),
        "harness: block 1 must be staged under the write-era token"
    );
    let current = bump_fencing(&h, ino, 3).await;
    assert!(
        current > stale_token,
        "harness: churn must advance the fencing generation"
    );

    // Start the writeback worker and require convergence: the unit must
    // resolve (durable merge + staged-entry removal), not livelock.
    h.fs.init(h.req).await.expect("fuse init");
    let nvme = h.nvme.clone();
    let key_probe = key.clone();
    let drained = wait_until(
        move || nvme.get_staged_fencing_token(&key_probe).is_none(),
        Duration::from_secs(15),
    )
    .await;
    assert!(
        drained,
        "FIND-M11-A livelock: staged {key} still present after 15 s — the \
         writeback unit is cycling FencingTokenExpired (token {stale_token} \
         vs generation {current}) instead of flushing acked custody bytes \
         to durability (never-lossy) or resolving as superseded"
    );

    // Never-lossy proof: the bytes are durable in the block map and read
    // back exactly.
    let meta =
        h.fs.router
            .fetch_metadata(&format!("inode_{ino}"))
            .await
            .expect("fetch metadata");
    let mapped = meta
        .block_map
        .as_ref()
        .map(|m| m.contains_key(&1))
        .unwrap_or(false);
    assert!(
        mapped,
        "block 1 drained from staging but never reached the block map — \
         staged custody was DROPPED, violating never-lossy"
    );
    assert_eq!(
        read_range(&h, ino, BLOCK_SIZE, 2048).await,
        payload,
        "block 1 bytes corrupted across the stale-token flush"
    );
}

/// Contract 2 (the superseded face, S != T): a unit whose staging was
/// re-staged under a newer token resolves as a clean no-op — the ladder
/// doc's "genuinely superseded units never reach this ladder" — and the
/// NEWEST staging's bytes are what reaches durability.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_unit_resolves_as_noop_newest_bytes_win() {
    let _serial = serial().await;
    let h = make("m11a_superseded").await;
    let ino = create(&h, "superseded.bin").await;

    write_at(&h, ino, 0, &vec![0x11u8; 4097]).await;

    // Staging #1: block 1 = 0xAA under token T1 (unit U1 queued; the worker
    // is not started yet, so the queue holds both units).
    write_at(&h, ino, BLOCK_SIZE, &vec![0xAAu8; 2048]).await;
    let t1 = h.dlm.get_fencing_token_ino(ino);
    h.fs.flush_memory_buffers_for_inode(ino, t1)
        .await
        .expect("stage #1");

    // Churn bumps the generation, then staging #2 re-stages block 1 = 0xBB
    // under T2 > T1 (unit U2 queued). U1 is now SUPERSEDED: the staged
    // entry's token no longer matches its captured token.
    bump_fencing(&h, ino, 1).await;
    let newest = vec![0xBBu8; 2048];
    write_at(&h, ino, BLOCK_SIZE, &newest).await;
    let t2 = h.dlm.get_fencing_token_ino(ino);
    assert!(t2 > t1, "harness: T2 must be a newer generation than T1");
    h.fs.flush_memory_buffers_for_inode(ino, t2)
        .await
        .expect("stage #2");
    let key = squeezefs::keys::active_block(ino, 1).to_string();
    assert_eq!(
        h.nvme.get_staged_fencing_token(&key),
        Some(t2),
        "harness: staged entry must carry the re-stage token"
    );

    // More churn ages BOTH units below the DLM generation (the incident's
    // multi-unit pile: tokens 24/28/31 all behind expected 35).
    let current = bump_fencing(&h, ino, 2).await;
    assert!(current > t2);

    // Worker on: U1 must no-op (superseded), U2 must flush to durability.
    h.fs.init(h.req).await.expect("fuse init");
    let nvme = h.nvme.clone();
    let key_probe = key.clone();
    let drained = wait_until(
        move || nvme.get_staged_fencing_token(&key_probe).is_none(),
        Duration::from_secs(15),
    )
    .await;
    assert!(
        drained,
        "FIND-M11-A livelock: staged {key} still present after 15 s — \
         superseded unit U1 (token {t1}) and owner unit U2 (token {t2}) are \
         cycling FencingTokenExpired against generation {current} instead \
         of resolving (no-op / durable flush)"
    );

    assert_eq!(
        read_range(&h, ino, BLOCK_SIZE, 2048).await,
        newest,
        "superseded unit's stale bytes must never displace the newest \
         staging's bytes"
    );
}

/// Contract 3 (the teardown face): the dismount force-flush must fully
/// drain stale-token staged entries — flush AND remove, with
/// `staged_writes_in_flight` reaching 0 — so the `destroy` drain-wait is
/// bounded. The incident teardown spun kill-9-deep because entries staged
/// under older generations could be flushed but never removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_force_flush_drains_stale_token_entries_bounded() {
    let _serial = serial().await;
    let h = make("m11a_teardown").await;
    let ino = create(&h, "teardown.bin").await;

    write_at(&h, ino, 0, &vec![0x11u8; 4097]).await;
    let payload = vec![0xCCu8; 2048];
    write_at(&h, ino, BLOCK_SIZE, &payload).await;
    let stale_token = h.dlm.get_fencing_token_ino(ino);
    h.fs.flush_memory_buffers_for_inode(ino, stale_token)
        .await
        .expect("stage block 1");
    bump_fencing(&h, ino, 3).await;

    // No worker: teardown's authoritative sweep owns the staged data.
    let summary = tokio::time::timeout(
        Duration::from_secs(30),
        h.fs.flush_all_staged_blocks_to_backend(),
    )
    .await
    .expect("teardown force-flush must be bounded (no fencing livelock)");

    assert_eq!(
        summary.failed, 0,
        "teardown force-flush failed stale-token units instead of flushing \
         them authoritatively: {:?}",
        summary.error_samples
    );
    assert_eq!(summary.flushed, summary.attempted, "partial teardown flush");
    let leaked = staged_active_keys(&h.nvme);
    assert!(
        leaked.is_empty(),
        "teardown force-flush LEAKED staged entries it flushed (removal \
         keyed on the presented token instead of the staged stamp): {leaked:?} \
         — destroy's drain-wait spins on these until kill -9"
    );
    assert_eq!(
        h.nvme
            .staged_writes_in_flight
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "staged_writes_in_flight never drained — the destroy drain-wait \
         livelock"
    );
    assert_eq!(
        read_range(&h, ino, BLOCK_SIZE, 2048).await,
        payload,
        "teardown flush corrupted block 1"
    );
}

/// Contract 5 (the NotFound face of the same ladder break — surfaced by the
/// churn-amplified 013 rig: `Io(NotFound "Inode N not found")` cycling
/// attempts 0→3 forever, 48 spins for one ino until process exit): a unit
/// whose inode was unlinked + reclaimed after staging is SUPERSEDED BY THE
/// DELETE. Its merge fails NotFound on every retry (the inode record is
/// gone; v3 inos are never reused), so the unit must verify the inode is
/// truly absent and DISCARD the orphan staged entry ("missing inode meta
/// discards orphan active blocks" — the recovery contract, applied live)
/// instead of spinning forever.
///
/// The incident's leak shape (RAM meta evicted ⇒ the size-derived sweep
/// missed beyond-size blocks) was closed at the source by the VL10
/// occupancy-index prefix sweep (`27c29d9` — delete now enumerates the
/// staged overlay by key prefix, size-independent), so the harness
/// rebuilds the orphan DIRECTLY after reclaim: the surviving crash-window
/// analog (staged pre-crash, sweep raced / process died mid-delete). The
/// worker-side discard ladder is the contract under test either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaimed_inode_unit_discards_orphan_staging_no_spin() {
    let _serial = serial().await;
    let h = make("m11a_reclaimed").await;
    let ino = create(&h, "reclaimed.bin").await;

    // Blocks 0–1 persist; block 5 (beyond the persisted layout — the
    // shape the extent fold refuses) STAGES at flush and its writeback
    // UNIT enters the queue. The worker is not running yet, so the unit
    // waits for init below.
    write_at(&h, ino, 0, &vec![0x11u8; 4097]).await;
    write_at(&h, ino, 5 * BLOCK_SIZE, &vec![0xDDu8; 2048]).await;
    let token = h.dlm.get_fencing_token_ino(ino);
    h.fs.flush_memory_buffers_for_inode(ino, token)
        .await
        .expect("stage");
    let key = squeezefs::keys::active_block(ino, 5).to_string();
    assert!(
        h.nvme.get_staged_fencing_token(&key).is_some(),
        "harness: block 5 must stage (the fold-refused shape) so its \
         writeback unit is queued"
    );

    // Close + unlink + reclaim (the fsstress open/write/close/unlink
    // grammar): the inode record is destroyed (monotonic inos — never
    // reused) and delete's occupancy-index sweep clears the overlay.
    h.fs.release(h.req, ino, 0, 0, 0, false)
        .await
        .expect("release");
    h.fs.unlink(h.req, 1, OsStr::new("reclaimed.bin"))
        .await
        .expect("unlink");
    h.fs.reclaim_orphaned_batch(vec![ino]).await;
    let attr_post_reclaim =
        h.fs.meta_backend
            .as_ref()
            .expect("meta backend")
            .getattr(ino)
            .await;
    assert!(
        attr_post_reclaim.is_err(),
        "harness: reclaim must have destroyed the inode record: \
         {attr_post_reclaim:?}"
    );

    // Delete's occupancy-index sweep (the VL10 linger fix) removed the
    // staged entry — but block 5's flush-era UNIT is still queued.
    // Rebuild the staged source under its flush-era token: the
    // crash-window analog (an orphan entry for a destroyed inode). The
    // queued unit must walk the full ladder — upload → merge → NotFound
    // → authoritative absence probe → DISCARD — never spin.
    assert!(
        h.nvme.get_staged_fencing_token(&key).is_none(),
        "harness: delete's sweep must have cleared the staged entry"
    );
    h.nvme
        .put_active_block_async(key.clone(), bytes::Bytes::from(vec![0xDDu8; 2048]), token)
        .await
        .expect("stage the orphan entry");
    assert!(
        h.nvme.get_staged_fencing_token(&key).is_some(),
        "harness: the orphan staged entry is the repro precondition"
    );

    // Worker on: the unit must resolve by discarding the orphan — never
    // spin NotFound forever, never resurrect metadata for a dead inode.
    h.fs.init(h.req).await.expect("fuse init");
    let nvme = h.nvme.clone();
    let key_probe = key.clone();
    let discarded = wait_until(
        move || nvme.get_staged_fencing_token(&key_probe).is_none(),
        Duration::from_secs(15),
    )
    .await;
    assert!(
        discarded,
        "FIND-M11-A (NotFound face): orphan staged {key} for reclaimed ino \
         {ino} still present after 15 s — the writeback unit is cycling \
         Io(NotFound) through the retry ladder instead of discarding \
         superseded-by-delete custody"
    );
    assert_eq!(
        h.nvme
            .staged_writes_in_flight
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "orphan discard must drain staged_writes_in_flight (bounded \
         teardown)"
    );
    // The dead inode must NOT have been resurrected by the flush.
    let attr =
        h.fs.meta_backend
            .as_ref()
            .expect("meta backend")
            .getattr(ino)
            .await;
    assert!(
        attr.is_err(),
        "flushing a reclaimed inode's orphan block resurrected it: {attr:?}"
    );
}

/// Contract 4 (the statistical incident shape): fsstress-class epoch churn —
/// concurrent writers staging partial blocks while open/close churn bumps
/// fencing generations, racing the live writeback worker — must CONVERGE
/// once the churn stops: staged active blocks drain, the newest bytes are
/// durable and readable, and a final teardown sweep is clean. The incident
/// binary wedged here (27,479 spinning FencingTokenExpired errors, kill -9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsstress_shaped_epoch_churn_converges_after_quiesce() {
    let _serial = serial().await;
    let h = Arc::new(make("m11a_churn").await);
    h.fs.init(h.req).await.expect("fuse init");

    const WRITERS: usize = 3;
    const ROUNDS: usize = 20;
    let mut inos = Vec::new();
    for w in 0..WRITERS {
        let ino = create(&h, &format!("churn_{w}.bin")).await;
        write_at(&h, ino, 0, &vec![0x11u8; 4097]).await;
        inos.push(ino);
    }

    // Churn: write partial block → stage it (enqueue unit) → drop + re-acquire
    // the lease (bump the generation). The worker races every step.
    let mut tasks = tokio::task::JoinSet::new();
    for (w, &ino) in inos.iter().enumerate() {
        let h = h.clone();
        tasks.spawn(async move {
            for round in 0..ROUNDS {
                let fill = 0x20u8 + ((w * ROUNDS + round) % 0x60) as u8;
                let payload = vec![fill; 2048];
                // A racing invalidate/bump can stale the cached lease
                // between ops; one retry re-acquires (the FUSE app-retry
                // shape). Persistent failure is a real regression.
                let mut wrote = false;
                for _ in 0..3 {
                    match h
                        .fs
                        .write(
                            h.req,
                            ino,
                            0,
                            BLOCK_SIZE,
                            bytes::Bytes::copy_from_slice(&payload),
                            0,
                            0,
                        )
                        .await
                    {
                        Ok(_) => {
                            wrote = true;
                            break;
                        }
                        Err(_) => {
                            h.fs.invalidate_local_lease(ino);
                        }
                    }
                }
                assert!(wrote, "churn write ino {ino} round {round} never landed");
                let token = h.dlm.get_fencing_token_ino(ino);
                h.fs.flush_memory_buffers_for_inode(ino, token)
                    .await
                    .expect("churn stage");
                // open/close churn: bump the generation under the worker.
                h.fs.invalidate_local_lease(ino);
                let lease = h
                    .dlm
                    .acquire_lock(&format!("inode_{ino}"), None, Duration::from_secs(5))
                    .await
                    .expect("churn bump");
                drop(lease);
            }
        });
    }
    let mut done = 0usize;
    while let Some(res) = tasks.join_next().await {
        res.expect("churn task panicked");
        done += 1;
    }
    assert_eq!(done, WRITERS);
    // Each writer's final fill is deterministic from its formula.
    let mut last_fill = std::collections::HashMap::new();
    for (w, &ino) in inos.iter().enumerate() {
        let fill = 0x20u8 + ((w * ROUNDS + (ROUNDS - 1)) % 0x60) as u8;
        last_fill.insert(ino, fill);
    }

    // Quiesce: every staged active block must drain (each staging enqueued
    // a unit; the last unit per block is the owner and must flush; earlier
    // ones are superseded no-ops).
    let nvme = h.nvme.clone();
    let converged = wait_until(
        move || staged_active_keys(&nvme).is_empty(),
        Duration::from_secs(25),
    )
    .await;
    let leftovers = staged_active_keys(&h.nvme);
    assert!(
        converged,
        "FIND-M11-A: churn never converged — staged active blocks still \
         spinning in the fencing ladder 25 s after quiesce: {leftovers:?}"
    );

    // Newest bytes visible + a clean teardown sweep behind them.
    for &ino in &inos {
        let expect = vec![last_fill[&ino]; 2048];
        assert_eq!(
            read_range(&h, ino, BLOCK_SIZE, 2048).await,
            expect,
            "ino {ino}: newest churned bytes lost"
        );
    }
    let summary = tokio::time::timeout(
        Duration::from_secs(30),
        h.fs.flush_all_staged_blocks_to_backend(),
    )
    .await
    .expect("post-churn teardown sweep must be bounded");
    assert_eq!(
        summary.failed, 0,
        "post-churn teardown sweep failed: {:?}",
        summary.error_samples
    );
    assert_eq!(
        h.nvme
            .staged_writes_in_flight
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "staged_writes_in_flight nonzero after convergence + sweep"
    );
}
