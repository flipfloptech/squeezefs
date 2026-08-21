//! Zero-length device DMA contract (7.1-kernel-venue bring-up, finding A).
//!
//! A zero-length read is a legitimate degenerate ask (a zero-length
//! durable clip after truncate/extend cycles resolves to `size == 0`),
//! and it must be served EMPTY at the device door — never submitted as
//! an SQE. On the sqz 7.1.8 kernel a 0-len `ReadFixed` against the
//! registered read-bounce slab oopses the kernel
//! (`iov_iter_alignment_bvec` NULL deref via `io_read_fixed` →
//! `btrfs_direct_read`), and the oops path SIGKILLs the WORKER THREAD
//! only: the lane's request channel strands with its oneshots pinned in
//! limbo, every later write on the lane parks forever (the
//! `extend_vs_promotion_never_strands_payload` wedge) and every later
//! read times out ETIMEDOUT. Even on kernels that survive it, the
//! submission is a wasted syscall for zero bytes.
//!
//! The contract: `size == 0` returns `Ok(empty)` immediately — the
//! exact-length law (VL8 item 5) holds trivially — and no request ever
//! reaches the worker ring.

use squeezefs::nvme_dev::NvmeBlockDev;
use std::time::{Duration, Instant};

fn backing_file(len: u64) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().expect("backing temp file");
    f.as_file().set_len(len).expect("size backing file");
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_length_read_serves_empty_without_a_device_op() {
    let backing = backing_file(16 * 1024 * 1024);
    let dev = NvmeBlockDev::new(backing.path().to_str().unwrap());

    // Prime the lane with a real cycle so the zero-length ask hits a
    // LIVE worker (the crash shape: a healthy lane killed mid-stream).
    let payload = bytes::Bytes::from(vec![0xA5u8; 8192]);
    dev.write_block(4096, payload.clone())
        .await
        .expect("priming write");
    let got = dev.read_block(4096, 8192).await.expect("priming read");
    assert_eq!(got.as_ref(), payload.as_ref(), "priming round-trip");

    // The contract under test: a zero-length read at an allocated offset
    // serves empty immediately — never a device op, never a wedge.
    let t0 = Instant::now();
    let empty = dev
        .read_block(4096, 0)
        .await
        .expect("zero-length read must serve empty, not error");
    assert!(empty.is_empty(), "exact-length law: 0 requested, 0 served");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "zero-length read must be immediate (a device round-trip here \
         means the guard is gone; on the sqz 7.1 kernel it is fatal)"
    );

    // The lane must still be ALIVE after the zero-length ask (the bug
    // killed the worker thread: every later op stranded forever).
    let again = dev
        .read_block(4096, 8192)
        .await
        .expect("lane must survive a zero-length ask");
    assert_eq!(again.as_ref(), payload.as_ref(), "post-ask round-trip");
    dev.write_block(16384, bytes::Bytes::from(vec![0x5Au8; 4096]))
        .await
        .expect("post-ask write must not strand");
}
