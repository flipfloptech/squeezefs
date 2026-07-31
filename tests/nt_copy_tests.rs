//! Near-zero-copy campaign (2026-07-31) — contracts for the
//! **non-temporal DMA-destined copy core** (`squeezefs::nt_copy`).
//!
//! The copy census proved the two remaining hot-path payload copies are
//! both *load-bearing* (the ring sever is the §5.2 isolation boundary;
//! the kernel-path merge is what lets the FUSE-over-io_uring ent re-arm
//! and the write ACK detach from the DMA), so the campaign optimizes
//! their COST, not their existence: destinations that are next consumed
//! by device DMA — the `ActiveBlockBuf` merge and the placed-sever
//! assembly — may use non-temporal stores, halving the destination's
//! cache-line traffic (no RFO read of the destination) and keeping 4 MiB
//! streams from sweeping the LLC.
//!
//! Contracts:
//! 1. **Byte exactness at every alignment** — the NT body has scalar
//!    head/tail legs; every (dst offset, src offset, len) shape must be
//!    exact and must never touch a byte outside the destination window.
//! 2. **Policy is pure and pinned** — enabled by default at a 256 KiB
//!    floor, `SQUEEZEFS_NT_COPY=0` kills it, `SQUEEZEFS_NT_COPY_MIN`
//!    retunes the floor (measurement levers, decided by the census rig).
//! 3. **Engagement is observable** — `dma_copy` reports the path taken
//!    (the call sites feed `nt_copy_bytes`, the stats-inode engagement
//!    instrument: a census row claiming the NT lever is INVALID unless
//!    the counter accounts for the row's merge bytes).
//! 4. **Publication safety** — NT stores are weakly ordered; the core
//!    must fence (`sfence`) before returning so the existing
//!    lock/atomic publication edges (merge under `BLOCK_FLUSH_LOCKS`,
//!    placed-claim `end_write`) carry the bytes as usual.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use squeezefs::nt_copy;

/// Contract 1: exactness across head/tail alignment shapes, and no
/// out-of-window writes (guard bytes).
#[test]
fn dma_copy_exact_across_alignments_with_guard_bytes() {
    // Big enough to cross the policy floor so the NT body actually runs
    // (policy floor tested separately; here we force the NT leg).
    let lens = [
        0usize,
        1,
        15,
        16,
        17,
        31,
        32,
        33,
        63,
        64,
        65,
        4095,
        4096,
        4097,
        256 * 1024,
        256 * 1024 + 13,
        1024 * 1024,
    ];
    for &len in &lens {
        for dst_off in [0usize, 1, 7, 8, 15, 16, 31, 32, 33, 63] {
            let src: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let mut dst = vec![0xEEu8; dst_off + len + 64];
            let expect = dst.clone();
            let engaged = nt_copy::dma_copy_forced(&mut dst[dst_off..dst_off + len], &src);
            assert_eq!(
                &dst[dst_off..dst_off + len],
                &src[..],
                "window bytes exact (len {len}, dst_off {dst_off}, engaged {engaged})"
            );
            assert_eq!(
                &dst[..dst_off],
                &expect[..dst_off],
                "head guard untouched (len {len}, dst_off {dst_off})"
            );
            assert_eq!(
                &dst[dst_off + len..],
                &expect[dst_off + len..],
                "tail guard untouched (len {len}, dst_off {dst_off})"
            );
        }
    }
}

/// Contract 1 (raw form): the raw-pointer variant used by the placed
/// sever matches the slice semantics byte for byte.
#[test]
fn dma_copy_raw_matches_slice_semantics() {
    let len = 512 * 1024 + 21;
    let src: Vec<u8> = (0..len).map(|i| (i * 17 + 3) as u8).collect();
    let mut a = vec![0u8; len];
    let mut b = vec![0u8; len];
    nt_copy::dma_copy_forced(&mut a, &src);
    // SAFETY: disjoint, correctly sized buffers.
    unsafe { nt_copy::dma_copy_raw_forced(b.as_mut_ptr(), src.as_ptr(), len) };
    assert_eq!(a, b, "raw and slice variants must be byte-identical");
    assert_eq!(a, src);
}

/// Contract 2: the policy core is a pure function of its env inputs —
/// default ON at a 256 KiB floor; `0` kills; `SQUEEZEFS_NT_COPY_MIN`
/// retunes the floor.
#[test]
fn policy_is_pure_and_pinned() {
    let p = nt_copy::policy_from(None, None);
    assert!(
        p.enabled,
        "default posture is enabled (measured 2026-07-31)"
    );
    assert_eq!(p.min_bytes, 256 * 1024, "default floor is 256 KiB");

    let off = nt_copy::policy_from(Some("0"), None);
    assert!(!off.enabled, "SQUEEZEFS_NT_COPY=0 is the kill switch");

    let tuned = nt_copy::policy_from(Some("1"), Some("65536"));
    assert!(tuned.enabled);
    assert_eq!(tuned.min_bytes, 65536, "explicit floor wins verbatim");

    let garbage = nt_copy::policy_from(Some("1"), Some("not-a-number"));
    assert_eq!(
        garbage.min_bytes,
        256 * 1024,
        "unparseable floor falls back to the default, never panics"
    );
}

/// Contract 3: `dma_copy` (the policy-gated entry the call sites use)
/// reports engagement truthfully — big copies engage on x86_64 under the
/// default policy, sub-floor copies never do.
#[test]
fn dma_copy_reports_engagement_by_policy() {
    let src_big = vec![0xABu8; 1024 * 1024];
    let mut dst_big = vec![0u8; 1024 * 1024];
    let engaged_big = nt_copy::dma_copy(&mut dst_big, &src_big);
    #[cfg(target_arch = "x86_64")]
    assert!(
        engaged_big,
        "a 1 MiB copy under default policy must take the NT path on x86_64"
    );
    assert_eq!(dst_big, src_big);

    let src_small = vec![0xCDu8; 4096];
    let mut dst_small = vec![0u8; 4096];
    let engaged_small = nt_copy::dma_copy(&mut dst_small, &src_small);
    assert!(
        !engaged_small,
        "a sub-floor copy must ride the plain cached copy"
    );
    assert_eq!(dst_small, src_small);
}

/// Contract 3 (stats surface): the engagement counter exists and its
/// increments are immediately visible (the stats-row exactness law from
/// `tests/metrics_counter_tests.rs`).
#[test]
fn nt_copy_bytes_counter_is_immediately_visible() {
    use squeezefs::fuse_client::METRICS;
    let before = METRICS.nt_copy_bytes.load(Ordering::Relaxed);
    METRICS.nt_copy_bytes.fetch_add(7, Ordering::Relaxed);
    assert_eq!(
        METRICS.nt_copy_bytes.load(Ordering::Relaxed) - before,
        7,
        "nt_copy_bytes must be an exact, immediately-visible counter"
    );
}

/// Contract 4: cross-thread publication smoke — bytes stored by the NT
/// body must be visible to another thread through an ordinary
/// release/acquire edge (the core's trailing sfence is what makes the
/// existing publication edges sufficient).
#[test]
fn nt_copy_publication_edge_smoke() {
    for round in 0..20u8 {
        let len = 1024 * 1024;
        let src: Arc<Vec<u8>> = Arc::new((0..len).map(|i| (i as u8) ^ round).collect());
        let dst = Arc::new(std::sync::Mutex::new(vec![0u8; len]));
        let ready = Arc::new(AtomicBool::new(false));

        let w_src = Arc::clone(&src);
        let w_dst = Arc::clone(&dst);
        let w_ready = Arc::clone(&ready);
        let writer = std::thread::spawn(move || {
            let mut guard = w_dst.lock().unwrap();
            nt_copy::dma_copy_forced(&mut guard, &w_src);
            drop(guard);
            w_ready.store(true, Ordering::Release);
        });
        while !ready.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        let guard = dst.lock().unwrap();
        assert_eq!(&*guard, &*src, "round {round}: published bytes exact");
        drop(guard);
        writer.join().unwrap();
    }
}
