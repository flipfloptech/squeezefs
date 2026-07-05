use squeezefs::nvme_dev::NvmeBlockDev;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_write_block_to_offset() {
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

/// Exercise the unaligned write path (posix_memalign + copy in nvme_dev) to prevent
/// regressions in buffer management / leaks on that branch.
#[tokio::test]
async fn test_write_unaligned_size() {
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
