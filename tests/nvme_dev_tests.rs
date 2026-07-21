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
