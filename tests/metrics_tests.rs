//! P3-1: metrics counters are lock-free and visible.

use squeezefs::fuse_client::METRICS;
use std::sync::atomic::Ordering;

#[test]
fn test_layout_and_admission_metrics_increment() {
    let before_inline = METRICS.layout_inline_writes.load(Ordering::Relaxed);
    METRICS.layout_inline_writes.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS.layout_inline_writes.load(Ordering::Relaxed),
        before_inline + 1
    );

    let before_adm = METRICS.bg_spawn_admitted.load(Ordering::Relaxed);
    METRICS.bg_spawn_admitted.fetch_add(3, Ordering::Relaxed);
    assert_eq!(
        METRICS.bg_spawn_admitted.load(Ordering::Relaxed),
        before_adm + 3
    );

    let before_full = METRICS.uring_queue_full.load(Ordering::Relaxed);
    METRICS.uring_queue_full.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS.uring_queue_full.load(Ordering::Relaxed),
        before_full + 1
    );

    // PR 2 (zero-copy write-path §5.6): the pooled-buffer alignment-contract
    // violation detector must be a live, lock-free counter.
    let before_fallbacks = METRICS
        .nvme_unaligned_write_fallbacks
        .load(Ordering::Relaxed);
    METRICS
        .nvme_unaligned_write_fallbacks
        .fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        METRICS
            .nvme_unaligned_write_fallbacks
            .load(Ordering::Relaxed),
        before_fallbacks + 1
    );
}

#[test]
fn test_lease_metrics_fields_exist() {
    // Smoke: fields are readable (no panics / alignment issues).
    let _ = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let _ = METRICS.lease_acquire_fail.load(Ordering::Relaxed);
    let _ = METRICS.writeback_hard_failures.load(Ordering::Relaxed);
    let _ = METRICS.layout_staged_writes.load(Ordering::Relaxed);
    let _ = METRICS.layout_striped_writes.load(Ordering::Relaxed);
}
