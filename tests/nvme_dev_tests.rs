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

    let result = writer.write_block(offset, &data).await;
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
    let result = writer.write_block(offset, &data).await;
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
                .write_block(offset as u64, &data)
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
