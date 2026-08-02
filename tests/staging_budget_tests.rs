//! Staging-budget correctness contracts.
//!
//! The NVMe staging admission gate (`current_staged_write_bytes` vs
//! `max_write_bytes`) must reflect the *live* staged bytes, and hitting the
//! cap must be self-healing (promotion drains the pool) — not a permanent
//! ratchet that degrades every staged write into a 2 s stall + synchronous
//! spill (the `squeezefs bench` small-file hang).
//!
//! Contracts:
//! 1. Re-staging the same growing file costs its latest size, not the sum of
//!    every intermediate payload.
//! 2. Unlink returns a staged file's budget.
//! 3. Writing past the cap keeps succeeding, drains the pool below the cap,
//!    and every file reads back byte-exact (sequential and concurrent).
//! 4. A write that spills to a direct block write (pool full) must be the
//!    data readers see — the stale ring entry for the same file_id must not
//!    shadow it. Budget is returned and the file stays fully usable.
//! 5. Remount seeds the budget from recovered *staged* entries only (orphan
//!    active blocks must not consume the staged budget), and recovered
//!    entries stay creditable.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

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

/// Per-entry metadata overhead allowance (staged header + serialized meta).
const SLACK: u64 = 4096;
/// 64 KiB blocks: inline <= 4 KiB, staged 4 KiB..64 KiB, striped > 64 KiB.
const BLOCK_SIZE: u64 = 65536;
/// Staged payload size used throughout: within the staged window.
const STAGED_LEN: usize = 60 * 1024;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    nvme: squeezefs::cache::nvme::NvmeStaging,
    max_write_bytes: u64,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make_with_write_cap(write_disk_cap: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK_SIZE.to_string());
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("staging_budget_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(write_disk_cap),
        ba.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let nvme = cache.nvme.clone();
    let max_write_bytes = nvme.max_write_bytes();
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
        req,
        nvme,
        max_write_bytes,
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

async fn read_all(h: &H, ino: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, 0, size, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} failed: {e:?}"))
        .data
        .to_vec()
}

/// Byte-exact fill check with a DIAGNOSTIC summary instead of two 60 KiB
/// vec dumps: first mismatch offset, its value, and the distinct foreign
/// bytes — enough to tell zeros (lost payload) from another file's fill
/// (cross-entry clobber) from a torn mix.
fn assert_content(got: &[u8], fill: u8, what: &str) {
    if got.len() != STAGED_LEN {
        panic!(
            "{what} corrupt after capacity churn: short read {} of {STAGED_LEN}",
            got.len()
        );
    }
    if let Some(pos) = got.iter().position(|&b| b != fill) {
        let mut foreign: Vec<u8> = got.iter().copied().filter(|&b| b != fill).collect();
        foreign.sort_unstable();
        foreign.dedup();
        let foreign_count = got.iter().filter(|&&b| b != fill).count();
        panic!(
            "{what} corrupt after capacity churn: first mismatch at {pos:#x} \
             (got {:#04x}, want {fill:#04x}); {foreign_count}/{} bytes foreign, \
             distinct foreign values {foreign:x?}",
            got[pos],
            got.len()
        );
    }
}

fn budget(h: &H) -> u64 {
    h.nvme.current_staged_write_bytes()
}

/// Wait (bounded) for the background merge worker to drain the budget below
/// `target`. Coordinates on `space_freed_notify` — no sleep-based sync.
async fn wait_budget_below(h: &H, target: u64, secs: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let cur = budget(h);
        if cur < target {
            return cur;
        }
        let notified = h.nvme.space_freed_notify.notified();
        tokio::pin!(notified);
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return budget(h);
        }
    }
}

/// Contract 1: a file re-staged as it grows must cost its latest payload,
/// not the sum of every intermediate stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_restage_same_file_does_not_leak_staging_budget() {
    let h = make_with_write_cap("64MB").await;
    let ino = create(&h, "grow.bin").await;

    // 4 growing writes: 16 -> 32 -> 48 -> 60 KiB, all within the staged window.
    let chunk = vec![0xAAu8; 16 * 1024];
    write_at(&h, ino, 0, &chunk).await;
    write_at(&h, ino, 16 * 1024, &chunk).await;
    write_at(&h, ino, 32 * 1024, &chunk).await;
    write_at(&h, ino, 48 * 1024, &vec![0xAAu8; 12 * 1024]).await;

    let used = budget(&h);
    assert!(
        used <= STAGED_LEN as u64 + SLACK,
        "staging budget leaked on re-stage: {used} bytes counted for one \
         {STAGED_LEN}-byte staged file (sum-of-payloads ratchet)"
    );
    assert_eq!(read_all(&h, ino, 60 * 1024).await, vec![0xAAu8; STAGED_LEN]);
}

/// Contract 2: unlink returns the staged budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unlink_returns_staging_budget() {
    let h = make_with_write_cap("64MB").await;
    // Storage reclaim runs on the background pool started by FUSE init.
    h.fs.init(h.req).await.expect("fuse init failed");
    let ino = create(&h, "gone.bin").await;
    write_at(&h, ino, 0, &vec![0xBBu8; STAGED_LEN]).await;

    let before = budget(&h);
    assert!(
        before >= STAGED_LEN as u64,
        "staged write not accounted: {before}"
    );

    h.fs.unlink(h.req, 1, OsStr::new("gone.bin"))
        .await
        .expect("unlink failed");
    // Storage reclaim is deferred to FORGET (open-unlinked semantics); mirror
    // the kernel's sequence — close the create() handle, then FORGET — and
    // wait (bounded) for the queued reclaim.
    h.fs.release(h.req, ino, 0, 0, 0, true)
        .await
        .expect("release failed");
    h.fs.forget(h.req, ino, 1).await;

    let after = wait_budget_below(&h, SLACK + 1, 15).await;
    assert!(
        after <= SLACK,
        "unlink+forget did not return staging budget: {before} -> {after}"
    );
}

/// Contract 3 (the bench hang): writing far past the staging cap must keep
/// succeeding at staged-write speed, drain the pool below the cap, and every
/// file must read back byte-exact — sequentially and concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_staging_full_writes_recover_and_drain() {
    // 2 MiB cap => ~34 staged files of 60 KiB fit; we write 50.
    let h = make_with_write_cap("2MB").await;
    assert!(
        h.max_write_bytes <= 2 * 1024 * 1024,
        "test needs a tiny staging cap, got {}",
        h.max_write_bytes
    );

    let wave_start = std::time::Instant::now();
    let mut inos = Vec::new();
    for i in 0..50u32 {
        let ino = create(&h, &format!("seq_{i}.bin")).await;
        write_at(&h, ino, 0, &vec![i as u8; STAGED_LEN]).await;
        inos.push((ino, i as u8));
    }
    let wave_elapsed = wave_start.elapsed();

    // Dead-drain regression: with promotion wired, over-cap writes are
    // admitted after a short wait for freed space — not a hardcoded 4x500ms
    // futile stall + synchronous spill per file (~2s each, 16+ files here).
    assert!(
        wave_elapsed < std::time::Duration::from_secs(20),
        "staged writes past the cap degraded to stall+spill: 50 writes took {wave_elapsed:?}"
    );

    // The pool must not be left pinned at the cap: promotion + exact
    // accounting keep it below the admission ceiling (resident data below
    // the high-water mark is the intended fast path, not a leak).
    let drained = wait_budget_below(&h, h.max_write_bytes, 30).await;
    assert!(
        drained < h.max_write_bytes,
        "staging budget pinned after write burst: {} / {} (dead drain)",
        drained,
        h.max_write_bytes
    );

    for (ino, fill) in &inos {
        let got = read_all(&h, *ino, STAGED_LEN as u32).await;
        assert_content(&got, *fill, &format!("sequential file {fill}"));
    }

    // Concurrent wave across the same saturated pool.
    let fsarc = Arc::new(h);
    let mut tasks = Vec::new();
    for t in 0..8u32 {
        let h = fsarc.clone();
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for f in 0..2u32 {
                let fill = 100 + (t * 2 + f) as u8;
                let ino = create(&h, &format!("conc_{t}_{f}.bin")).await;
                write_at(&h, ino, 0, &vec![fill; STAGED_LEN]).await;
                out.push((ino, fill));
            }
            out
        }));
    }
    let mut conc = Vec::new();
    for t in tasks {
        conc.extend(t.await.expect("writer task panicked"));
    }
    for (ino, fill) in conc {
        let got = read_all(&fsarc, ino, STAGED_LEN as u32).await;
        assert_content(&got, fill, &format!("concurrent file {fill}"));
    }

    let end = wait_budget_below(&fsarc, fsarc.max_write_bytes, 30).await;
    assert!(
        end < fsarc.max_write_bytes,
        "staging budget pinned after concurrent wave: {} / {}",
        end,
        fsarc.max_write_bytes
    );
}

/// Contract 4: when the pool is full and a write spills to a direct block
/// write, readers must see the spilled (newest) data — not a stale staged
/// ring entry left behind under the same file_id — and the budget must be
/// returned so the file stays fully usable afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_spilled_write_reads_back_new_data_and_returns_budget() {
    let h = make_with_write_cap("2MB").await;
    let ino = create(&h, "spill.bin").await;

    // v1 goes to the staging ring.
    write_at(&h, ino, 0, &vec![0xAAu8; STAGED_LEN]).await;
    let staged_cost = budget(&h);
    assert!(staged_cost >= STAGED_LEN as u64, "v1 not staged");

    // Artificially pin the gauge at the cap so the next stage attempt is
    // rejected deterministically and must take the spill path.
    h.nvme
        .current_staged_write_bytes
        .fetch_add(h.max_write_bytes, std::sync::atomic::Ordering::Relaxed);

    // v2 spills. It must still succeed, and it must be what readers see.
    write_at(&h, ino, 0, &vec![0xBBu8; STAGED_LEN]).await;
    let got = read_all(&h, ino, STAGED_LEN as u32).await;
    assert_eq!(
        got,
        vec![0xBBu8; STAGED_LEN],
        "read returned stale pre-spill staged data"
    );

    // Unpin the artificial pressure.
    h.nvme
        .current_staged_write_bytes
        .fetch_sub(h.max_write_bytes, std::sync::atomic::Ordering::Relaxed);

    // The spilled write must have released v1's ring budget.
    let after_spill = budget(&h);
    assert!(
        after_spill <= SLACK,
        "spill did not return the stale ring entry budget: {after_spill}"
    );

    // The file must remain fully writable/readable on the normal staged path.
    write_at(&h, ino, 0, &vec![0xCCu8; STAGED_LEN]).await;
    let got = read_all(&h, ino, STAGED_LEN as u32).await;
    assert_eq!(got, vec![0xCCu8; STAGED_LEN], "post-spill rewrite corrupt");
}

/// Contract 6 (bench EIO): the staging ring must never destroy other live
/// staged entries to make room — staged data is the sole durable copy until
/// promotion/spill, so shard exhaustion must surface as `StorageFull`
/// backpressure (loud), and an entry that cannot fit at all must be an
/// error, not a silent success-with-drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stage_write_shard_full_is_loud_never_lossy() {
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("staging_shard_full").await.unwrap());
    let dir = tempdir().unwrap();
    // < 10 MiB write cap => exactly one shard of exactly this capacity.
    let cache = TieredCache::new(
        vec![dir.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("1MB"),
        ba.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let st = cache.nvme.clone();

    let payload = |fill: u8| vec![fill; 300 * 1024];
    for (id, fill) in [("id-a", 0xA1u8), ("id-b", 0xB2), ("id-c", 0xC3)] {
        st.stage_write(&format!("/{id}"), id, bytes::Bytes::from(payload(fill)), 1)
            .await
            .unwrap_or_else(|e| panic!("stage {id} failed: {e:?}"));
    }

    // A fourth 300 KiB entry cannot fit without destroying id-a: it must be
    // refused loudly (no promotion is wired on this bare cache).
    let res = st
        .stage_write("/id-d", "id-d", bytes::Bytes::from(payload(0xD4)), 1)
        .await;
    assert!(
        res.is_err(),
        "over-capacity stage_write silently destroyed a live staged entry"
    );

    // An entry larger than the whole pool must be an error, not a silent
    // success-with-drop.
    let res = st
        .stage_write(
            "/id-huge",
            "id-huge",
            bytes::Bytes::from(vec![0xEE; 2 * 1024 * 1024]),
            1,
        )
        .await;
    assert!(res.is_err(), "oversized stage_write reported success");

    // Every previously staged payload must still be intact.
    for (id, fill) in [("id-a", 0xA1u8), ("id-b", 0xB2), ("id-c", 0xC3)] {
        let got = st
            .read_staged(id)
            .unwrap_or_else(|| panic!("staged entry {id} destroyed by a neighbor's stage"));
        assert_eq!(got, payload(fill), "staged entry {id} corrupted");
    }

    // Re-staging an existing id here must wrap onto its *own* live block —
    // an in-place overwrite of the sole copy is torn on crash, so it must be
    // refused loudly too, leaving every entry (including id-a) intact.
    let res = st
        .stage_write(
            "/id-a",
            "id-a",
            bytes::Bytes::from(vec![0x5A; 200 * 1024]),
            1,
        )
        .await;
    assert!(
        res.is_err(),
        "self-overlapping re-stage must be refused (crash-torn otherwise)"
    );
    for (id, fill) in [("id-a", 0xA1u8), ("id-b", 0xB2), ("id-c", 0xC3)] {
        assert_eq!(
            st.read_staged(id)
                .expect("entry destroyed by refused re-stage"),
            payload(fill),
            "staged entry {id} corrupted"
        );
    }
}

/// PR 3 eviction-latency guard (zero-copy write-path design §5.5): the
/// guard-backed flush holds a staging-shard READ lock across exactly one
/// transform-or-DMA (plus the sampled verify on verification mounts). That
/// bound must keep same-pool writers/evictors live: concurrent staging churn
/// (put/read/remove — all shard write-lock ops) racing a full guard-backed
/// flush wave must complete promptly, and every flushed file must read back
/// byte-exact. A retained guard (cache put, batch-future capture, drop after
/// `remove_active_block`) turns this into a stall/deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_flush_guard_holds_do_not_stall_staging_churn() {
    let h = make_with_write_cap("16MB").await;

    // Four striped files, each with one content-complete STAGED active
    // block plus a partial RAM tail: the first oversized write of a fresh
    // file routes through the router's direct striped path, so grow the
    // file first, then overwrite — the non-block-multiple overwrite of a
    // striped file goes through write_file_staged and stages block 0.
    let mut inos = Vec::new();
    for i in 0..4u32 {
        let fill = 0x40 + i as u8;
        let ino = create(&h, &format!("guard_{i}.bin")).await;
        write_at(&h, ino, 0, &vec![!fill; BLOCK_SIZE as usize + 1]).await;
        write_at(&h, ino, 0, &vec![fill; BLOCK_SIZE as usize + 1]).await;
        inos.push((ino, fill));
    }

    let h = Arc::new(h);

    // (a) Flush everything: guard-backed DMAs in flight across the pool.
    let flusher = {
        let h = h.clone();
        tokio::spawn(async move { h.fs.force_flush_all_staged_data().await })
    };

    // (b) Same-pool churn: put/read/remove active blocks while the flush
    // wave holds staging read guards across device writes.
    let churner = {
        let h = h.clone();
        tokio::spawn(async move {
            let payload = vec![0xEEu8; BLOCK_SIZE as usize];
            for i in 0..64u64 {
                let key = format!("active_block:inode_777000:block_{}", i % 4);
                let hh = h.clone();
                let k = key.clone();
                let p = payload.clone();
                let admitted =
                    tokio::task::spawn_blocking(move || hh.nvme.put_active_block(&k, &p, 1))
                        .await
                        .expect("churn put panicked");
                let _ = h.nvme.current_staged_write_bytes();
                if admitted {
                    let hh = h.clone();
                    let k = key.clone();
                    tokio::task::spawn_blocking(move || hh.nvme.remove_active_block(&k))
                        .await
                        .expect("churn remove panicked");
                }
            }
        })
    };

    let joined = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let (f, c) = tokio::join!(flusher, churner);
        f.expect("flush task panicked").expect("flush failed");
        c.expect("churn task panicked");
    })
    .await;
    assert!(
        joined.is_ok(),
        "guard-backed flush DMAs starved same-pool staging churn \
         (evictor/writer wait no longer bounded — §5.5 hold-bound regression)"
    );

    // Flushed data intact after the churn race.
    for (ino, fill) in inos {
        let got = read_all(&h, ino, BLOCK_SIZE as u32 + 1).await;
        assert_eq!(
            got,
            vec![fill; BLOCK_SIZE as usize + 1],
            "file {fill:#x} corrupt after concurrent flush + staging churn"
        );
    }
}

/// Contract 5: remount seeds the budget from recovered *staged* entries only;
/// orphan active blocks must not consume the staged budget, and recovered
/// staged entries must still be creditable (ledger survives recovery).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_remount_budget_counts_staged_entries_only() {
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("staging_budget_remount").await.unwrap());
    let dir = tempdir().unwrap();

    // Both sessions carry the SAME filesystem generation: the remount
    // seeding contract below runs BEHIND a matching generation gate (the
    // reformat-over-stale-staging fix must not regress warm restarts).
    let mk = || {
        TieredCache::new(
            vec![dir.path().to_path_buf()],
            Some("64MB"),
            Some("64MB"),
            Some("16MB"),
            Some("8MB"),
            ba.clone(),
            nvme_dev.clone(),
            Some("staging-budget-remount-generation"),
        )
    };

    // Session A: one staged file + one orphan active block, then "crash".
    let a = mk().await.unwrap();
    a.nvme
        .stage_write(
            "/f1",
            "file-id-1",
            bytes::Bytes::from(vec![0x11u8; STAGED_LEN]),
            7,
        )
        .await
        .expect("stage_write failed");
    assert!(
        a.nvme
            .put_active_block("active_block:/big:0", &vec![0x22u8; BLOCK_SIZE as usize], 7),
        "active block put refused with an empty pool"
    );
    drop(a);

    // Session B: recovery must seed the budget from the staged entry only.
    let bch = mk().await.unwrap();
    let seeded = bch.nvme.current_staged_write_bytes();
    assert!(
        seeded >= STAGED_LEN as u64,
        "recovered staged entry not counted: {seeded}"
    );
    assert!(
        seeded <= STAGED_LEN as u64 + SLACK,
        "remount budget counts non-staged ring entries (orphan active block): {seeded}"
    );

    // Recovered entries must be creditable.
    assert!(
        bch.nvme.remove_staged("file-id-1").is_some(),
        "recovered staged entry unreadable"
    );
    let after = bch.nvme.current_staged_write_bytes();
    assert!(
        after <= SLACK,
        "removing a recovered staged entry did not return budget: {after}"
    );
}
