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

use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

/// Directory (directly under the mountpoint) holding the persistent
/// bench dataset.
const DATASET_DIR: &str = "squeezefs-bench";

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
        match self {
            Phase::Write => "Write",
            Phase::Read => "Read",
            Phase::Stat => "Stat",
            Phase::Del => "Del",
        }
    }

    /// Whether this phase moves data (write/read) or is metadata-only.
    fn moves_data(self) -> bool {
        matches!(self, Phase::Write | Phase::Read)
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
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.bytes as f64 / (1024.0 * 1024.0)) / secs
    }

    /// Aggregate operations per second.
    pub fn iops(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.ops as f64 / secs
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
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty size (expected e.g. 4k / 128k / 4m / 10g or plain bytes)".to_string());
    }
    let (digits, multiplier) = match trimmed.chars().last() {
        Some(c) if c.eq_ignore_ascii_case(&'k') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some(c) if c.eq_ignore_ascii_case(&'m') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some(c) if c.eq_ignore_ascii_case(&'g') => {
            (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024)
        }
        _ => (trimmed, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "invalid size {trimmed:?} (expected e.g. 4k / 128k / 4m / 10g or plain bytes)"
        ));
    }
    let value: u64 = digits
        .parse()
        .map_err(|e| format!("invalid size {trimmed:?}: {e}"))?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {trimmed:?} overflows 64 bits"))?;
    if bytes == 0 {
        return Err(format!("size {trimmed:?} must be non-zero"));
    }
    Ok(bytes)
}

/// Map phase flags to the fixed execution order `write, read, stat, del`
/// (regardless of flag order on the command line). No flags selects the
/// default `write + read` set.
pub fn select_phases(write: bool, read: bool, stat: bool, del: bool) -> Vec<Phase> {
    if !write && !read && !stat && !del {
        return vec![Phase::Write, Phase::Read];
    }
    let mut phases = Vec::new();
    if write {
        phases.push(Phase::Write);
    }
    if read {
        phases.push(Phase::Read);
    }
    if stat {
        phases.push(Phase::Stat);
    }
    if del {
        phases.push(Phase::Del);
    }
    phases
}

/// Validate the shape before any phase runs: nonzero counts/sizes and the
/// O_DIRECT alignment contract (`block % 4096 == 0`, `size % block == 0`).
pub fn validate_shape(shape: &Shape) -> Result<(), BenchError> {
    if shape.threads == 0 {
        return Err(BenchError::Shape(
            "--threads must be >= 1 (0 workers would benchmark nothing)".to_string(),
        ));
    }
    if shape.files == 0 {
        return Err(BenchError::Shape(
            "--files must be >= 1 (0 files per thread would benchmark nothing)".to_string(),
        ));
    }
    if shape.size == 0 {
        return Err(BenchError::Shape("--size must be non-zero".to_string()));
    }
    if shape.block == 0 {
        return Err(BenchError::Shape("--block must be non-zero".to_string()));
    }
    if shape.direct {
        if shape.block % 4096 != 0 {
            return Err(BenchError::Shape(format!(
                "--direct requires the I/O block size (-b) to be a multiple of 4096 bytes \
                 (O_DIRECT alignment); got {} bytes",
                shape.block
            )));
        }
        if shape.size % shape.block != 0 {
            return Err(BenchError::Shape(format!(
                "--direct requires the file size (-s) to be a multiple of the block size (-b): \
                 O_DIRECT cannot write a partial EOF tail block (got size={} block={}, \
                 remainder={})",
                shape.size,
                shape.block,
                shape.size % shape.block
            )));
        }
    }
    Ok(())
}

/// Root directory of the persistent dataset: `<mountpoint>/squeezefs-bench`.
pub fn dataset_root(mount: &Path) -> PathBuf {
    mount.join(DATASET_DIR)
}

/// Path of one dataset file: `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin`.
pub fn bench_file_path(mount: &Path, tid: usize, fid: usize) -> PathBuf {
    dataset_root(mount)
        .join(format!("t{tid}"))
        .join(format!("f{fid}.bin"))
}

/// Count every regular file under `dir` (recursively). Dataset strays are
/// as much of a shape mismatch as missing files, so everything counts.
fn count_files_recursive(dir: &Path) -> std::io::Result<u64> {
    let mut count = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            count += count_files_recursive(&entry.path())?;
        } else {
            count += 1;
        }
    }
    Ok(count)
}

/// Validate that an existing dataset matches the requested shape (file
/// count and per-file sizes) BEFORE any timing starts. On mismatch the
/// error reports found-vs-expected and tells the user to run `-w`.
/// Never creates anything.
pub fn validate_dataset(mount: &Path, shape: &Shape) -> Result<(), BenchError> {
    let root = dataset_root(mount);
    let expected_files = (shape.threads as u64) * (shape.files as u64);
    let shape_hint = format!(
        "expected {} file(s) ({} thread(s) x {} file(s)/thread) of {} bytes each under {}; \
         run the write phase first (-w) with this shape to (re)create the dataset",
        expected_files,
        shape.threads,
        shape.files,
        shape.size,
        root.display()
    );

    if !root.is_dir() {
        return Err(BenchError::Dataset(format!(
            "no bench dataset found at {}: {shape_hint}",
            root.display()
        )));
    }

    let found = count_files_recursive(&root)?;
    if found != expected_files {
        return Err(BenchError::Dataset(format!(
            "bench dataset shape mismatch: found {found} file(s) under {}, {shape_hint}",
            root.display()
        )));
    }

    for tid in 0..shape.threads {
        for fid in 0..shape.files {
            let path = bench_file_path(mount, tid, fid);
            let meta = std::fs::metadata(&path).map_err(|e| {
                BenchError::Dataset(format!(
                    "bench dataset file {} is missing ({e}): {shape_hint}",
                    path.display()
                ))
            })?;
            if meta.len() != shape.size {
                return Err(BenchError::Dataset(format!(
                    "bench dataset size mismatch: {} is {} bytes, expected {} bytes: {shape_hint}",
                    path.display(),
                    meta.len(),
                    shape.size
                )));
            }
        }
    }
    Ok(())
}

/// Number of `block`-sized operations per file (`ceil(size / block)`);
/// the final block may be partial when `size % block != 0`.
pub fn block_count(shape: &Shape) -> u64 {
    shape.size.div_ceil(shape.block)
}

/// The per-file block visit order: identity for sequential, a shuffled
/// full-coverage permutation of `0..nblocks` for `--rand` (every block
/// exactly once).
pub fn block_order(rand: bool, nblocks: u64) -> Vec<u64> {
    let mut order: Vec<u64> = (0..nblocks).collect();
    if rand {
        order.shuffle(&mut rand::thread_rng());
    }
    order
}

/// splitmix64 finalizer — cheap avalanche for seed derivation.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministic per-`(tid, fid, block)` seed for the write pattern.
pub fn block_seed(tid: usize, fid: usize, block: u64) -> u64 {
    // "SQFSBENC" — fixed generator tag so patterns are reproducible
    // across processes and runs.
    let mut seed = splitmix64(0x5351_4653_4245_4E43 ^ (tid as u64));
    seed = splitmix64(seed ^ (fid as u64));
    seed = splitmix64(seed ^ block);
    if seed == 0 {
        // xorshift64* needs a non-zero state; remap the (vanishingly
        // unlikely) zero seed.
        seed = 0xA5A5_A5A5_A5A5_A5A5;
    }
    seed
}

/// Fill `buf` with the cheap deterministic non-zero pattern for
/// `(tid, fid, block)`. Same inputs + same length ⇒ same bytes, so reads
/// could in principle verify content; the stream is xorshift-based so
/// compression cannot fake write numbers.
pub fn fill_block(buf: &mut [u8], tid: usize, fid: usize, block: u64) {
    let mut state = block_seed(tid, fid, block);
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let mut chunks = buf.chunks_exact_mut(8);
    for chunk in &mut chunks {
        chunk.copy_from_slice(&next().to_le_bytes());
    }
    let rem = chunks.into_remainder();
    if !rem.is_empty() {
        let bytes = next().to_le_bytes();
        let len = rem.len();
        rem.copy_from_slice(&bytes[..len]);
    }
}

/// O_DIRECT-compatible buffer: page-aligned (4096) storage when `direct`
/// is set, plain heap otherwise.
struct AlignedBuf {
    _raw: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(size: usize, direct: bool) -> Self {
        if direct {
            let raw = vec![0u8; size + 4096];
            let addr = raw.as_ptr() as usize;
            let offset = (4096 - (addr % 4096)) % 4096;
            Self {
                _raw: raw,
                offset,
                len: size,
            }
        } else {
            Self {
                _raw: vec![0u8; size],
                offset: 0,
                len: size,
            }
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self._raw[self.offset..self.offset + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self._raw[self.offset..self.offset + self.len]
    }
}

/// Per-worker accumulation: op count, bytes moved, per-op latencies (ns).
struct WorkerOut {
    ops: u64,
    bytes: u64,
    lat_ns: Vec<u64>,
}

impl WorkerOut {
    fn with_capacity(cap: usize) -> Self {
        Self {
            ops: 0,
            bytes: 0,
            lat_ns: Vec::with_capacity(cap),
        }
    }
}

fn open_options(phase: Phase, direct: bool) -> tokio::fs::OpenOptions {
    let mut options = tokio::fs::OpenOptions::new();
    match phase {
        // Overwrite in place (no O_TRUNC): iteration N+1 measures steady
        // -state overwrites of the same inodes; the post-write set_len
        // makes -w authoritative for the shape anyway.
        Phase::Write => {
            options.write(true).create(true);
        }
        _ => {
            options.read(true);
        }
    }
    if direct {
        options.custom_flags(libc::O_DIRECT);
    }
    options
}

/// Byte length of block `b` (the final block may be partial).
fn block_len(shape: &Shape, b: u64) -> usize {
    std::cmp::min(shape.block, shape.size - b * shape.block) as usize
}

async fn write_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let nblocks = block_count(&shape);
    let mut out = WorkerOut::with_capacity((nblocks as usize).saturating_mul(shape.files));
    let dir = dataset_root(&mount).join(format!("t{tid}"));
    tokio::fs::create_dir_all(&dir).await?;
    let mut buf = AlignedBuf::new(shape.block as usize, shape.direct);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let mut file = open_options(Phase::Write, shape.direct).open(&path).await?;
        let mut pos = 0u64;
        for b in block_order(shape.rand, nblocks) {
            let off = b * shape.block;
            let len = block_len(&shape, b);
            fill_block(&mut buf.as_mut_slice()[..len], tid, fid, b);
            let t0 = Instant::now();
            if pos != off {
                file.seek(SeekFrom::Start(off)).await?;
            }
            file.write_all(&buf.as_slice()[..len]).await?;
            out.lat_ns.push(t0.elapsed().as_nanos() as u64);
            pos = off + len as u64;
            out.ops += 1;
            out.bytes += len as u64;
            pb.inc(1);
        }
        // -w is authoritative for the dataset shape: trim any stale tail
        // left by a previously larger dataset, then make it durable —
        // fsync is INSIDE the timed phase (honest durable write numbers).
        file.set_len(shape.size).await?;
        file.sync_all().await?;
    }
    Ok(out)
}

async fn read_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let nblocks = block_count(&shape);
    let mut out = WorkerOut::with_capacity((nblocks as usize).saturating_mul(shape.files));
    let mut buf = AlignedBuf::new(shape.block as usize, shape.direct);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let mut file = open_options(Phase::Read, shape.direct).open(&path).await?;
        let mut pos = 0u64;
        for b in block_order(shape.rand, nblocks) {
            let off = b * shape.block;
            // Exact short read at EOF: the final block reads only the
            // partial tail when size % block != 0.
            let len = block_len(&shape, b);
            let t0 = Instant::now();
            if pos != off {
                file.seek(SeekFrom::Start(off)).await?;
            }
            file.read_exact(&mut buf.as_mut_slice()[..len]).await?;
            out.lat_ns.push(t0.elapsed().as_nanos() as u64);
            pos = off + len as u64;
            out.ops += 1;
            out.bytes += len as u64;
            pb.inc(1);
        }
    }
    Ok(out)
}

async fn stat_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let mut out = WorkerOut::with_capacity(shape.files);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let t0 = Instant::now();
        tokio::fs::metadata(&path).await?;
        out.lat_ns.push(t0.elapsed().as_nanos() as u64);
        out.ops += 1;
        pb.inc(1);
    }
    Ok(out)
}

async fn del_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let mut out = WorkerOut::with_capacity(shape.files);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let t0 = Instant::now();
        tokio::fs::remove_file(&path).await?;
        out.lat_ns.push(t0.elapsed().as_nanos() as u64);
        out.ops += 1;
        pb.inc(1);
    }
    Ok(out)
}

/// Nearest-rank percentile on an already-sorted latency vector.
fn percentile_ns(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn progress_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("#>-")
}

async fn run_one_phase(
    mount: &Path,
    phase: Phase,
    shape: &Shape,
    mp: &MultiProgress,
) -> Result<PhaseResult, BenchError> {
    let per_file_ops = if phase.moves_data() {
        block_count(shape)
    } else {
        1
    };
    let total_ops = per_file_ops * (shape.threads as u64) * (shape.files as u64);
    let pb = mp.add(ProgressBar::new(total_ops));
    pb.set_style(progress_style());
    pb.set_message(phase.label());

    let start = Instant::now();
    let mut tasks = Vec::with_capacity(shape.threads);
    for tid in 0..shape.threads {
        let mount = mount.to_path_buf();
        let shape = shape.clone();
        let pb = pb.clone();
        tasks.push(tokio::spawn(async move {
            match phase {
                Phase::Write => write_worker(mount, tid, shape, pb).await,
                Phase::Read => read_worker(mount, tid, shape, pb).await,
                Phase::Stat => stat_worker(mount, tid, shape, pb).await,
                Phase::Del => del_worker(mount, tid, shape, pb).await,
            }
        }));
    }

    let mut ops = 0u64;
    let mut bytes = 0u64;
    let mut lat_ns: Vec<u64> = Vec::with_capacity(total_ops as usize);
    for task in tasks {
        let out = task.await??;
        ops += out.ops;
        bytes += out.bytes;
        lat_ns.extend(out.lat_ns);
    }
    let elapsed = start.elapsed();
    pb.finish_with_message(format!("{} done", phase.label()));

    if phase == Phase::Del {
        // Cleanup bookkeeping OUTSIDE the timed region: the timed ops are
        // the file unlinks; removing the (now-empty) t{tid} dirs and the
        // dataset root is not part of the measurement.
        tokio::fs::remove_dir_all(dataset_root(mount)).await?;
    }

    lat_ns.sort_unstable();
    let sum_ns: u128 = lat_ns.iter().map(|&v| v as u128).sum();
    let avg_ns = if lat_ns.is_empty() {
        0
    } else {
        (sum_ns / lat_ns.len() as u128) as u64
    };
    Ok(PhaseResult {
        phase,
        ops,
        bytes,
        elapsed,
        lat_min: Duration::from_nanos(lat_ns.first().copied().unwrap_or(0)),
        lat_avg: Duration::from_nanos(avg_ns),
        lat_p99: Duration::from_nanos(percentile_ns(&lat_ns, 99.0)),
        lat_max: Duration::from_nanos(lat_ns.last().copied().unwrap_or(0)),
    })
}

/// Execute one iteration of the given phases (already in fixed order) over
/// `mount` with `shape`. When the set does not include [`Phase::Write`],
/// the existing dataset is validated against the shape before any timing.
pub async fn run_phases(
    mount: &Path,
    phases: &[Phase],
    shape: &Shape,
) -> Result<BenchReport, BenchError> {
    validate_shape(shape)?;
    if !phases.contains(&Phase::Write) && !phases.is_empty() {
        // Reusing a dataset from an earlier -w: its shape must match
        // BEFORE any timing starts. Never silently create files here.
        validate_dataset(mount, shape)?;
    }
    let mp = MultiProgress::new();
    let mut report = BenchReport::default();
    for &phase in phases {
        report
            .phases
            .push(run_one_phase(mount, phase, shape, &mp).await?);
    }
    Ok(report)
}

/// Snapshot the daemon metric counters from the mount's virtual `.stats`
/// file (None when the path is not a live SqueezeFS mount).
pub fn get_daemon_metrics_from_stats(mount_path: &Path) -> Option<HashMap<String, u64>> {
    let stats_path = mount_path.join(".stats");
    let stats_str = std::fs::read_to_string(&stats_path).ok()?;
    let stats_json: serde_json::Value = serde_json::from_str(&stats_str).ok()?;

    let mut metrics_map = HashMap::new();
    if let Some(metrics_obj) = stats_json.get("metrics").and_then(|v| v.as_object()) {
        for (k, v) in metrics_obj {
            if let Some(val_u64) = v.as_u64() {
                metrics_map.insert(k.clone(), val_u64);
            }
        }
    }

    Some(metrics_map)
}

/// Best-effort detection of a live SqueezeFS mount (virtual `.config` +
/// `.stats` files). Only warns — the bench itself runs against any
/// directory (POSIX-only mode, no daemon metrics).
fn warn_if_not_squeezefs_mount(path: &Path) {
    let config_path = path.join(".config");
    let stats_path = path.join(".stats");
    let is_squeeze = {
        let cfg_ok =
            std::fs::metadata(&config_path).is_ok() && std::fs::metadata(&stats_path).is_ok();
        if cfg_ok {
            match std::fs::read_to_string(&config_path) {
                Ok(config_str) => {
                    // Accept valid JSON, or any readable .config alongside
                    // .stats (partial/wedged JSON should not force
                    // POSIX-only mode).
                    serde_json::from_str::<serde_json::Value>(&config_str).is_ok()
                        || !config_str.is_empty()
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    println!(
                        "{}",
                        "Error: Permission denied accessing FUSE mountpoint configuration.\n\
                         Note: FUSE mounts are restricted to the mounting user by default.\n\
                         Please run the benchmark without 'sudo', or ensure 'allow_other' was set during mount."
                            .red()
                            .bold()
                    );
                    false
                }
                Err(e) => {
                    println!(
                        "{}",
                        format!(
                            "Warning: could not read {}: {} (mount may be wedged or not SqueezeFS).",
                            config_path.display(),
                            e
                        )
                        .yellow()
                        .bold()
                    );
                    // Still allow metrics if .stats is visible.
                    stats_path.exists()
                }
            }
        } else {
            false
        }
    };

    if !is_squeeze {
        println!(
            "{}",
            format!(
                "Warning: {} does not look like a live SqueezeFS mount (missing .config/.stats).",
                path.display()
            )
            .yellow()
            .bold()
        );
        println!(
            "{}",
            "Running POSIX-only benchmark (no daemon metrics). Mount with allow_other and pass the real mountpoint."
                .yellow()
                .bold()
        );
    }
}

/// Render a byte count in the bench's own size vocabulary (4k/1m/10g),
/// falling back to plain bytes when not a whole binary unit.
fn fmt_size(bytes: u64) -> String {
    const G: u64 = 1024 * 1024 * 1024;
    const M: u64 = 1024 * 1024;
    const K: u64 = 1024;
    if bytes % G == 0 {
        format!("{}g", bytes / G)
    } else if bytes % M == 0 {
        format!("{}m", bytes / M)
    } else if bytes % K == 0 {
        format!("{}k", bytes / K)
    } else {
        format!("{bytes}")
    }
}

fn fmt_latency(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 100_000.0 {
        format!("{:.1} ms", us / 1000.0)
    } else if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.1} us")
    }
}

/// Print the effective shape line (replaces the old canned-workload
/// header).
fn print_shape_header(mount: &Path, phases: &[Phase], shape: &Shape, iterations: usize) {
    let phase_list = phases
        .iter()
        .map(|p| p.label().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(",");
    let sep = "==================================================================================";
    println!("{}", sep.bold());
    println!(
        "  SqueezeFS Bench @ {} — phases: {}",
        mount.display(),
        phase_list
    );
    println!(
        "  threads={} files/thread={} size={} ({} B) block={} ({} B) access={} io={} iterations={}",
        shape.threads,
        shape.files,
        fmt_size(shape.size),
        shape.size,
        fmt_size(shape.block),
        shape.block,
        if shape.rand { "rand" } else { "seq" },
        if shape.direct { "direct" } else { "buffered" },
        iterations
    );
    println!("{}", sep.bold());
}

fn print_report(report: &BenchReport) {
    let hline = "+--------+------------------+------------------+------------+------------+------------+------------+";
    println!("\n{}", hline.bold());
    println!(
        "| {:<6} | {:<16} | {:<16} | {:<10} | {:<10} | {:<10} | {:<10} |",
        "PHASE", "THROUGHPUT", "IOPS", "LAT MIN", "LAT AVG", "LAT P99", "LAT MAX"
    );
    println!("{}", hline.bold());
    for res in &report.phases {
        let tput_str = if res.phase.moves_data() {
            let v = res.throughput_mib_s();
            let s = format!("{v:>10.2} MiB/s");
            if v > 100.0 {
                s.green()
            } else if v > 50.0 {
                s.yellow()
            } else {
                s.red()
            }
        } else {
            format!("{:>16}", "-").normal()
        };
        let iops_v = res.iops();
        let iops_s = format!("{iops_v:>10.2} ops/s");
        let iops_str = if res.phase.moves_data() {
            if iops_v > 200.0 {
                iops_s.green()
            } else if iops_v > 100.0 {
                iops_s.yellow()
            } else {
                iops_s.red()
            }
        } else if iops_v > 2000.0 {
            iops_s.green()
        } else if iops_v > 1000.0 {
            iops_s.yellow()
        } else {
            iops_s.red()
        };
        println!(
            "| {:<6} | {:>16} | {:>16} | {:>10} | {:>10} | {:>10} | {:>10} |",
            res.phase.label(),
            tput_str,
            iops_str,
            fmt_latency(res.lat_min),
            fmt_latency(res.lat_avg),
            fmt_latency(res.lat_p99),
            fmt_latency(res.lat_max)
        );
    }
    println!("{}", hline.bold());
}

fn print_metrics_delta(base: Option<HashMap<String, u64>>, post: Option<HashMap<String, u64>>) {
    if let (Some(base), Some(post)) = (base, post) {
        println!(
            "\n{}",
            "Daemon Backend Metrics (End-to-End Audit):".bold().cyan()
        );
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );
        println!("| {:<28} | {:<18} |", "DAEMON METRIC", "DIFF VALUE");
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );

        let show_metric = |label: &str, key: &str| {
            let v1 = base.get(key).unwrap_or(&0);
            let v2 = post.get(key).unwrap_or(&0);
            let diff = v2.saturating_sub(*v1);
            println!("| {label:<28} | {diff:>18} |");
        };

        show_metric("FUSE Operations", "fuse_ops");
        show_metric("Metadata Updates", "meta_updates");
        show_metric("Block Backend Write", "put_obj");
        show_metric("Block Backend Read", "get_obj");
        show_metric("Block Backend Delete", "del_obj");
        show_metric("Cache Hits (RAM)", "cache_hits");
        show_metric("Cache Misses", "cache_misses");
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );
    } else {
        println!(
            "\n(Note: FUSE daemon metrics not available. Ensure squeezefs mount is running locally)"
        );
    }
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
    if iterations == 0 {
        return Err(BenchError::Shape("--iterations must be >= 1".to_string()));
    }
    validate_shape(shape)?;

    if let Err(e) = std::fs::metadata(mount) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(BenchError::Dataset(format!(
                "Permission denied accessing benchmark path {mount:?}.\n\
                 Note: FUSE mounts are by default only accessible to the mounting user.\n\
                 Please run the command as the mounting user (without sudo), or ensure the \
                 filesystem was mounted with FUSE options allow_other."
            )));
        }
        return Err(BenchError::Dataset(format!(
            "Benchmark path {mount:?} does not exist: {e}"
        )));
    }

    warn_if_not_squeezefs_mount(mount);
    print_shape_header(mount, phases, shape, iterations);

    for iter in 1..=iterations {
        if iterations > 1 {
            println!("\n--- Benchmark Iteration {iter}/{iterations} ---");
        }
        // Daemon metrics come straight from `.stats`; don't gate them on
        // the `.config`-based mount detection. A readable/parseable
        // `.stats` is sufficient, so metrics still show even when
        // `.config` is momentarily unreadable.
        let baseline = get_daemon_metrics_from_stats(mount);
        let report = run_phases(mount, phases, shape).await?;
        let post = get_daemon_metrics_from_stats(mount);
        print_report(&report);
        print_metrics_delta(baseline, post);
    }
    Ok(())
}
