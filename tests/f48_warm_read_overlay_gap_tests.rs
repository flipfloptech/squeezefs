//! Finding 48 — a same-mount WARM read of a freshly written file and the
//! COLD (remount) read disagree on the block whose FIRST segment rode the
//! layout-promotion arm (boarded by the f46 train,
//! `.benchmarks/2026-09-02-f46-kvmap-stream-publish.md` §Boarded;
//! pre-existing — identical fingerprint on the pre-kvmap baseline).
//!
//! The field shape (squeeze-test, cacheless mount, 4 MiB blocks, fio
//! 1 MiB sequential libaio direct writes, close, warm `md5sum`, umount,
//! remount, cold `md5sum`): a fresh file's first write promotes inline →
//! striped and publishes block 0 as a bare key holding a PARTIAL image
//! (one segment); the block's other three segments install an
//! OVERWRITE-shape overlay record over that key whose never-written
//! quarter is a permanent gap, so coverage never completes and the record
//! stays Open past `close` (`overlay_open` = one per file). The first
//! read that crosses the block boundary drains it: gap seeded from the
//! old key, dest published, the binding FED to the rewrite epoch — RAM
//! only. Nothing owns that epoch: the reader's handle is clean, the
//! writer already closed, and the idle sweeper needs 30 s. A clean
//! unmount inside that window drops the fed binding, the durable map
//! keeps naming the promotion key, and the cold read serves ONE segment
//! plus never-written device bytes (zeros on a fresh target). The warm
//! bytes were right; the DURABLE image was wrong — the finding-44 corpse
//! shape ("block 0 reads zeros past its first segment after a remount").
//!
//! Two contracts, each red before the fix:
//! * a `close` settles the ino's Open records (no Open gap-bearing record
//!   survives a close; `overlay_open` → 0), so the release's own layout
//!   persist carries the binding;
//! * a clean unmount closes every open rewrite epoch, so a binding fed by
//!   a read drain (or by the unmount drain itself) is never lost.
//! Both legs assert warm == written == cold byte-exact.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::NamedTempFile;

const BS: u64 = 65536;
const SEG: u64 = BS / 4; // the field shape: 1 MiB segments on 4 MiB blocks
const WIN: u64 = BS / 16; // the kernel's readahead window: 256 KiB of 4 MiB

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
}

async fn make(b: &NamedTempFile, m: &NamedTempFile, alloc_ns: &str, format: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // The field's 1 MiB segments are patch-oversize on 4 MiB blocks.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    // The shipped posture: overlay ON (Bytes vehicle), ACK-early ON.
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    squeezefs::device_overlay::set_ack_early_for_tests(true, true);
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    // CACHELESS — the field mount: no staging dirs, so a fresh file's
    // first beyond-inline write promotes straight to striped.
    let cache = TieredCache::new(
        vec![],
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
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let routed: Arc<RoutedMetaBackend> = {
        if format {
            format_v3(
                m.path(),
                128 * 1024 * 1024,
                &FormatV3Options {
                    node_size: 64 * 1024,
                    journal_len_override: None,
                    force: true,
                    full_wipe: false,
                    format_config_xattr: None,
                },
            )
            .await
            .unwrap();
        }
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
        ..Default::default()
    };
    H { fs, req }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: Vec<u8>) {
    let len = data.len();
    let w =
        h.fs.write(h.req, ino, 0, off, bytes::Bytes::from(data), 0, 0)
            .await
            .unwrap_or_else(|e| panic!("write at {off} (block {}): {e:?}", off / BS));
    assert_eq!(w.written as usize, len, "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("READ at {off} (block {}): {e:?}", off / BS))
        .data
        .to_vec()
}

/// `close(2)` as the kernel delivers it: FLUSH then RELEASE.
async fn close(h: &H, ino: u64) {
    let _ = h.fs.flush(h.req, ino, ino, 0).await;
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

fn first_mismatch(got: &[u8], want: &[u8]) -> Option<String> {
    if got.len() != want.len() {
        return Some(format!("length {} != {}", got.len(), want.len()));
    }
    (0..got.len()).find(|&i| got[i] != want[i]).map(|i| {
        let blk = i as u64 / BS;
        let seg = (i as u64 % BS) / SEG;
        let zeros = got[i..]
            .iter()
            .take(SEG as usize)
            .filter(|&&x| x == 0)
            .count();
        format!(
            "first mismatch at byte {i} (block {blk}, segment {seg}): got {:#04x} want {:#04x}; \
             {zeros}/{SEG} of the segment reads zero",
            got[i], want[i]
        )
    })
}

fn backing_pair(tag: &str) -> (NamedTempFile, NamedTempFile) {
    // Backing under target/ — tmpfs refuses the O_DIRECT open the
    // zc_write_fd screen requires (the ack-early suite's rule).
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let b = tempfile::Builder::new()
        .prefix(&format!("f48-{tag}-b"))
        .tempfile_in(dir)
        .unwrap();
    b.as_file().set_len(64 * 1024 * 1024).unwrap();
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    (b, m)
}

/// Bounded poll (never a bare sleep-for-sync: the deadline is the
/// verdict, not the clock).
async fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if f() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    f()
}

/// The kernel's warm read shape: readahead-sized windows, two in flight,
/// sequential — the boundary-crossing window is what drains block 0.
async fn warm_read(h: &H, ino: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut off = 0u64;
    while off < len as u64 {
        let l1 = WIN.min(len as u64 - off) as usize;
        let l2 = WIN.min((len as u64).saturating_sub(off + WIN)) as usize;
        let (a, b) = if l2 > 0 {
            futures::future::join(read_at(h, ino, off, l1), read_at(h, ino, off + WIN, l2)).await
        } else {
            (read_at(h, ino, off, l1).await, Vec::new())
        };
        out.extend_from_slice(&a);
        out.extend_from_slice(&b);
        off += 2 * WIN;
    }
    out
}

/// The field composition on one block: promotion-arm first segment +
/// overwrite-record remainder, then (`close_first`) the kernel's close
/// or (else) a handle still open at unmount, warm reads in the kernel's
/// shapes, the product's clean unmount, remount, cold read.
async fn run(ns: &str, concurrent: bool, close_first: bool) {
    let _ = env_logger::builder().is_test(true).try_init();
    let (b, m) = backing_pair(ns);
    let h = make(&b, &m, ns, true).await;
    let ino = create(&h, "f48").await;

    let ord = Ordering::Relaxed;
    let ow0 = METRICS.overlay_overwrite_installs.load(ord);
    let st0 = METRICS.overlay_stores.load(ord) + METRICS.overlay_ack_early_stores.load(ord);
    let entire0 = METRICS.write_lock_scope_entire.load(ord);

    // Two blocks: block 0 is the composition under test; block 1 keeps
    // the file multi-block so a boundary-crossing read exists.
    let want = pattern(2 * BS as usize, 7);
    let seg = |i: u64| want[(i * SEG) as usize..((i + 1) * SEG) as usize].to_vec();
    // The FIRST write to the fresh file: the promotion arm.
    write_at(&h, ino, 0, seg(0)).await;
    assert_eq!(
        METRICS.write_lock_scope_entire.load(ord),
        entire0 + 1,
        "fixture: the first segment must ride the promotion arm (EntireOp scope)"
    );
    if concurrent {
        let mut w = Vec::new();
        for i in 1..8u64 {
            w.push(write_at(&h, ino, i * SEG, seg(i)));
        }
        futures::future::join_all(w).await;
    } else {
        for i in 1..8u64 {
            write_at(&h, ino, i * SEG, seg(i)).await;
        }
    }
    let ow1 = METRICS.overlay_overwrite_installs.load(ord);
    let st1 = METRICS.overlay_stores.load(ord) + METRICS.overlay_ack_early_stores.load(ord);
    assert_eq!(
        ow1 - ow0,
        1,
        "fixture: block 0's remainder must install exactly one OVERWRITE record"
    );
    assert!(
        st1 - st0 >= 3,
        "fixture: the remainder must ride overlay stores"
    );
    assert!(
        METRICS.overlay_open.load(ord) >= 1,
        "fixture: block 0's gap-bearing record must be Open before the close"
    );

    let mut open_after_close = None;
    if close_first {
        close(&h, ino).await;
        // The release's flush is backgrounded; a bounded poll is the
        // assertion's clock.
        let settled = eventually(|| METRICS.overlay_open.load(ord) == 0).await;
        open_after_close = Some((settled, METRICS.overlay_open.load(ord)));
    }

    let warm = warm_read(&h, ino, want.len()).await;
    let warm_verdict = first_mismatch(&warm, &want);
    let whole = read_at(&h, ino, 0, want.len()).await;
    let whole_verdict = first_mismatch(&whole, &want);
    eprintln!(
        "[f48 {ns}] after warm reads: gap serves {} drains {} gap seeds {} open {} epochs {}",
        METRICS.overlay_read_gap_serves.load(ord),
        METRICS.overlay_read_drains.load(ord),
        METRICS.overlay_gap_seeds.load(ord),
        METRICS.overlay_open.load(ord),
        METRICS.rewrite_shadow_open_epochs.load(ord),
    );

    // The product's clean unmount (DESTROY), then the true remount.
    h.fs.destroy(h.req).await;
    drop(h);
    let h2 = make(&b, &m, ns, false).await;
    let cold = read_at(&h2, ino, 0, want.len()).await;
    let cold_verdict = first_mismatch(&cold, &want);
    eprintln!(
        "[f48 {ns}] warm windowed {warm_verdict:?} / warm whole {whole_verdict:?} / cold {cold_verdict:?}"
    );

    assert!(
        warm_verdict.is_none(),
        "WARM windowed read differs from the written bytes: {}",
        warm_verdict.unwrap()
    );
    assert!(
        whole_verdict.is_none(),
        "WARM whole-file read differs from the written bytes: {}",
        whole_verdict.unwrap()
    );
    assert!(
        cold_verdict.is_none(),
        "COLD (clean unmount + remount) read differs from the written and warm-served \
         bytes: {} — acked bytes lost across a clean unmount",
        cold_verdict.unwrap()
    );
    if let Some((settled, open)) = open_after_close {
        assert!(
            settled,
            "an Open gap-bearing overlay record survived the close (overlay_open = {open})"
        );
    }
}

/// The field sequence: write, close, warm read, clean unmount, cold read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_file_warm_and_cold_reads_match_the_written_bytes() {
    let _g = serial().await;
    run("f48_closed", false, true).await;
}

/// The same with the block's segments in flight together (fio iodepth).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_file_warm_and_cold_reads_match_with_concurrent_segments() {
    let _g = serial().await;
    run("f48_closed_conc", true, true).await;
}

/// A handle still open at unmount: the warm read's drain feeds the
/// epoch; the clean unmount must publish it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_file_read_then_clean_unmount_keeps_the_acked_bytes() {
    let _g = serial().await;
    run("f48_open", false, false).await;
}
