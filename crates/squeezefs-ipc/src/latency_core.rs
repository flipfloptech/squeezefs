//! µs-bucket latency-histogram core — the ONE definition of the repo's
//! standard 26-bucket latency shape (`<=1us` … `>16s`, powers of two).
//!
//! Canonical file in the `squeezefs-ipc` tree, `#[path]`-included by the
//! root crate (`LatencyHistogram` delegates its bucketing and labels here)
//! and by the fuse3 fork (the `read_transport_phase_ns` transport-side
//! histograms) — the `numa_core.rs`/`thp.rs` production-sharing precedent.
//! Both consumers bucket through the same function, so the stats-inode
//! phase tables (`read_serve_phase_ns` root-side vs
//! `read_transport_phase_ns` fuse3-side) are bucket-for-bucket comparable
//! BY CONSTRUCTION — drift between the two crates' histograms is
//! structurally impossible, not merely tested against.
//!
//! Dependency-free on purpose (std only; no atomics — storage belongs to
//! the including crate, exactly like the other shared cores: type
//! identities never cross a crate boundary).

/// Bucket count of the standard latency histogram.
pub const LATENCY_BUCKETS: usize = 26;

/// Bucket labels, index-aligned with [`latency_bucket_index`].
pub const LATENCY_BUCKET_LABELS: [&str; LATENCY_BUCKETS] = [
    "<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us", "<=256us",
    "<=512us", "<=1024us", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms",
    "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s", "<=16s", ">16s",
];

/// Bucket index for a duration in microseconds (the historical
/// `LatencyHistogram::record` formula, verbatim: ≤1 µs ⇒ 0, then one
/// bucket per power of two, saturating at the last bucket).
#[inline]
pub fn latency_bucket_index(micros: u64) -> usize {
    if micros <= 1 {
        0
    } else {
        std::cmp::min((micros - 1).ilog2() as usize + 1, LATENCY_BUCKETS - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_boundaries_match_the_labels() {
        assert_eq!(latency_bucket_index(0), 0);
        assert_eq!(latency_bucket_index(1), 0);
        assert_eq!(latency_bucket_index(2), 1); // <=2us
        assert_eq!(latency_bucket_index(3), 2); // <=4us
        assert_eq!(latency_bucket_index(4), 2); // <=4us
        assert_eq!(latency_bucket_index(1024), 10); // <=1024us
        assert_eq!(latency_bucket_index(16_000_000), 24); // <=16s
        assert_eq!(latency_bucket_index(u64::MAX), LATENCY_BUCKETS - 1);
        assert_eq!(LATENCY_BUCKET_LABELS.len(), LATENCY_BUCKETS);
    }
}
