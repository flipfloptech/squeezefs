//! `squeezefs bench` engine — simplified-elbencho benchmark model.
//!
//! Explicit phases (`write`, `read`, `stat`, `del`) executed in a fixed
//! order over a **persistent, reusable dataset** laid out as
//! `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin`, one shape vocabulary
//! (`threads` × `files` × `size` @ `block`), sequential or `--rand`
//! shuffled full-coverage access, optional `O_DIRECT`.
//!
//! The engine is deliberately decoupled from FUSE: it benchmarks *any*
//! directory through ordinary POSIX/tokio file I/O — bench is the load
//! generator measuring the mounted filesystem through the kernel, exactly
//! like elbencho. Do **not** route bench I/O through `crate::uring_fs`;
//! the io_uring mandate governs the filesystem's own data paths, not this
//! measurement client.
//!
//! Write phases produce a cheap deterministic non-zero pattern seeded per
//! `(tid, fid, block)` (see [`fill_block`]) so compression cannot fake
//! numbers and reads could in principle verify content. Each file is
//! fsync'd before the write clock stops — the write phase reports durable
//! numbers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Errors surfaced by the bench engine. All are loud and fatal — the
/// engine never silently repairs a shape/dataset mismatch.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// Invalid shape (thread/file counts, size/block alignment, O_DIRECT
    /// constraint violations).
    #[error("{0}")]
    Shape(String),
    /// The on-disk dataset does not match the requested shape.
    #[error("{0}")]
    Dataset(String),
    /// Underlying I/O failure while running a phase.
    #[error("bench I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A worker task panicked or was cancelled.
    #[error("bench worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Benchmark phases, in their fixed execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Create/overwrite the dataset, timed (includes open/create and the
    /// per-file fsync — durable write numbers).
    Write,
    /// Read the dataset back, timed.
    Read,
    /// Stat every file, timed.
    Stat,
    /// Delete the dataset, timed (doubles as cleanup).
    Del,
}

impl Phase {
    /// Human label used in progress bars and the results table.
    pub fn label(self) -> &'static str {
        todo!("bench engine not implemented yet")
    }
}

/// The one shape vocabulary shared by every phase.
#[derive(Debug, Clone)]
pub struct Shape {
    /// Worker count; worker `tid` owns `t{tid}/`.
    pub threads: usize,
    /// Files per thread.
    pub files: usize,
    /// Size of each file in bytes.
    pub size: u64,
    /// I/O size per operation in bytes.
    pub block: u64,
    /// Random offsets: shuffled full-coverage block list (every block
    /// exactly once).
    pub rand: bool,
    /// O_DIRECT I/O (requires `block % 4096 == 0` and `size % block == 0`).
    pub direct: bool,
}

/// Timed result of one phase (aggregated across all workers).
#[derive(Debug, Clone)]
pub struct PhaseResult {
    /// Which phase this row measures.
    pub phase: Phase,
    /// Total operations across all workers.
    pub ops: u64,
    /// Total bytes moved (0 for stat/del).
    pub bytes: u64,
    /// Wall-clock duration of the phase.
    pub elapsed: Duration,
    /// Fastest single operation.
    pub lat_min: Duration,
    /// Mean operation latency.
    pub lat_avg: Duration,
    /// 99th-percentile operation latency (nearest-rank on the sorted
    /// per-op latency vector).
    pub lat_p99: Duration,
    /// Slowest single operation.
    pub lat_max: Duration,
}

impl PhaseResult {
    /// Aggregate throughput in MiB/s (0.0 for byte-less phases).
    pub fn throughput_mib_s(&self) -> f64 {
        todo!("bench engine not implemented yet")
    }

    /// Aggregate operations per second.
    pub fn iops(&self) -> f64 {
        todo!("bench engine not implemented yet")
    }
}

/// Results of one iteration of the selected phase set.
#[derive(Debug, Clone, Default)]
pub struct BenchReport {
    /// One entry per executed phase, in execution order.
    pub phases: Vec<PhaseResult>,
}

/// Parse a human-readable size: plain bytes (`1048576`) or `k`/`m`/`g`
/// binary-unit suffixes (`4k`, `128k`, `4m`, `10g`), case-insensitive.
/// Zero and garbage are rejected loudly.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let _ = s;
    todo!("bench engine not implemented yet")
}

/// Map phase flags to the fixed execution order `write, read, stat, del`
/// (regardless of flag order on the command line). No flags selects the
/// default `write + read` set.
pub fn select_phases(write: bool, read: bool, stat: bool, del: bool) -> Vec<Phase> {
    let _ = (write, read, stat, del);
    todo!("bench engine not implemented yet")
}

/// Validate the shape before any phase runs: nonzero counts/sizes and the
/// O_DIRECT alignment contract (`block % 4096 == 0`, `size % block == 0`).
pub fn validate_shape(shape: &Shape) -> Result<(), BenchError> {
    let _ = shape;
    todo!("bench engine not implemented yet")
}

/// Root directory of the persistent dataset: `<mountpoint>/squeezefs-bench`.
pub fn dataset_root(mount: &Path) -> PathBuf {
    let _ = mount;
    todo!("bench engine not implemented yet")
}

/// Path of one dataset file: `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin`.
pub fn bench_file_path(mount: &Path, tid: usize, fid: usize) -> PathBuf {
    let _ = (mount, tid, fid);
    todo!("bench engine not implemented yet")
}

/// Validate that an existing dataset matches the requested shape (file
/// count and per-file sizes) BEFORE any timing starts. On mismatch the
/// error reports found-vs-expected and tells the user to run `-w`.
/// Never creates anything.
pub fn validate_dataset(mount: &Path, shape: &Shape) -> Result<(), BenchError> {
    let _ = (mount, shape);
    todo!("bench engine not implemented yet")
}

/// Number of `block`-sized operations per file (`ceil(size / block)`);
/// the final block may be partial when `size % block != 0`.
pub fn block_count(shape: &Shape) -> u64 {
    let _ = shape;
    todo!("bench engine not implemented yet")
}

/// The per-file block visit order: identity for sequential, a shuffled
/// full-coverage permutation of `0..nblocks` for `--rand` (every block
/// exactly once).
pub fn block_order(rand: bool, nblocks: u64) -> Vec<u64> {
    let _ = (rand, nblocks);
    todo!("bench engine not implemented yet")
}

/// Deterministic per-`(tid, fid, block)` seed for the write pattern.
pub fn block_seed(tid: usize, fid: usize, block: u64) -> u64 {
    let _ = (tid, fid, block);
    todo!("bench engine not implemented yet")
}

/// Fill `buf` with the cheap deterministic non-zero pattern for
/// `(tid, fid, block)`. Same inputs + same length ⇒ same bytes, so reads
/// could in principle verify content; the stream is xorshift-based so
/// compression cannot fake write numbers.
pub fn fill_block(buf: &mut [u8], tid: usize, fid: usize, block: u64) {
    let _ = (buf, tid, fid, block);
    todo!("bench engine not implemented yet")
}

/// Execute one iteration of the given phases (already in fixed order) over
/// `mount` with `shape`. When the set does not include [`Phase::Write`],
/// the existing dataset is validated against the shape before any timing.
pub async fn run_phases(
    mount: &Path,
    phases: &[Phase],
    shape: &Shape,
) -> Result<BenchReport, BenchError> {
    let _ = (mount, phases, shape);
    todo!("bench engine not implemented yet")
}

/// Snapshot the daemon metric counters from the mount's virtual `.stats`
/// file (None when the path is not a live SqueezeFS mount).
pub fn get_daemon_metrics_from_stats(mount_path: &Path) -> Option<HashMap<String, u64>> {
    let _ = mount_path;
    todo!("bench engine not implemented yet")
}

/// CLI entry point: mount detection warnings, `iterations` repetitions of
/// the phase set (fresh timing each), per-iteration results table and
/// daemon `.stats` metrics delta.
pub async fn run_cli(
    mount: &Path,
    phases: &[Phase],
    shape: &Shape,
    iterations: usize,
) -> Result<(), BenchError> {
    let _ = (mount, phases, shape, iterations);
    todo!("bench engine not implemented yet")
}
