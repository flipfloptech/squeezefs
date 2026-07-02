//! P1 resource / backpressure smoke tests.
//! Prefer `--test-threads=1` when Garnet is involved.

use squeezefs::nvme_dev::NvmeBlockDev;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// P1-6: bounded uring queue still serves a concurrent burst without hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_uring_bounded_queue_handles_concurrent_burst() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let dev = Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    let mut handles = Vec::new();
    for i in 0..128 {
        let d = dev.clone();
        handles.push(tokio::spawn(async move {
            let data = vec![(i % 255) as u8; 4096];
            let offset = ((i % 16) * 4096) as u64;
            d.write_block(offset, &data).await
        }));
    }
    let mut ok = 0usize;
    let mut err = 0usize;
    for h in handles {
        match h.await.expect("join") {
            Ok(()) => ok += 1,
            Err(_) => err += 1,
        }
    }
    // Under normal load all should succeed; backpressure may reject some under extreme load.
    assert!(
        ok + err == 128 && ok > 0,
        "expected mixed or full success, ok={ok} err={err}"
    );
}

/// P1-6 smoke: sequential unaligned writes under bounded queue complete.
#[tokio::test]
async fn test_uring_sequential_unaligned_under_bound() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(8 * 1024 * 1024).unwrap();
    }
    let dev = NvmeBlockDev::new(path.to_str().unwrap());
    for i in 0..32u64 {
        let data: Vec<u8> = (0u8..123).map(|x| x.wrapping_add(i as u8)).collect();
        dev.write_block((i % 4) * 4096, &data)
            .await
            .expect("unaligned write under bounded queue");
    }
}
