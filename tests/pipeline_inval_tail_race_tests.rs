//! The detached pipeline upload's invalidation tail vs a concurrent
//! same-block writer — the 2026-08-04 squeeze-test 13:10 battery crash
//! (daemon `d3edac27`, shim write row):
//!
//! ```text
//! PANIC occurred: panicked at src/fuse_client.rs:10423:
//! parked entry cannot vanish under the held block lock
//! ```
//!
//! Mechanism: the ACK-before-DMA pipeline task publishes, retires its
//! overlay UNDER the block guard, **drops the guard**, then runs
//! `upload_invalidation_tail` — whose first act retires the same key
//! AGAIN, now unguarded (it also removes the staged sibling). A new
//! write to the same block that took the freed lock, parked a fresh
//! overlay entry, and is mid-window (absorb/sibling awaits) loses its
//! entry out from under its held lock: the merge's
//! `.expect("parked entry cannot vanish under the held block lock")`
//! panics, the handler task dies with the FUSE reply, and the mount is
//! gone (`Transport endpoint is not connected` for every later row).
//! The same unguarded window can remove the NEW write's staged custody
//! — silent acked-byte loss, strictly worse than the panic.
//!
//! Contract: the pipeline task's invalidation tail runs UNDER its still
//! -held block guard — a concurrent writer serializes behind it and the
//! parked-entry invariant is real. Deterministic schedule via two seams
//! (`set_test_inval_tail_stall_ms` — parks the tail in the overlap
//! window; `set_test_checkout_stall_ms` — holds the second write inside
//! its guarded window), the `SQUEEZEFS_TEST_WRITE_STALL_MS` pattern:
//! load selects such schedules, the seams select them deterministically.
//!
//! RED against `d3edac27`: write B's future panics with the field
//! message verbatim. GREEN with the guard-held tail: both writes land,
//! read-back serves B's bytes exactly.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
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

/// Restore default posture on scope exit (knob hygiene): both seams off,
/// W1 patch back on.
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_test_inval_tail_stall_ms(0);
        squeezefs::fuse_client::set_test_checkout_stall_ms(0);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
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
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

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
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
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

/// The field schedule, deterministic. The field wrote 1 MiB segments on
/// 4 MiB blocks — the coverage-union overlay leg (never whole-block
/// one-shots), so the repro uses the same 4-segment shape scaled to the
/// harness block (4 × 1 KiB on 4 KiB):
///
/// * write A = segments 1–4 of block 0; the 4th completes the coverage
///   union → write-through → the detached pipeline task publishes,
///   retires its overlay under the guard, drops the guard, and PARKS in
///   the tail seam for 250 ms;
/// * write B (1 KiB at block 0, offset 0) — takes the freed lock,
///   parks a fresh overlay entry, and HOLDS inside its guarded window
///   for 1500 ms (the checkout seam);
/// * A's tail fires at ~250 ms — squarely inside B's held window.
///
/// Pre-fix: the unguarded tail retires B's entry; B's merge panics with
/// the field message (`parked entry cannot vanish under the held block
/// lock`). Post-fix: the tail runs under A's still-held guard, so B
/// only parks after the tail completed — the overlap is structurally
/// gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_inval_tail_cannot_retire_a_concurrent_writers_parked_entry() {
    let _g = serial().await;
    let _seams = SeamGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let h = make_harness("inval_tail_race").await;
    let seg = (FBS / 4) as usize; // 4 segments per block — the field shape

    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new("f1"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;

    // Prime: a 4-block striped file (the field files were pre-existing;
    // 4 blocks is the striped_fixture premise shape), fully quiesced so
    // the primer's own pipeline tasks are gone before the schedule arms.
    let blocks = 4u64;
    let v0 = pattern((blocks * FBS) as usize, 1);
    h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&v0), 0, 0)
        .await
        .expect("prime write");
    h.fs.fsync(h.req, ino, 0, false).await.expect("prime fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "primer must drain"
    );
    {
        let path = squeezefs::keys::inode_path(ino);
        h.fs.router.metadata_cache.remove(&ino);
        let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
        assert_eq!(meta.file_type, "striped", "premise: striped");
    }

    // Write A, segments 1..4 of block 0 (sub-block → the coverage-union
    // overlay leg). Tail seam armed before the union-completing 4th
    // segment; checkout seam stays 0 so A's own segments fly through.
    let wt_base = squeezefs::fuse_client::METRICS
        .write_through_blocks
        .load(std::sync::atomic::Ordering::Relaxed);
    let va = pattern(FBS as usize, 2);
    for i in 0..3usize {
        h.fs.write(
            h.req,
            ino,
            0,
            (i * seg) as u64,
            bytes::Bytes::copy_from_slice(&va[i * seg..(i + 1) * seg]),
            0,
            0,
        )
        .await
        .expect("write A segment");
    }
    squeezefs::fuse_client::set_test_inval_tail_stall_ms(250);
    h.fs.write(
        h.req,
        ino,
        0,
        (3 * seg) as u64,
        bytes::Bytes::copy_from_slice(&va[3 * seg..]),
        0,
        0,
    )
    .await
    .expect("write A final segment");
    // A's pipeline task is detached (ACK-before-DMA). Launch B only once
    // the task RETIRED its overlay (the write_through_blocks bump happens
    // strictly between the retire and the tail) — before that instant B
    // would win the lock first and the supersession law would (correctly)
    // re-drive A; the field crash needs B parking INSIDE the tail window.
    let t0 = std::time::Instant::now();
    while squeezefs::fuse_client::METRICS
        .write_through_blocks
        .load(std::sync::atomic::Ordering::Relaxed)
        == wt_base
    {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(20),
            "A's pipeline task never reached its publish/retire"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    // Write B: takes the lock the moment A's task drops it (pre-fix:
    // immediately — the tail is parked 250 ms in the seam; post-fix:
    // only after the tail completed under the guard), parks a fresh
    // overlay, and holds its guarded window 1500 ms — so A's tail fires
    // squarely inside it. Spawned so a pre-fix panic surfaces as a
    // JoinError instead of killing the test thread.
    squeezefs::fuse_client::set_test_checkout_stall_ms(1500);
    let fs_b = h.fs.clone();
    let req_b = h.req;
    let vb = pattern(seg, 3);
    let vb_send = bytes::Bytes::copy_from_slice(&vb);
    let b_task = tokio::spawn(async move {
        fs_b.write(req_b, ino, 0, 0, vb_send, 0, 0)
            .await
            .map(|w| w.written)
    });

    let joined = b_task.await;
    match joined {
        Ok(Ok(written)) => {
            assert_eq!(written as usize, vb.len(), "write B short");
        }
        Ok(Err(e)) => panic!("write B errored: {e:?}"),
        Err(join_err) => {
            // The RED face: the handler task panicked — the field's
            // daemon-killing schedule reproduced in-process.
            panic!(
                "write B's handler PANICKED (the field's 13:10 crash — the \
                 detached invalidation tail retired B's parked entry out \
                 from under its held block lock): {join_err:?}"
            );
        }
    }

    // Disarm before the drain so the residual pipeline tasks run clean.
    squeezefs::fuse_client::set_test_inval_tail_stall_ms(0);
    squeezefs::fuse_client::set_test_checkout_stall_ms(0);
    h.fs.fsync(h.req, ino, 0, false).await.expect("final fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );

    // Newest-wins read-back: B's segment over A's remainder, exactly.
    let got =
        h.fs.read(h.req, ino, 0, 0, FBS as u32, 0)
            .await
            .expect("read back")
            .data
            .to_vec();
    assert_eq!(&got[..seg], &vb[..], "segment 0 must serve write B's bytes");
    assert_eq!(
        &got[seg..],
        &va[seg..],
        "segments 1-3 must keep write A's bytes"
    );
}
