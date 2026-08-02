//! Pooled-buffer contract tests (zero-copy write-path design §5.6, PR 2).
//!
//! `BufferPool` / `PooledBuf` back the striped RMW / read-assembly paths and
//! feed `nvme_dev::write_block` via `into_bytes`. This file pins the pool
//! contract the DMA path relies on:
//!
//! 1. **Alignment is contractual, not allocator luck**: every handout —
//!    fresh, recycled, beyond-capacity, or grown past the pooled backing —
//!    is `POOLED_BUF_ALIGN` (4 KiB)-aligned, so a pooled full-block payload
//!    always qualifies for `WriteData::Aligned` zero-copy DMA.
//! 2. **Recycled-buffer hygiene**: buffers are handed out logically empty
//!    (`len() == 0`) and `resize` fills every newly exposed byte, so
//!    recycled pool memory can never leak a previous user's content through
//!    hole reads or short-read tails.
//! 3. **Recycling**: pool-sized backings return to the pool on drop (also
//!    via the `into_bytes` owner); grown backings are freed instead of
//!    poisoning the pool.

use squeezefs::cache::pool::{AlignedBufPool, BufferPool, POOLED_BUF_ALIGN};
use std::sync::Arc;

fn is_aligned(ptr: *const u8) -> bool {
    (ptr as usize).is_multiple_of(POOLED_BUF_ALIGN)
}

#[test]
fn test_pool_alloc_and_reclamation() {
    let pool = Arc::new(BufferPool::new(10, 4 * 1024 * 1024));
    let initial_len = pool.len();
    assert_eq!(
        initial_len, 10,
        "Pool should start with pre-allocated buffers"
    );

    {
        let mut buf = pool.alloc();
        assert_eq!(
            pool.len(),
            9,
            "Allocating a buffer should decrease pool count"
        );
        assert_eq!(
            buf.len(),
            0,
            "Pooled buffers must be handed out logically empty"
        );
        assert_eq!(
            buf.capacity(),
            4 * 1024 * 1024,
            "Backing capacity should default to the pool buffer size"
        );
        assert!(
            is_aligned(buf.as_ptr()),
            "pooled buffer handout must honor the 4 KiB alignment contract"
        );

        let before = buf.as_ptr();
        buf.resize(4 * 1024 * 1024, 0);
        assert_eq!(buf.len(), 4 * 1024 * 1024, "resize sets the logical length");
        assert_eq!(
            buf.as_ptr(),
            before,
            "resize within capacity must not reallocate"
        );
        assert!(
            buf.iter().all(|&b| b == 0),
            "resize must zero-fill every newly exposed byte"
        );

        // Write some data
        buf[0] = 42;
        buf[100] = 99;
    }

    assert_eq!(
        pool.len(),
        10,
        "Dropping PooledBuf should return it to the pool"
    );
}

/// The hygiene contract on the exact buffer that was dirtied: with a
/// capacity-1 pool the recycled handout is provably the same backing the
/// previous user wrote through, and it still must present as empty and
/// zero-fill on resize (no cross-user content leak — the class the design's
/// Security section calls "recycled pool memory served through hole reads").
#[test]
fn test_recycled_buffer_never_leaks_prior_content() {
    let pool = Arc::new(BufferPool::new(1, 65536));

    {
        let mut buf = pool.alloc();
        buf.resize(65536, 0);
        buf[..].fill(0xAB);
    }
    assert_eq!(pool.len(), 1, "dirty buffer recycled");

    let mut buf = pool.alloc();
    assert_eq!(
        buf.len(),
        0,
        "recycled buffer must be handed out logically empty"
    );
    buf.resize(65536, 0);
    assert!(
        buf.iter().all(|&b| b == 0),
        "recycled pooled buffer leaked a previous user's content"
    );
}

/// Growing past the pooled backing (oversized-block configurations) must
/// keep the alignment contract, preserve content, zero-fill the extension,
/// return the pool-sized backing to the pool at grow time, and free (not
/// pool) the oversized backing on drop.
#[test]
fn test_grow_beyond_pool_capacity_stays_aligned_and_recycles() {
    let pool = Arc::new(BufferPool::new(2, 8192));

    let mut buf = pool.alloc();
    assert_eq!(pool.len(), 1);
    buf.resize(8192, 0);
    buf[..].fill(0x5A);

    buf.resize(8192 + 4096, 0);
    assert!(
        is_aligned(buf.as_ptr()),
        "grown buffer must keep the 4 KiB alignment contract"
    );
    assert_eq!(
        buf.capacity() % POOLED_BUF_ALIGN,
        0,
        "grown capacity rounded"
    );
    assert!(
        buf[..8192].iter().all(|&b| b == 0x5A),
        "grow must preserve the initialized prefix"
    );
    assert!(
        buf[8192..].iter().all(|&b| b == 0),
        "grow must zero-fill the extension"
    );
    assert_eq!(
        pool.len(),
        2,
        "the displaced pool-sized backing returns to the pool at grow time"
    );

    drop(buf);
    assert_eq!(
        pool.len(),
        2,
        "an oversized backing is freed on drop, never pushed into the pool"
    );

    let again = pool.alloc();
    assert_eq!(
        again.capacity(),
        8192,
        "the pool only ever hands out pool-sized backings"
    );
}

/// `into_bytes` exposes the pooled backing zero-copy: the `Bytes` pointer is
/// the aligned pool pointer (so a full-block pooled payload takes
/// `write_block`'s aligned DMA branch), the content is the logical prefix,
/// and dropping the `Bytes` recycles the backing.
#[test]
fn test_into_bytes_is_aligned_zero_copy_and_recycles() {
    let pool = Arc::new(BufferPool::new(1, 16384));

    let mut buf = pool.alloc();
    buf.resize(16384, 7);
    let backing_ptr = buf.as_ptr();

    let bytes = buf.into_bytes();
    assert_eq!(bytes.len(), 16384);
    assert_eq!(
        bytes.as_ptr(),
        backing_ptr,
        "into_bytes must expose the pooled backing zero-copy"
    );
    assert!(
        is_aligned(bytes.as_ptr()),
        "into_bytes payload must honor the 4 KiB alignment contract"
    );
    assert!(
        bytes.iter().all(|&b| b == 7),
        "resize fill value must be visible through the Bytes view"
    );
    assert_eq!(
        pool.len(),
        0,
        "backing is out of the pool while Bytes lives"
    );

    drop(bytes);
    assert_eq!(
        pool.len(),
        1,
        "dropping the Bytes must recycle the pooled backing"
    );
}

/// Alignment holds across recycle cycles and under pool exhaustion (fresh
/// over-capacity allocations must satisfy the same contract).
#[test]
fn test_alignment_contract_across_recycle_and_exhaustion() {
    let pool = Arc::new(BufferPool::new(2, 8192));

    for _ in 0..3 {
        let mut a = pool.alloc();
        a.resize(8192, 1);
        assert!(is_aligned(a.as_ptr()), "recycled handout must stay aligned");
    }

    let held: Vec<_> = (0..5).map(|_| pool.alloc()).collect();
    for b in &held {
        assert!(
            is_aligned(b.as_ptr()),
            "exhaustion-path (fresh) handout must honor the alignment contract"
        );
    }
}

/// `AlignedBufPool` (active blocks / reads / unaligned-write bounce buffers)
/// carries the same contract; pin it as tested rather than incidental:
/// `alloc_raw` handouts are aligned across fresh, recycled, and
/// exhaustion-path allocations.
#[test]
fn test_aligned_buf_pool_alloc_raw_contract() {
    let pool = Arc::new(AlignedBufPool::new(2, 8192));
    assert_eq!(pool.buf_size(), 8192);

    let a = pool.alloc_raw();
    let b = pool.alloc_raw();
    let c = pool.alloc_raw(); // pool empty -> fresh allocation
    for (name, p) in [("pooled a", a), ("pooled b", b), ("fresh c", c)] {
        assert!(
            !p.is_null() && is_aligned(p),
            "{name} handout must be 4 KiB-aligned"
        );
    }

    // SAFETY: a/b/c came from this pool's `alloc_raw` above and are recycled
    // exactly once, with no live references into the buffers.
    unsafe {
        pool.recycle(a);
        pool.recycle(b);
        pool.recycle(c); // queue full -> freed, must not poison the pool
    }

    let again = pool.alloc_raw();
    assert!(is_aligned(again), "recycled handout must stay aligned");
    // SAFETY: same contract as above — single recycle of this pool's handout.
    unsafe { pool.recycle(again) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pool_concurrent_stress() {
    let pool = Arc::new(BufferPool::new(20, 1024));
    let mut handles = vec![];
    for _ in 0..50 {
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..100 {
                let mut buf = pool_clone.alloc();
                buf.resize(1024, 0);
                assert!(
                    (buf.as_ptr() as usize).is_multiple_of(POOLED_BUF_ALIGN),
                    "alignment contract must hold under concurrency"
                );
                buf[0] = 1;
                tokio::task::yield_now().await;
                assert_eq!(buf[0], 1);
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(
        pool.len(),
        20,
        "All buffers should be returned to pool after concurrent tasks finish"
    );
}
