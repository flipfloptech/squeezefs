//! E2E performance-audit instrument ladder (PR A1) — the contracts the
//! `.benchmarks/2026-09-02-e2e-audit` note's numbers rest on. Every
//! `*_phase_ns` family was a 26-bucket power-of-two µs histogram with no
//! sum and no count word, so every mean the evidence notes quote carried
//! up to 2× per-bucket error and no containment law (`total ≈ Σ phases`)
//! was checkable to the ns. These are histogram/export laws, never perf
//! numbers.
//!
//! A. Exact sums: `LatencyHistogram` (root) and the fuse3 sharded tables
//!    carry `count` + `sum_ns`; JSON = `{"buckets": {label: n}, "count",
//!    "sum_ns", "mean_ns"}` with the bucket labels byte-identical.

use squeezefs::fuse_client::LatencyHistogram;
use squeezefs::latency_core::{latency_bucket_index, LATENCY_BUCKETS, LATENCY_BUCKET_LABELS};
use std::time::Duration;

// ---------------------------------------------------------------------------
// A — exact sums
// ---------------------------------------------------------------------------

/// The four top-level keys every histogram export carries.
const HIST_KEYS: [&str; 4] = ["buckets", "count", "sum_ns", "mean_ns"];

fn hist_count(h: &serde_json::Value) -> u64 {
    h["count"].as_u64().expect("count is u64")
}

fn hist_sum_ns(h: &serde_json::Value) -> u64 {
    h["sum_ns"].as_u64().expect("sum_ns is u64")
}

fn bucket_sum(h: &serde_json::Value) -> u64 {
    h["buckets"]
        .as_object()
        .expect("buckets is an object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

#[test]
fn histogram_reports_exact_sum_count_and_mean() {
    let h = LatencyHistogram::default();
    assert_eq!(h.count(), 0);
    assert_eq!(h.sum_ns(), 0);
    assert_eq!(h.mean_ns(), 0, "an empty histogram's mean is 0, never NaN");

    let spans_ns: [u64; 5] = [1, 999, 1_000, 123_456, 7_000_000_000];
    for ns in spans_ns {
        h.record(Duration::from_nanos(ns));
    }
    let want_sum: u64 = spans_ns.iter().sum();
    assert_eq!(h.count(), spans_ns.len() as u64, "one count per record");
    assert_eq!(h.sum_ns(), want_sum, "sum is exact to the ns, not a bucket estimate");
    assert_eq!(h.mean_ns(), want_sum / spans_ns.len() as u64);

    // The bucket law is unchanged: the same µs → index formula as before.
    let json = h.to_json();
    for ns in spans_ns {
        let idx = latency_bucket_index(ns / 1_000);
        let n = json["buckets"][LATENCY_BUCKET_LABELS[idx]]
            .as_u64()
            .expect("bucket present");
        assert!(n >= 1, "{ns} ns must land in bucket {idx}");
    }
    assert_eq!(bucket_sum(&json), h.count(), "Σ buckets ≡ count");
}

#[test]
fn histogram_json_shape_is_pinned() {
    let h = LatencyHistogram::default();
    h.record(Duration::from_micros(100));
    h.record(Duration::from_micros(300));
    let json = h.to_json();
    let obj = json.as_object().expect("histogram JSON is an object");
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    let mut want = HIST_KEYS.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want, "exactly the four top-level keys");

    let buckets = json["buckets"].as_object().expect("buckets object");
    assert_eq!(buckets.len(), LATENCY_BUCKETS);
    for label in LATENCY_BUCKET_LABELS {
        assert!(buckets.contains_key(label), "label {label} byte-identical");
    }
    assert_eq!(hist_count(&json), 2);
    assert_eq!(hist_sum_ns(&json), 400_000);
    assert_eq!(json["mean_ns"].as_u64(), Some(200_000));
}

#[test]
fn empty_histogram_json_is_all_zero() {
    let json = LatencyHistogram::default().to_json();
    assert_eq!(hist_count(&json), 0);
    assert_eq!(hist_sum_ns(&json), 0);
    assert_eq!(json["mean_ns"].as_u64(), Some(0));
    assert_eq!(bucket_sum(&json), 0);
}

/// Every root phase family exports through the one histogram shape.
#[test]
fn phase_families_export_the_exact_shape() {
    use squeezefs::fuse_client::{
        meta_txpass_phase_json, publish_phase_json, read_fill_phase_json,
        read_serve_phase_json, write_pipeline_phase_json,
    };
    for fam in [
        write_pipeline_phase_json(),
        publish_phase_json(),
        meta_txpass_phase_json(),
        read_serve_phase_json(),
        read_fill_phase_json(),
    ] {
        for (phase, h) in fam.as_object().expect("family object") {
            for k in HIST_KEYS {
                assert!(h.get(k).is_some(), "phase {phase} lacks {k}");
            }
            assert_eq!(bucket_sum(h), hist_count(h), "phase {phase}: Σ buckets ≡ count");
        }
    }
}

/// The fuse3 sharded tables fold EXACTLY across recording threads: N
/// threads each own a shard; the snapshot's count/sum deltas equal the
/// known totals to the ns.
#[test]
fn fuse3_sharded_fold_is_exact() {
    use fuse3::{write_transport_phase_record, write_transport_phase_snapshot, TransportPhase};
    const THREADS: u64 = 6;
    const PER_THREAD: u64 = 500;
    let phase_idx = write_transport_phase_snapshot()
        .iter()
        .position(|p| p.name == "commit_flush")
        .expect("commit_flush phase present");
    let before = write_transport_phase_snapshot();
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    // Distinct ns spans so a bucket-midpoint estimate could
                    // never reproduce the sum by accident.
                    let ns = 1_000 * (t + 1) + 7 * i;
                    write_transport_phase_record(
                        TransportPhase::CommitFlush,
                        Duration::from_nanos(ns),
                    );
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let after = write_transport_phase_snapshot();
    let want_count = THREADS * PER_THREAD;
    let want_sum: u64 = (0..THREADS)
        .flat_map(|t| (0..PER_THREAD).map(move |i| 1_000 * (t + 1) + 7 * i))
        .sum();
    assert_eq!(
        after[phase_idx].count - before[phase_idx].count,
        want_count,
        "count folds exactly across shards"
    );
    assert_eq!(
        after[phase_idx].sum_ns - before[phase_idx].sum_ns,
        want_sum,
        "sum_ns folds exactly across shards"
    );
    let bucket_delta: u64 = after[phase_idx]
        .buckets
        .iter()
        .zip(before[phase_idx].buckets.iter())
        .map(|(a, b)| a - b)
        .sum();
    assert_eq!(bucket_delta, want_count, "Σ buckets ≡ count on the fold too");

    // The daemon renders the fold through the same JSON shape.
    let json = squeezefs::fuse_client::write_transport_phase_json();
    let h = &json["commit_flush"];
    for k in HIST_KEYS {
        assert!(h.get(k).is_some(), "transport JSON lacks {k}");
    }
    assert_eq!(hist_count(h), after[phase_idx].count);
    assert_eq!(hist_sum_ns(h), after[phase_idx].sum_ns);
}
