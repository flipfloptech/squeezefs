use squeezefs::cache::active_block::ActiveBlockBuf;
use squeezefs::cache::pool::{BUFFER_POOL, POOLED_BUF_ALIGN};
use squeezefs::fuse_client::METRICS;
use squeezefs::nvme_dev::NvmeBlockDev;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use tempfile::NamedTempFile;

/// `nvme_unaligned_write_fallbacks` is process-global, so tests that submit
/// writes (all of which may move the counter) serialize against the tests
/// that assert counter deltas. Keeps every test exact under the default
/// parallel test runner, not just under `--test-threads=1`.
static WRITE_SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    WRITE_SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn unaligned_fallbacks() -> u64 {
    METRICS
        .nvme_unaligned_write_fallbacks
        .load(Ordering::Relaxed)
}

#[tokio::test]
async fn test_write_block_to_offset() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Ensure file is at least 8MB so we can write to 4MB offset
    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let writer = NvmeBlockDev::new(path.to_str().unwrap());

    let data = vec![0xAB; 4 * 1024 * 1024]; // 4MB of 0xAB
    let offset = 4 * 1024 * 1024; // 4MB offset

    let result = writer
        .write_block(offset, bytes::Bytes::from(data.clone()))
        .await;
    assert!(result.is_ok(), "Writing block should succeed");

    // Verify the data was written exactly at the offset
    let mut check_file = File::open(&path).unwrap();
    check_file.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    check_file.read_exact(&mut buf).unwrap();

    assert_eq!(
        buf, data,
        "Data written to block device should match exactly"
    );
}

#[tokio::test]
async fn test_read_block_from_offset() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Ensure file is at least 8MB so we can read from 4MB offset
    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let writer = NvmeBlockDev::new(path.to_str().unwrap());

    let data = vec![0xCD; 4 * 1024 * 1024]; // 4MB of 0xCD
    let offset = 4 * 1024 * 1024; // 4MB offset

    // Write it
    let result = writer
        .write_block(offset, bytes::Bytes::from(data.clone()))
        .await;
    assert!(result.is_ok(), "Writing block should succeed");

    // Read it back
    let read_result = writer.read_block(offset, 4 * 1024 * 1024).await;
    assert!(read_result.is_ok(), "Reading block should succeed");

    let read_data: bytes::Bytes = read_result.unwrap();
    assert_eq!(read_data.len(), data.len(), "Read data length should match");
    assert_eq!(
        read_data.as_ref(),
        data.as_slice(),
        "Data read from block device should match exactly"
    );
}

#[tokio::test]
async fn test_concurrent_stress() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Ensure file is large enough for concurrent writes/reads
    let file = File::create(&path).unwrap();
    file.set_len(16 * 1024 * 1024).unwrap(); // 16MB

    let writer = std::sync::Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));

    let mut handles = vec![];
    let concurrency = 256; // Make sure this is larger than physical cores to cause collision

    for i in 0..concurrency {
        let writer_clone = writer.clone();
        let handle = tokio::spawn(async move {
            let offset = (i % 4) * 4 * 1024 * 1024; // 4MB blocks
            let data = vec![i as u8; 4 * 1024 * 1024];
            writer_clone
                .write_block(offset as u64, bytes::Bytes::from(data))
                .await
                .unwrap();
            let read_back = writer_clone
                .read_block(offset as u64, 4 * 1024 * 1024)
                .await
                .unwrap();
            assert_eq!(read_back.len(), 4 * 1024 * 1024);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

/// Zero-copy write-path design §5.6 (PR 2): pooled write sources are
/// contractually 4 KiB-aligned, so a full-block pooled payload must take
/// `write_block`'s zero-copy `WriteData::Aligned` DMA branch — observable as
/// the `nvme_unaligned_write_fallbacks` counter *not* moving. Covers both
/// pooled provenances: a `BUFFER_POOL` `PooledBuf::into_bytes` payload (the
/// striped router write source) and an `ALIGNED_BUF_POOL`-backed
/// `ActiveBlockBuf::snapshot` (the active-block flush/upload source).
#[tokio::test]
async fn test_pooled_write_sources_take_aligned_dma_branch() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    file.set_len(16 * 1024 * 1024).unwrap();

    let dev = NvmeBlockDev::new(path.to_str().unwrap());
    let block = 4 * 1024 * 1024usize;

    // (a) BUFFER_POOL source (DataRouter striped write shape).
    let mut pooled = BUFFER_POOL.alloc();
    pooled.resize(block, 0);
    for (i, b) in pooled.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let payload = pooled.into_bytes();
    assert_eq!(
        payload.as_ptr() as usize % POOLED_BUF_ALIGN,
        0,
        "pooled payload pointer must honor the alignment contract"
    );

    let before = unaligned_fallbacks();
    dev.write_block(0, payload.clone())
        .await
        .expect("pooled write");
    assert_eq!(
        unaligned_fallbacks(),
        before,
        "a full-block BUFFER_POOL payload must take the aligned DMA branch, \
         not the bounce-copy fallback"
    );
    let read_back = dev.read_block(0, block).await.expect("read back");
    assert_eq!(
        read_back.as_ref(),
        payload.as_ref(),
        "aligned-branch write must round-trip byte-exact"
    );

    // (b) ActiveBlockBuf snapshot source (active-block flush/upload shape).
    let mut abb = ActiveBlockBuf::fresh(block);
    abb.record_write(0, block); // one-shot full coverage (PR 4 memset elision)
    for (i, b) in abb.make_mut().iter_mut().enumerate() {
        *b = (i % 239) as u8;
    }
    let snap = abb.snapshot();
    assert_eq!(
        snap.as_ptr() as usize % POOLED_BUF_ALIGN,
        0,
        "active-block snapshot must honor the alignment contract"
    );

    let before = unaligned_fallbacks();
    dev.write_block(block as u64, snap.clone())
        .await
        .expect("snapshot write");
    assert_eq!(
        unaligned_fallbacks(),
        before,
        "an ActiveBlockBuf snapshot must take the aligned DMA branch"
    );
    let read_back = dev
        .read_block(block as u64, block)
        .await
        .expect("read back");
    assert_eq!(
        read_back.as_ref(),
        snap.as_ref(),
        "aligned-branch snapshot write must round-trip byte-exact"
    );
}

/// The fallback detector itself: a payload that can never take the DMA
/// branch (non-4 KiB-multiple length) must be counted as exactly one
/// fallback — and still complete correctly via the bounce copy (the
/// fallback stays never-lossy, just observable).
#[tokio::test]
async fn test_unaligned_length_write_counts_fallback() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let dev = NvmeBlockDev::new(path.to_str().unwrap());
    let data: Vec<u8> = (0u8..123).collect();

    let before = unaligned_fallbacks();
    dev.write_block(4096, bytes::Bytes::from(data.clone()))
        .await
        .expect("unaligned-length write should succeed via the bounce copy");
    assert_eq!(
        unaligned_fallbacks(),
        before + 1,
        "a non-4KiB-multiple write must be counted as an unaligned fallback"
    );

    let read_back = dev
        .read_block(4096, data.len())
        .await
        .expect("read after unaligned-length write");
    assert_eq!(read_back.as_ref(), data.as_slice());
}

/// Exercise the unaligned write path (posix_memalign + copy in nvme_dev) to prevent
/// regressions in buffer management / leaks on that branch.
#[tokio::test]
async fn test_write_unaligned_size() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let writer = NvmeBlockDev::new(path.to_str().unwrap());

    // Small size that is unlikely to be 4k-aligned in memory from vec, plus non-multiple.
    let data: Vec<u8> = (0u8..123).collect();
    let offset = 4096u64; // within first block but offset aligned for device

    writer
        .write_block(offset, bytes::Bytes::from(data.clone()))
        .await
        .expect("unaligned write should succeed");

    // Read back and compare (read path uses aligned pool)
    let read_back = writer
        .read_block(offset, data.len())
        .await
        .expect("read after unaligned write");
    assert_eq!(read_back.as_ref(), data.as_slice());
}

/// P0-1: Dropping the device while unaligned writes are in flight must join the
/// uring worker cleanly (no hang) and free unaligned buffers on exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_drop_device_during_unaligned_writes_joins_cleanly() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    file.set_len(16 * 1024 * 1024).unwrap();

    let writer = Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    let mut handles = Vec::new();
    for i in 0..64 {
        let w = writer.clone();
        handles.push(tokio::spawn(async move {
            // Force unaligned path (size not multiple of 4k, heap ptr rarely 4k-aligned).
            let data: Vec<u8> = (0u8..200).map(|x| x.wrapping_add(i as u8)).collect();
            let offset = ((i % 8) * 4096) as u64;
            w.write_block(offset, bytes::Bytes::from(data)).await
        }));
    }

    // Drop our Arc while tasks may still be in flight; last Arc drop joins worker.
    drop(writer);

    let start = Instant::now();
    for h in handles {
        // Results may be Ok or Err (shutdown); must not hang.
        let _ = h.await.expect("task join");
    }
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "worker join under concurrent unaligned writes must complete promptly"
    );
}

/// P0-1: Explicit drop after a burst of unaligned writes must not hang on join.
#[tokio::test]
async fn test_drop_after_unaligned_burst_does_not_hang() {
    use std::time::{Duration, Instant};

    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();

    let writer = NvmeBlockDev::new(path.to_str().unwrap());
    for i in 0..32 {
        let data: Vec<u8> = (0u8..177).map(|x| x.wrapping_add(i as u8)).collect();
        writer
            .write_block((i % 4) * 4096, bytes::Bytes::from(data))
            .await
            .expect("unaligned write in burst");
    }

    let start = Instant::now();
    drop(writer);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "Drop/join of uring worker after unaligned burst must be prompt"
    );
}

#[tokio::test]
async fn test_read_block_exceeds_pool_size() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    file.set_len(16 * 1024 * 1024).unwrap();

    let dev = NvmeBlockDev::new(path.to_str().unwrap());

    // Attempt to read 5MB which is greater than pool size (4MB)
    let too_large = 5 * 1024 * 1024;
    let result = dev.read_block(0, too_large).await;
    assert!(result.is_err());
    let err_str = result.err().unwrap().to_string();
    assert!(err_str.contains("exceeds pool buffer size"));
}

/// VL8 item 5 — a read entirely past EOF on a file-backed substrate returns
/// 0 bytes from the kernel; the completion path must NOT report success with
/// recycled pool-buffer bytes. Real block devices are all-or-EIO; the short
/// read is the file-backed-substrate silent-garbage class.
#[tokio::test]
async fn test_read_past_eof_errors_instead_of_recycled_garbage() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    let file_len: u64 = 1024 * 1024; // 1 MiB backing file
    file.set_len(file_len).unwrap();

    let dev = NvmeBlockDev::new(path.to_str().unwrap());

    // Prime a pool buffer with a recognizable pattern via a real
    // write+read cycle so a recycled buffer would carry 0xEE garbage.
    let pattern = vec![0xEEu8; 4096];
    dev.write_block(0, bytes::Bytes::from(pattern.clone()))
        .await
        .expect("in-range write must succeed");
    let read_back = dev
        .read_block(0, 4096)
        .await
        .expect("in-range read must succeed");
    assert_eq!(read_back.as_ref(), pattern.as_slice());

    // Read entirely past EOF: kernel returns 0 bytes. This must be a loud
    // error, never Ok(recycled bytes).
    let res = dev.read_block(file_len, 4096).await;
    match res {
        Err(_) => {} // correct: loud failure
        Ok(bytes) => panic!(
            "past-EOF read reported success with {} bytes (first byte {:#x}) — silent garbage",
            bytes.len(),
            bytes.first().copied().unwrap_or(0)
        ),
    }
}

/// VL8 item 5 — a read straddling EOF returns a SHORT count from the kernel;
/// success must not be reported for the full requested size (the tail would
/// be recycled pool-buffer bytes).
#[tokio::test]
async fn test_read_straddling_eof_errors_instead_of_garbage_tail() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let file = File::create(&path).unwrap();
    let file_len: u64 = 1024 * 1024;
    file.set_len(file_len).unwrap();

    let dev = NvmeBlockDev::new(path.to_str().unwrap());

    // Prime pool recycling with a distinct garbage pattern. (Note: writes
    // must stay clear of the EOF window — the unaligned bounce path pads
    // writes to 4 KiB, which would legitimately extend the backing file.)
    let garbage = vec![0xEEu8; 4096];
    dev.write_block(0, bytes::Bytes::from(garbage))
        .await
        .expect("in-range write must succeed");
    let _ = dev.read_block(0, 4096).await.expect("prime read");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        file_len,
        "backing file must not have grown — the straddle premise"
    );

    // 4 KiB read whose second half is past EOF ⇒ kernel returns 2048.
    let res = dev.read_block(file_len - 2048, 4096).await;
    match res {
        Err(_) => {} // correct: exact-length contract, short ⇒ loud error
        Ok(bytes) => panic!(
            "straddling-EOF read reported success with {} bytes; tail byte {:#x} — garbage tail",
            bytes.len(),
            bytes.last().copied().unwrap_or(0)
        ),
    }
}

// ---------------------------------------------------------------------------
// 2026-07-25 ipc-miss-path fix: size-classed read-bounce pools. A sub-block
// device read must never check out (or fresh-allocate) a whole-block
// backing — that was the ring-read miss path's 6 ms/op convoy (4 MiB
// THP-zeroing fault + munmap TLB storm per over-capacity op) — and every
// pooled backing must recycle into its HOME pool (a cross-pool recycle
// would later hand a 64 KiB backing out as a 4 MiB one: heap overflow).
// ---------------------------------------------------------------------------

#[test]
fn read_bounce_pool_routing_and_home_recycle() {
    use squeezefs::cache::pool::{
        read_bounce_pool, ALIGNED_BUF_POOL, RANGED_BUF_POOL, RANGED_BUF_SIZE,
    };

    // The gauges below are process-global: serialize against every test
    // that checks out pool buffers (pre-existing parallel-observation
    // flake — a concurrent test's in-flight read bounce holds a pool
    // slot while this test samples `allocated_bytes`; exact under
    // `--test-threads=1`, ~40 % failed under the parallel runner even
    // with the 2026-08-07 write-lane tests skipped).
    let _serial = WRITE_SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .blocking_lock();

    // Routing: sub-block windows ride the small pool; whole-block stays big.
    assert!(std::sync::Arc::ptr_eq(
        read_bounce_pool(4096),
        &RANGED_BUF_POOL
    ));
    assert!(std::sync::Arc::ptr_eq(
        read_bounce_pool(RANGED_BUF_SIZE),
        &RANGED_BUF_POOL
    ));
    assert!(std::sync::Arc::ptr_eq(
        read_bounce_pool(RANGED_BUF_SIZE + 1),
        &ALIGNED_BUF_POOL
    ));
    assert!(std::sync::Arc::ptr_eq(
        read_bounce_pool(4 * 1024 * 1024),
        &ALIGNED_BUF_POOL
    ));
    assert_eq!(RANGED_BUF_POOL.buf_size(), RANGED_BUF_SIZE);

    // Home recycle: alloc from EACH pool, drop the Bytes owner, and the
    // idle byte count of BOTH pools must return exactly to its prior value
    // (small never leaks into big, big never leaks into small).
    let small_before = RANGED_BUF_POOL.allocated_bytes();
    let big_before = ALIGNED_BUF_POOL.allocated_bytes();
    let (_p1, small_bytes) = RANGED_BUF_POOL.alloc();
    let (_p2, big_bytes) = ALIGNED_BUF_POOL.alloc();
    assert_eq!(
        RANGED_BUF_POOL.allocated_bytes(),
        small_before - RANGED_BUF_SIZE as u64,
        "small alloc checks out of the small pool"
    );
    assert_eq!(
        ALIGNED_BUF_POOL.allocated_bytes(),
        big_before - ALIGNED_BUF_POOL.buf_size() as u64,
        "big alloc checks out of the big pool"
    );
    drop(small_bytes);
    drop(big_bytes);
    assert_eq!(
        RANGED_BUF_POOL.allocated_bytes(),
        small_before,
        "small backing must recycle into its HOME pool"
    );
    assert_eq!(
        ALIGNED_BUF_POOL.allocated_bytes(),
        big_before,
        "big backing must recycle into its HOME pool"
    );
}

// ---------------------------------------------------------------------------
// I/O-lane fan-out. Reads (2026-08-06 read-queue-wall campaign): ONE
// submitting thread per device = ONE blk-mq software queue = ONE nvme-tcp
// connection = ONE RX softirq core per device — the ~2.7 GB/s/core
// read-copy wall the flat field D-ladder rode (~22 GB/s over 8 namespaces
// at ANY pinned depth; local written-region discriminator: 1 submitter/dev
// qd16 = 11.5 GB/s, qd32 = 12.1 — depth through one queue buys ~nothing —
// while 4 submitters/dev at the SAME in-flight = 16.7, and 8 = 17.7).
// Writes (2026-08-07 write-lane-fanout campaign): the read campaign's
// "writes escape it" held only up to the TX splice's higher per-connection
// ceiling — the field decomposition walls kernel seq writes at 35.31 GB/s
// = 7.06 GB/s × 5 data namespaces (`write_pipeline_phase_ns` dma mode
// 2–8 ms, interior µs-clean; depth probes 20 up = 20 backoffs — depth into
// a per-connection wall converts nothing) while raw fio reaches 49.7 GB/s
// on the SAME namespaces by spreading queues. So DATA WRITES fan out too,
// with LANE AFFINITY BY BLOCK OFFSET (same block → same lane → same
// submission FIFO → same connection: exactly the per-offset submission
// order the single worker provided), and the DUR-2 barrier drains EVERY
// lane's write watermark before the device flush issues. The lane count
// DERIVES: cpus / data devices, floor 1, self-bounded by cpus (never a
// constant); explicit SQUEEZEFS_NVME_READ_LANES /
// SQUEEZEFS_NVME_WRITE_LANES win verbatim (1 = the pre-change posture,
// the A/B lever).
// ---------------------------------------------------------------------------

#[test]
fn io_lanes_derivation_table() {
    use squeezefs::nvme_dev::io_lanes_for;
    // Exact divisions are unchanged by the ceiling (2026-08-14 field
    // re-derivation — the user ruling: a fractional share rounds UP, a
    // submission lane is cheap and the per-connection wall is not).
    // The read field shape: 32 CPUs / 8 data namespaces => 4.
    assert_eq!(io_lanes_for(32, 8), 4);
    // Fractional shares round UP now: 32 / 5 = 6.4 => 7 (was 6 — the
    // 2026-08-14 field mount derived 3 of a fractional 4-ish share and
    // sat a lane short of its wall).
    assert_eq!(io_lanes_for(32, 5), 7);
    // The local devsub: 32 / 4 => 8.
    assert_eq!(io_lanes_for(32, 4), 8);
    // The field conviction shape: a ~3.x share must derive 4, not 3.
    assert_eq!(io_lanes_for(30, 8), 4);
    // Self-bounding: one device may use every CPU (the raw row's shape);
    // more devices than CPUs still derives 1 whole lane (ceil of a
    // fraction below 1 is 1 — never zero, never more than needed).
    assert_eq!(io_lanes_for(32, 1), 32);
    assert_eq!(io_lanes_for(4, 8), 1);
    assert_eq!(io_lanes_for(0, 0), 1, "degenerate inputs floor at 1");
}

// ---------------------------------------------------------------------------
// Write-lane affinity: the lane pick is a PURE function of the block
// offset — `(offset / grain) % lanes` — so two DMAs naming the same
// device offset (supersession / in-place / W1 patch shapes) ride the
// same lane, the same channel FIFO and the same connection, preserving
// the per-offset submission order the single worker gave structurally.
// ---------------------------------------------------------------------------

#[test]
fn write_lane_affinity_is_pure_and_block_stable() {
    use squeezefs::nvme_dev::write_lane_index;
    let grain = 4 * 1024 * 1024u64; // the shipped block size
    let lanes = 6; // the 32-cpu / 5-namespace field derivation

    // Same offset → same lane, always (per-offset ordering).
    for off in [0u64, 4096, grain - 4096, grain, 7 * grain + 8192] {
        assert_eq!(
            write_lane_index(off, grain, lanes),
            write_lane_index(off, grain, lanes),
            "the pick must be pure"
        );
    }
    // Same BLOCK → same lane: a whole-block write at the block start and
    // a W1 sub-block patch inside it must not reorder across lanes.
    for block in 0..64u64 {
        let base = write_lane_index(block * grain, grain, lanes);
        for rel in [0u64, 4096, 512 * 1024, grain - 4096] {
            assert_eq!(
                write_lane_index(block * grain + rel, grain, lanes),
                base,
                "block {block} rel {rel} must stay on its block's lane"
            );
        }
    }
    // Consecutive blocks SPREAD: the whole point of the fan-out.
    let mut seen = std::collections::HashSet::new();
    for block in 0..lanes as u64 {
        seen.insert(write_lane_index(block * grain, grain, lanes));
    }
    assert_eq!(seen.len(), lanes, "streaming blocks must cover every lane");
    // Degenerate inputs never panic and land on lane 0.
    assert_eq!(write_lane_index(123, 0, 4), write_lane_index(123, 1, 4));
    assert_eq!(write_lane_index(u64::MAX, grain, 1), 0);
    assert_eq!(write_lane_index(42, grain, 0), 0);
}

#[tokio::test]
async fn write_lane_pool_spreads_by_offset_and_stays_byte_exact() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    File::create(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();

    let dev = std::sync::Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    assert_eq!(
        dev.write_lanes(),
        1,
        "default = the pre-change single-worker posture"
    );

    let grain = 1024 * 1024u64;
    dev.set_write_lanes(4, grain);
    assert_eq!(dev.write_lanes(), 4);
    dev.set_write_lanes(2, grain);
    assert_eq!(dev.write_lanes(), 4, "lane count only grows (monotone)");

    // 8 block writes at 1 MiB stride: affinity says block b rides lane
    // b % 4, so each lane's submit counter moves by exactly 2.
    let before: Vec<u64> = dev
        .write_lane_watermarks()
        .iter()
        .map(|(s, _)| *s)
        .collect();
    for b in 0..8u64 {
        let data = vec![b as u8 + 1; grain as usize];
        dev.write_block(b * grain, bytes::Bytes::from(data))
            .await
            .unwrap();
    }
    let after: Vec<u64> = dev
        .write_lane_watermarks()
        .iter()
        .map(|(s, _)| *s)
        .collect();
    for (lane, (a, b)) in after.iter().zip(before.iter()).enumerate() {
        assert_eq!(
            a - b,
            2,
            "lane {lane} must carry exactly its residue class (blocks {lane} and {})",
            lane + 4
        );
    }

    // Two more writes to the SAME block land on ONE lane (affinity, not
    // round-robin): only block 5's lane (5 % 4 = 1) moves.
    let before: Vec<u64> = dev
        .write_lane_watermarks()
        .iter()
        .map(|(s, _)| *s)
        .collect();
    for _ in 0..2 {
        dev.write_block(5 * grain, bytes::Bytes::from(vec![0xEEu8; grain as usize]))
            .await
            .unwrap();
    }
    let after: Vec<u64> = dev
        .write_lane_watermarks()
        .iter()
        .map(|(s, _)| *s)
        .collect();
    for (lane, (a, b)) in after.iter().zip(before.iter()).enumerate() {
        let delta = a - b;
        if lane == 1 {
            assert_eq!(delta, 2, "same block → same lane, every time");
        } else {
            assert_eq!(delta, 0, "no other lane may carry block 5");
        }
    }

    // Byte parity across the whole span: every lane is a full citizen of
    // the write contract.
    dev.flush().await.unwrap();
    for b in 0..8u64 {
        let expect = if b == 5 { 0xEEu8 } else { b as u8 + 1 };
        let got = dev.read_block(b * grain, grain as usize).await.unwrap();
        assert!(got.iter().all(|&x| x == expect), "byte parity block {b}");
    }
    drop(dev);
}

// ---------------------------------------------------------------------------
// DUR-2 flush ordering across lanes — THE red contract of the write
// fan-out: a flush must not complete before a write submitted to a
// DIFFERENT lane has completed at the device. The drain is a per-lane
// completion watermark captured at barrier start; without it the Fsync
// on lane 0 races a stalled write on lane 3 and "everything written
// before the barrier started" silently stops being true.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_completes_no_earlier_than_writes_on_every_lane() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    File::create(&path)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();

    let dev = std::sync::Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    let grain = 1024 * 1024u64;
    dev.set_write_lanes(4, grain);

    // The worker-side stall stands in for a slow fabric DMA on lane 3
    // (the deterministic device-order seam — the set_test_read_stall
    // precedent): the write is SUBMITTED (watermark moves) but does not
    // complete for 800 ms.
    squeezefs::nvme_dev::set_test_write_stall(1, 800);

    let payload = bytes::Bytes::from(vec![0xABu8; grain as usize]);
    let write_dev = dev.clone();
    let flush_dev = dev.clone();
    // join! polls in order: the write future's FIRST poll runs its
    // submission synchronously (watermark bumped, request in lane 3's
    // channel) before the flush future is ever polled — "submitted
    // before the barrier started" holds by construction, no sleeps.
    let (write_res, marks_at_flush_return) = tokio::join!(
        async move { write_dev.write_block(3 * grain, payload).await },
        async move {
            flush_dev.flush().await.expect("flush must succeed");
            flush_dev.write_lane_watermarks()
        }
    );
    write_res.expect("the stalled write must land");

    let (submitted, completed) = marks_at_flush_return[3];
    assert_eq!(submitted, 1, "the write rode lane 3 (offset affinity)");
    assert_eq!(
        completed, submitted,
        "DUR-2: the barrier returned while a write submitted before it \
         started was still in flight on another lane — the flush covered \
         nothing for that block"
    );

    // And the barrier really did cover it: the bytes are on the device.
    let got = dev.read_block(3 * grain, grain as usize).await.unwrap();
    assert!(got.iter().all(|&x| x == 0xAB), "post-barrier byte parity");
    squeezefs::nvme_dev::set_test_write_stall(0, 0);
    drop(dev);
}

#[tokio::test]
async fn read_lane_pool_serves_exact_bytes_and_leaves_writes_on_lane_zero() {
    let _serial = serial().await;
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    File::create(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();

    let dev = std::sync::Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    assert_eq!(
        dev.read_lanes(),
        1,
        "default = the prior single-worker posture"
    );

    // Arm 4 read lanes (monotone: shrinking is refused silently).
    dev.set_read_lanes(4);
    assert_eq!(dev.read_lanes(), 4);
    dev.set_read_lanes(2);
    assert_eq!(dev.read_lanes(), 4, "lane count only grows");

    // Writes (worker 0) then concurrent reads round-robined across the
    // pool: byte-exact at every offset, every lane a full citizen of the
    // exact-length contract.
    for b in 0..8u64 {
        let data = vec![b as u8 + 1; 1024 * 1024];
        dev.write_block(b * 1024 * 1024, bytes::Bytes::from(data))
            .await
            .unwrap();
    }
    dev.flush().await.unwrap();
    let mut tasks = Vec::new();
    for round in 0..4u64 {
        for b in 0..8u64 {
            let d = dev.clone();
            tasks.push(tokio::spawn(async move {
                let got = d
                    .read_block_with_dest(b * 1024 * 1024, 1024 * 1024, None)
                    .await
                    .unwrap();
                assert_eq!(got.len(), 1024 * 1024, "exact-length contract");
                assert!(
                    got.iter().all(|&x| x == b as u8 + 1),
                    "byte parity lane round {round} block {b}"
                );
            }));
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
    drop(dev);
}
