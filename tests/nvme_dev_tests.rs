use std::io::{Seek, SeekFrom, Read};
use std::fs::File;
use tempfile::NamedTempFile;
use squeezefs::nvme_dev::NvmeBlockDev;

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
    
    assert_eq!(buf, data, "Data written to block device should match exactly");
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
    
    let read_data = read_result.unwrap();
    assert_eq!(read_data.len(), data.len(), "Read data length should match");
    assert_eq!(read_data, data, "Data read from block device should match exactly");
}
