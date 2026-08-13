//! `squeezefs bench` engine — simplified-elbencho benchmark model.
//!
//! Explicit phases (`write`, `read`, `stat`, `del`) executed in a fixed
//! order over a **persistent, reusable dataset** laid out as
//! `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin`, one shape vocabulary
//! (`threads` × `files` × `size` @ `block`), sequential or `--rand`
//! shuffled full-coverage access, optional `O_DIRECT`.
//!
//! The engine is deliberately decoupled from FUSE: it benchmarks *any*
//! directory through ordinary POSIX (std) file I/O — bench is the load
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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Directory (directly under the mountpoint) holding the persistent
/// bench dataset.
const DATASET_DIR: &str = "squeezefs-bench";

/// Auto-shape: worker-count cap (`threads = min(available_parallelism, 16)`).
pub const AUTO_THREADS_CAP: usize = 16;
/// Auto-shape: dataset floor — total = max(16 GiB, 2 GiB × threads).
pub const AUTO_TOTAL_FLOOR_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Auto-shape: per-thread dataset contribution (2 GiB × threads).
pub const AUTO_PER_THREAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Auto-shape: hard minimum — if even 4 GiB total does not fit under the
/// free-space cap, auto-sizing fails loudly.
pub const AUTO_MIN_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Auto sizes round down to 1 MiB so `-s % -b == 0` holds for BOTH suite
/// block sizes (1 MiB seq and 4 KiB rand; 4k divides 1m).
pub const AUTO_SIZE_ROUND_BYTES: u64 = 1024 * 1024;
/// The saturation suite's sequential-pass I/O block size.
pub const SUITE_SEQ_BLOCK: u64 = 1024 * 1024;
/// The saturation suite's random-pass I/O block size.
pub const SUITE_RAND_BLOCK: u64 = 4096;
/// Default wall-clock time box for `--rand` read/write passes (seconds);
/// sequential passes run to full coverage unless `--time` says otherwise.
pub const DEFAULT_RAND_TIME_BOX_SECS: u64 = 30;
/// Default I/O block size for explicit phase invocations without `-b`.
pub const DEFAULT_BLOCK: u64 = 1024 * 1024;

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
    Join(String),
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

/// Timed result of one pass (aggregated across all workers).
#[derive(Debug, Clone)]
pub struct PhaseResult {
    /// Which phase this row measures.
    pub phase: Phase,
    /// I/O block size this pass ran with.
    pub block: u64,
    /// Whether the pass used random (shuffled full-coverage) offsets.
    pub rand: bool,
    /// Whether the pass used O_DIRECT.
    pub direct: bool,
    /// Wall-clock time box the pass ran under (None = full coverage).
    pub time_box: Option<Duration>,
    /// Operations the full-coverage pass would perform; `ops < expected_ops`
    /// means the time box expired first (partial coverage — stated in the
    /// results row).
    pub expected_ops: u64,
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

    /// Fraction of the full-coverage op count this pass completed
    /// (1.0 unless a time box expired first).
    pub fn coverage(&self) -> f64 {
        if self.expected_ops == 0 {
            return 1.0;
        }
        self.ops as f64 / self.expected_ops as f64
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

fn open_options(phase: Phase, direct: bool) -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
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

fn write_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
    deadline: Option<Instant>,
) -> Result<WorkerOut, BenchError> {
    let nblocks = block_count(&shape);
    let mut out = WorkerOut::with_capacity((nblocks as usize).saturating_mul(shape.files));
    let dir = dataset_root(&mount).join(format!("t{tid}"));
    std::fs::create_dir_all(&dir)?;
    let mut buf = AlignedBuf::new(shape.block as usize, shape.direct);
    let mut expired = false;
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let mut file = open_options(Phase::Write, shape.direct).open(&path)?;
        let mut pos = 0u64;
        for b in block_order(shape.rand, nblocks) {
            let off = b * shape.block;
            let len = block_len(&shape, b);
            fill_block(&mut buf.as_mut_slice()[..len], tid, fid, b);
            let t0 = Instant::now();
            if pos != off {
                file.seek(SeekFrom::Start(off))?;
            }
            file.write_all(&buf.as_slice()[..len])?;
            out.lat_ns.push(t0.elapsed().as_nanos() as u64);
            pos = off + len as u64;
            out.ops += 1;
            out.bytes += len as u64;
            pb.inc(1);
            // Time box: checked AFTER each op so every worker completes at
            // least one operation (honest nonzero rows even under a zero
            // box).
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    expired = true;
                    break;
                }
            }
        }
        // -w is authoritative for the dataset shape: trim any stale tail
        // left by a previously larger dataset, then make it durable —
        // fsync is INSIDE the timed phase (honest durable write numbers).
        // On time-box expiry this still runs for the file in flight, so a
        // boxed overwrite pass leaves the dataset shape-valid.
        file.set_len(shape.size)?;
        file.sync_all()?;
        if expired {
            break;
        }
    }
    Ok(out)
}

fn read_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
    deadline: Option<Instant>,
) -> Result<WorkerOut, BenchError> {
    let nblocks = block_count(&shape);
    let mut out = WorkerOut::with_capacity((nblocks as usize).saturating_mul(shape.files));
    let mut buf = AlignedBuf::new(shape.block as usize, shape.direct);
    'files: for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let mut file = open_options(Phase::Read, shape.direct).open(&path)?;
        let mut pos = 0u64;
        for b in block_order(shape.rand, nblocks) {
            let off = b * shape.block;
            // Exact short read at EOF: the final block reads only the
            // partial tail when size % block != 0.
            let len = block_len(&shape, b);
            let t0 = Instant::now();
            if pos != off {
                file.seek(SeekFrom::Start(off))?;
            }
            file.read_exact(&mut buf.as_mut_slice()[..len])?;
            out.lat_ns.push(t0.elapsed().as_nanos() as u64);
            pos = off + len as u64;
            out.ops += 1;
            out.bytes += len as u64;
            pb.inc(1);
            // Time box: checked AFTER each op — every worker completes at
            // least one operation.
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    break 'files;
                }
            }
        }
    }
    Ok(out)
}

fn stat_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let mut out = WorkerOut::with_capacity(shape.files);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let t0 = Instant::now();
        std::fs::metadata(&path)?;
        out.lat_ns.push(t0.elapsed().as_nanos() as u64);
        out.ops += 1;
        pb.inc(1);
    }
    Ok(out)
}

fn del_worker(
    mount: PathBuf,
    tid: usize,
    shape: Shape,
    pb: ProgressBar,
) -> Result<WorkerOut, BenchError> {
    let mut out = WorkerOut::with_capacity(shape.files);
    for fid in 0..shape.files {
        let path = bench_file_path(&mount, tid, fid);
        let t0 = Instant::now();
        std::fs::remove_file(&path)?;
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

/// Human descriptor of a pass for progress bars and table rows:
/// data passes carry access + block ("Write seq 1m", "Read rand 4k"),
/// metadata passes just the label.
fn pass_desc(phase: Phase, block: u64, rand: bool) -> String {
    if phase.moves_data() {
        format!(
            "{} {} {}",
            phase.label(),
            if rand { "rand" } else { "seq" },
            fmt_size(block)
        )
    } else {
        phase.label().to_string()
    }
}

async fn run_one_pass(
    mount: &Path,
    pass: &Pass,
    mp: &MultiProgress,
) -> Result<PhaseResult, BenchError> {
    let phase = pass.phase;
    let shape = &pass.shape;
    let per_file_ops = if phase.moves_data() {
        block_count(shape)
    } else {
        1
    };
    let total_ops = per_file_ops * (shape.threads as u64) * (shape.files as u64);
    let pb = mp.add(ProgressBar::new(total_ops));
    pb.set_style(progress_style());
    pb.set_message(pass_desc(phase, shape.block, shape.rand));

    let start = Instant::now();
    // The wall-clock box starts with the pass clock; workers stop issuing
    // ops once it expires (each worker still completes >= 1 op).
    let deadline = pass.time_box.map(|d| start + d);
    // Parallelism = named OS threads (the rip-tokio-total sweep): the
    // syscall stream per worker is identical to the former task form;
    // the loader's concurrency vehicle is threads, stated per the
    // instrument-alignment law.
    let mut results: Vec<Result<WorkerOut, BenchError>> = Vec::with_capacity(shape.threads);
    std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(shape.threads);
        for tid in 0..shape.threads {
            let mount = mount.to_path_buf();
            let shape = shape.clone();
            let pb = pb.clone();
            joins.push(
                std::thread::Builder::new()
                    .name(format!("sqz-bench{tid}"))
                    .spawn_scoped(scope, move || match phase {
                        Phase::Write => write_worker(mount, tid, shape, pb, deadline),
                        Phase::Read => read_worker(mount, tid, shape, pb, deadline),
                        Phase::Stat => stat_worker(mount, tid, shape, pb),
                        Phase::Del => del_worker(mount, tid, shape, pb),
                    })
                    .expect("bench worker thread spawns"),
            );
        }
        for j in joins {
            results
                .push(j.join().unwrap_or_else(|_| {
                    Err(BenchError::Join("bench worker panicked".to_string()))
                }));
        }
    });

    let mut ops = 0u64;
    let mut bytes = 0u64;
    let mut lat_ns: Vec<u64> = Vec::with_capacity(total_ops as usize);
    for out in results {
        let out = out?;
        ops += out.ops;
        bytes += out.bytes;
        lat_ns.extend(out.lat_ns);
    }
    let elapsed = start.elapsed();
    pb.finish_with_message(format!(
        "{} done",
        pass_desc(phase, shape.block, shape.rand)
    ));

    if phase == Phase::Del {
        // Cleanup bookkeeping OUTSIDE the timed region: the timed ops are
        // the file unlinks; removing the (now-empty) t{tid} dirs and the
        // dataset root is not part of the measurement.
        std::fs::remove_dir_all(dataset_root(mount))?;
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
        block: shape.block,
        rand: shape.rand,
        direct: shape.direct,
        time_box: pass.time_box,
        expected_ops: total_ops,
        ops,
        bytes,
        elapsed,
        lat_min: Duration::from_nanos(lat_ns.first().copied().unwrap_or(0)),
        lat_avg: Duration::from_nanos(avg_ns),
        lat_p99: Duration::from_nanos(percentile_ns(&lat_ns, 99.0)),
        lat_max: Duration::from_nanos(lat_ns.last().copied().unwrap_or(0)),
    })
}

/// What a `squeezefs bench` invocation runs: the full saturation suite
/// (bare invocation — no phase flags) or an explicit phase set in the
/// fixed order write, read, stat, del.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchMode {
    /// Bare invocation: the full saturation suite over one auto-sized
    /// dataset — write seq 1m → read seq 1m → read rand 4k (time-boxed)
    /// → write rand 4k (time-boxed) → stat → del. All I/O passes
    /// O_DIRECT.
    Suite,
    /// Explicit phase flags, fixed order.
    Phases(Vec<Phase>),
}

/// Map phase flags to a [`BenchMode`]: no flags selects the full
/// saturation suite (the old write+read default is gone); any flags run
/// exactly those phases in the fixed order write, read, stat, del.
pub fn select_mode(write: bool, read: bool, stat: bool, del: bool) -> BenchMode {
    if !write && !read && !stat && !del {
        return BenchMode::Suite;
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
    BenchMode::Phases(phases)
}

/// The resolved (auto or explicit) run shape, with provenance markers so
/// the header can state which values were auto-sized.
#[derive(Debug, Clone)]
pub struct ResolvedShape {
    /// Worker count.
    pub threads: usize,
    /// Files per thread.
    pub files: usize,
    /// Per-file size in bytes.
    pub size: u64,
    /// I/O block size for explicit phase runs (the suite fixes its own
    /// per-pass block sizes).
    pub block: u64,
    /// `--threads` was auto-sized (not given on the command line).
    pub threads_auto: bool,
    /// `--files` was auto-sized.
    pub files_auto: bool,
    /// `--size` was auto-sized (statfs-derived).
    pub size_auto: bool,
    /// `--block` was defaulted.
    pub block_auto: bool,
}

/// Auto worker count: `min(available, 16)`, at least 1.
pub fn clamp_auto_threads(available: usize) -> usize {
    available.clamp(1, AUTO_THREADS_CAP)
}

/// Auto total dataset size: `max(16 GiB, 2 GiB × threads)`, capped at 25%
/// of the filesystem's free space and rounded down to a 1 MiB multiple.
/// Loud error when even [`AUTO_MIN_TOTAL_BYTES`] (4 GiB) does not fit
/// under the cap.
pub fn auto_total_bytes(threads: usize, free_bytes: u64) -> Result<u64, BenchError> {
    let desired = AUTO_TOTAL_FLOOR_BYTES.max(AUTO_PER_THREAD_BYTES.saturating_mul(threads as u64));
    let cap = (free_bytes / 4) / AUTO_SIZE_ROUND_BYTES * AUTO_SIZE_ROUND_BYTES;
    let total = desired.min(cap);
    if total < AUTO_MIN_TOTAL_BYTES {
        return Err(BenchError::Shape(format!(
            "auto-sizing refused: the filesystem has {} free and the bench caps its dataset \
             at 25% of free space ({} here), but even the 4 GiB minimum total does not fit. \
             Free up space or pass an explicit -s (and optionally -t/-n) sized for this \
             filesystem.",
            fmt_size(free_bytes),
            fmt_size(cap),
        )));
    }
    Ok(total)
}

/// Per-file size from a total: `total / (threads × files)` rounded down to
/// a 1 MiB multiple (so `-s % -b == 0` holds for both suite block sizes).
/// Loud error when that rounds to zero.
pub fn auto_file_size(total: u64, threads: usize, files: usize) -> Result<u64, BenchError> {
    let file_count = (threads as u64).saturating_mul(files as u64);
    if file_count == 0 {
        return Err(BenchError::Shape(
            "--threads and --files must be >= 1".to_string(),
        ));
    }
    let per_file = total / file_count / AUTO_SIZE_ROUND_BYTES * AUTO_SIZE_ROUND_BYTES;
    if per_file == 0 {
        return Err(BenchError::Shape(format!(
            "auto-sizing refused: {} total across {} file(s) leaves less than the 1 MiB \
             per-file rounding floor (auto sizes stay 1 MiB multiples so -s divides both \
             suite block sizes). Pass an explicit -s, or lower -t/-n.",
            fmt_size(total),
            file_count,
        )));
    }
    Ok(per_file)
}

/// Total bytes of an existing bench dataset under `mount` (0 when none).
/// Sums logical file sizes of everything under `squeezefs-bench/`.
pub fn dataset_bytes(mount: &Path) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let mut sum = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(ftype) = entry.file_type() {
                    if ftype.is_dir() {
                        sum += walk(&entry.path());
                    } else if let Ok(meta) = entry.metadata() {
                        sum += meta.len();
                    }
                }
            }
        }
        sum
    }
    walk(&dataset_root(mount))
}

/// Free space used for AUTO-SIZING: statvfs free plus the bytes of any
/// existing bench dataset. The dataset a previous `-w` wrote consumes
/// statfs free space; without adding it back, a cap-bound follow-up
/// single-phase run would re-derive a SMALLER shape than the dataset it
/// is supposed to reuse and refuse it — breaking the consistency rule
/// (same auto shape for every invocation over one dataset).
pub fn sizing_free_bytes(mount: &Path) -> Result<u64, BenchError> {
    Ok(mount_free_bytes(mount)?.saturating_add(dataset_bytes(mount)))
}

/// Free space (bytes available to unprivileged users) on the filesystem
/// holding `path`, via statvfs.
pub fn mount_free_bytes(path: &Path) -> Result<u64, BenchError> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        BenchError::Shape(format!(
            "path {} contains an interior NUL byte",
            path.display()
        ))
    })?;
    // SAFETY: `stat` is a plain-old-data out-param zeroed before the call;
    // `c_path` is a valid NUL-terminated C string that outlives the call.
    // statvfs writes the struct only on success (rc == 0).
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(BenchError::Io(std::io::Error::last_os_error()));
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Resolve explicit flags + auto defaults into a concrete run shape.
/// statvfs is consulted only when `-s` was not given.
pub fn resolve_shape(
    mount: &Path,
    threads: Option<usize>,
    files: Option<usize>,
    size: Option<u64>,
    block: Option<u64>,
) -> Result<ResolvedShape, BenchError> {
    let threads_auto = threads.is_none();
    let resolved_threads = threads.unwrap_or_else(|| {
        clamp_auto_threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        )
    });
    let files_auto = files.is_none();
    let resolved_files = files.unwrap_or(1);
    let block_auto = block.is_none();
    let resolved_block = block.unwrap_or(DEFAULT_BLOCK);
    let size_auto = size.is_none();
    let resolved_size = match size {
        Some(s) => s,
        None => {
            // Sizing free = statvfs free + any existing dataset's bytes:
            // re-deriving the auto shape over a dataset a previous -w
            // already wrote must reproduce that shape, not shrink it.
            let free = sizing_free_bytes(mount)?;
            let total = auto_total_bytes(resolved_threads, free)?;
            auto_file_size(total, resolved_threads, resolved_files)?
        }
    };
    Ok(ResolvedShape {
        threads: resolved_threads,
        files: resolved_files,
        size: resolved_size,
        block: resolved_block,
        threads_auto,
        files_auto,
        size_auto,
        block_auto,
    })
}

/// Resolve `--time` into a per-pass wall-clock box: explicit `N` caps the
/// pass at N seconds, explicit `0` forces full coverage, and the default
/// is 30 s for random passes / unlimited (full coverage) for sequential.
pub fn resolve_time_box(time: Option<u64>, rand: bool) -> Option<Duration> {
    match time {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None if rand => Some(Duration::from_secs(DEFAULT_RAND_TIME_BOX_SECS)),
        None => None,
    }
}

/// One executable pass: a phase plus the concrete shape, wall-clock box
/// and validate-before-timing flag it runs under.
#[derive(Debug, Clone)]
pub struct Pass {
    /// Which phase to run.
    pub phase: Phase,
    /// Concrete shape for this pass (dataset fields must match across the
    /// passes of one run; block/rand/direct may vary per pass).
    pub shape: Shape,
    /// Wall-clock time box (None = full coverage). Only read/write passes
    /// are boxed; stat/del always complete.
    pub time_box: Option<Duration>,
    /// Validate the existing dataset against the shape before this pass
    /// (never creates anything).
    pub validate: bool,
}

/// The full saturation suite over one dataset (threads × files × size):
/// write seq 1m direct → read seq 1m direct → read rand 4k direct
/// (time-boxed) → write rand 4k direct (time-boxed) → stat → del.
/// `time` overrides the rand passes' 30 s default box (`0` = full
/// coverage); sequential passes always run to completion — the dataset
/// lifecycle depends on it.
pub fn suite_passes(threads: usize, files: usize, size: u64, time: Option<u64>) -> Vec<Pass> {
    let io_shape = |block: u64, rand: bool| Shape {
        threads,
        files,
        size,
        block,
        rand,
        direct: true,
    };
    // Stat/del never open data-path fds; keep their shape access-neutral.
    let meta_shape = Shape {
        threads,
        files,
        size,
        block: SUITE_SEQ_BLOCK,
        rand: false,
        direct: false,
    };
    let rand_box = resolve_time_box(time, true);
    vec![
        Pass {
            phase: Phase::Write,
            shape: io_shape(SUITE_SEQ_BLOCK, false),
            time_box: None,
            validate: false,
        },
        Pass {
            phase: Phase::Read,
            shape: io_shape(SUITE_SEQ_BLOCK, false),
            time_box: None,
            validate: true,
        },
        Pass {
            phase: Phase::Read,
            shape: io_shape(SUITE_RAND_BLOCK, true),
            time_box: rand_box,
            validate: true,
        },
        Pass {
            phase: Phase::Write,
            shape: io_shape(SUITE_RAND_BLOCK, true),
            time_box: rand_box,
            validate: true,
        },
        Pass {
            phase: Phase::Stat,
            shape: meta_shape.clone(),
            time_box: None,
            validate: true,
        },
        Pass {
            phase: Phase::Del,
            shape: meta_shape,
            time_box: None,
            validate: true,
        },
    ]
}

/// Explicit phase flags → passes in the fixed order, all sharing `shape`.
/// Read/write passes get the `--time` box (rand default 30 s); when the
/// set lacks [`Phase::Write`] the first pass validates the dataset before
/// any timing (exactly the old `-r` semantics).
pub fn phase_passes(phases: &[Phase], shape: &Shape, time: Option<u64>) -> Vec<Pass> {
    let has_write = phases.contains(&Phase::Write);
    phases
        .iter()
        .enumerate()
        .map(|(i, &phase)| Pass {
            phase,
            shape: shape.clone(),
            time_box: if phase.moves_data() {
                resolve_time_box(time, shape.rand)
            } else {
                None
            },
            validate: i == 0 && !has_write,
        })
        .collect()
}

/// Execute a prepared pass sequence. All passes must share the dataset
/// fields (threads/files/size); block/rand/direct/time may vary per pass.
/// Pass validation (found-vs-expected, never creating) happens BEFORE that
/// pass's timing starts.
pub async fn run_passes(mount: &Path, passes: &[Pass]) -> Result<BenchReport, BenchError> {
    let mp = MultiProgress::new();
    let mut report = BenchReport::default();
    for pass in passes {
        validate_shape(&pass.shape)?;
        if pass.validate {
            validate_dataset(mount, &pass.shape)?;
        }
        report.phases.push(run_one_pass(mount, pass, &mp).await?);
    }
    Ok(report)
}

/// A parsed `squeezefs bench` command line: phase flags, optional shape
/// overrides (None = auto), access/time modifiers, iterations.
#[derive(Debug, Clone, Default)]
pub struct BenchInvocation {
    /// `-w`: write phase selected.
    pub write: bool,
    /// `-r`: read phase selected.
    pub read: bool,
    /// `--stat`: stat phase selected.
    pub stat: bool,
    /// `--del`: delete phase selected.
    pub del: bool,
    /// `-t` (None = auto: min(CPUs, 16)).
    pub threads: Option<usize>,
    /// `-n` (None = auto: 1).
    pub files: Option<usize>,
    /// `-s` in bytes (None = auto-sized from free space).
    pub size: Option<u64>,
    /// `-b` in bytes (None = 1m for explicit phases; suite fixes per-pass
    /// sizes and refuses an explicit `-b`).
    pub block: Option<u64>,
    /// `--rand` (explicit phase runs only; the suite fixes per-pass
    /// access and refuses the flag).
    pub rand: bool,
    /// `--direct` (suite I/O passes are always direct).
    pub direct: bool,
    /// `--time` seconds (None = default: 30 s rand / unlimited seq;
    /// 0 = force full coverage).
    pub time: Option<u64>,
    /// `-i` repetitions of the selected set.
    pub iterations: usize,
}

/// Execute one iteration of the given phases (already in fixed order) over
/// `mount` with `shape`. When the set does not include [`Phase::Write`],
/// the existing dataset is validated against the shape before any timing.
/// Read/write passes inherit the default time boxes (30 s for `--rand`,
/// unlimited for sequential) — see [`resolve_time_box`].
pub async fn run_phases(
    mount: &Path,
    phases: &[Phase],
    shape: &Shape,
) -> Result<BenchReport, BenchError> {
    run_passes(mount, &phase_passes(phases, shape, None)).await
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

/// `(auto)` / `(explicit)` provenance marker for the header.
fn mark(auto: bool) -> &'static str {
    if auto {
        "(auto)"
    } else {
        "(explicit)"
    }
}

fn fmt_time_box(b: Option<Duration>) -> String {
    match b {
        Some(d) => format!("{}s", d.as_secs()),
        None => "none (full coverage)".to_string(),
    }
}

/// Print the loud effective-shape header: mode, computed shape with
/// auto-vs-explicit provenance, per-pass access/io/time-box facts.
fn print_header(mount: &Path, mode: &BenchMode, resolved: &ResolvedShape, inv: &BenchInvocation) {
    let sep = "==================================================================================";
    println!("{}", sep.bold());
    match mode {
        BenchMode::Suite => {
            println!(
                "  SqueezeFS Bench @ {} — FULL SATURATION SUITE",
                mount.display()
            );
            println!(
                "  passes: write seq {seq} -> read seq {seq} -> read rand {rnd} -> write rand {rnd} -> stat -> del (I/O passes O_DIRECT)",
                seq = fmt_size(SUITE_SEQ_BLOCK),
                rnd = fmt_size(SUITE_RAND_BLOCK),
            );
            println!(
                "  threads={} {} files/thread={} {} size={}/file ({} B; total {}) {}",
                resolved.threads,
                mark(resolved.threads_auto),
                resolved.files,
                mark(resolved.files_auto),
                fmt_size(resolved.size),
                resolved.size,
                fmt_size(resolved.size * resolved.threads as u64 * resolved.files as u64),
                mark(resolved.size_auto),
            );
            println!(
                "  block=1m seq / 4k rand (suite) io=direct (suite) rand time-box={} {} iterations={}",
                fmt_time_box(resolve_time_box(inv.time, true)),
                mark(inv.time.is_none()),
                inv.iterations
            );
        }
        BenchMode::Phases(phases) => {
            let phase_list = phases
                .iter()
                .map(|p| p.label().to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "  SqueezeFS Bench @ {} — phases: {}",
                mount.display(),
                phase_list
            );
            println!(
                "  threads={} {} files/thread={} {} size={} ({} B) {} block={} ({} B) {}",
                resolved.threads,
                mark(resolved.threads_auto),
                resolved.files,
                mark(resolved.files_auto),
                fmt_size(resolved.size),
                resolved.size,
                mark(resolved.size_auto),
                fmt_size(resolved.block),
                resolved.block,
                mark(resolved.block_auto),
            );
            println!(
                "  access={} io={} time-box={} {} iterations={}",
                if inv.rand { "rand" } else { "seq" },
                if inv.direct { "direct" } else { "buffered" },
                fmt_time_box(resolve_time_box(inv.time, inv.rand)),
                mark(inv.time.is_none()),
                inv.iterations
            );
        }
    }
    let uses_direct = match mode {
        BenchMode::Suite => true,
        BenchMode::Phases(_) => inv.direct,
    };
    if uses_direct {
        print_direct_posture(mount);
    }
    println!("{}", sep.bold());
}

/// Hybrid I/O (user directive 2026-07-15): O_DIRECT reads serve from and
/// admit into the SqueezeFS read tiers by default, so `--direct` rows on a
/// default mount measure the HYBRID posture (tier serves once warm), not
/// the raw device path. The amplification/device-path methodology keeps
/// its ruler via the mount-scoped escape: `-o direct_device_true` (or
/// `SQUEEZEFS_DIRECT_DEVICE_TRUE=1` on the daemon). The bench sniffs the
/// mount's `.stats` inode and prints which ruler the rows carry — a
/// measurement note, never a gate (non-SqueezeFS mounts read as unknown).
fn print_direct_posture(mount: &Path) {
    let posture = std::fs::read_to_string(mount.join(".stats"))
        .ok()
        .map(|s| {
            if s.contains("\"direct_device_true\": true")
                || s.contains("\"direct_device_true\":true")
            {
                "device-true (-o direct_device_true): O_DIRECT rows measure the DEVICE path"
            } else {
                "hybrid (default): O_DIRECT rows may serve from the read tiers once warm; \
                 mount with -o direct_device_true for device-path/amplification measurement"
            }
        })
        .unwrap_or("unknown (.stats not readable — not a SqueezeFS v3 mount root?)");
    println!("  direct-I/O posture: {posture}");
}

fn fmt_coverage(res: &PhaseResult) -> String {
    let pct = res.coverage() * 100.0;
    if pct >= 99.95 {
        "100%".to_string()
    } else if pct >= 10.0 {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

fn print_report(report: &BenchReport) {
    let hline = "+------------------+------------------+------------------+------+------------+------------+------------+------------+";
    println!("\n{}", hline.bold());
    println!(
        "| {:<16} | {:<16} | {:<16} | {:<4} | {:<10} | {:<10} | {:<10} | {:<10} |",
        "PASS", "THROUGHPUT", "IOPS", "COV", "LAT MIN", "LAT AVG", "LAT P99", "LAT MAX"
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
            "| {:<16} | {:>16} | {:>16} | {:>4} | {:>10} | {:>10} | {:>10} | {:>10} |",
            pass_desc(res.phase, res.block, res.rand),
            tput_str,
            iops_str,
            fmt_coverage(res),
            fmt_latency(res.lat_min),
            fmt_latency(res.lat_avg),
            fmt_latency(res.lat_p99),
            fmt_latency(res.lat_max)
        );
    }
    println!("{}", hline.bold());
    for res in &report.phases {
        if res.coverage() < 1.0 - f64::EPSILON {
            println!(
                "  note: {} covered {} of {} ops ({}) — wall-clock time box {}",
                pass_desc(res.phase, res.block, res.rand),
                res.ops,
                res.expected_ops,
                fmt_coverage(res),
                fmt_time_box(res.time_box),
            );
        }
    }
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

/// CLI entry point behind [`BenchInvocation`]: resolves the mode and the
/// (auto or explicit) shape, prints the loud provenance header, then runs
/// `iterations` repetitions of the selected pass set — the full
/// saturation suite for a bare invocation, the explicit phases otherwise
/// — with a per-iteration results table and daemon `.stats` metrics
/// delta.
pub async fn run_invocation(mount: &Path, inv: &BenchInvocation) -> Result<(), BenchError> {
    if inv.iterations == 0 {
        return Err(BenchError::Shape("--iterations must be >= 1".to_string()));
    }
    let mode = select_mode(inv.write, inv.read, inv.stat, inv.del);
    if mode == BenchMode::Suite {
        // The suite fixes per-pass access and block sizes (1m seq + 4k
        // rand); a shape override that cannot apply is a loud error, never
        // a silent ignore.
        if inv.block.is_some() {
            return Err(BenchError::Shape(
                "-b applies to explicit phase runs; the bare invocation runs the fixed \
                 saturation suite (1m seq + 4k rand passes). Select phases \
                 (-w/-r/--stat/--del) to use -b."
                    .to_string(),
            ));
        }
        if inv.rand {
            return Err(BenchError::Shape(
                "--rand applies to explicit phase runs; the bare invocation runs the fixed \
                 saturation suite (which already includes rand 4k passes). Select phases \
                 (-w/-r/--stat/--del) to use --rand."
                    .to_string(),
            ));
        }
    }

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

    let resolved = resolve_shape(mount, inv.threads, inv.files, inv.size, inv.block)?;
    let passes = match &mode {
        BenchMode::Suite => suite_passes(resolved.threads, resolved.files, resolved.size, inv.time),
        BenchMode::Phases(phases) => {
            let shape = Shape {
                threads: resolved.threads,
                files: resolved.files,
                size: resolved.size,
                block: resolved.block,
                rand: inv.rand,
                direct: inv.direct,
            };
            phase_passes(phases, &shape, inv.time)
        }
    };
    // Fail loud on shape/alignment problems before any warning noise or
    // timing.
    for pass in &passes {
        validate_shape(&pass.shape)?;
    }

    warn_if_not_squeezefs_mount(mount);
    print_header(mount, &mode, &resolved, inv);

    for iter in 1..=inv.iterations {
        if inv.iterations > 1 {
            println!("\n--- Benchmark Iteration {iter}/{}---", inv.iterations);
        }
        // Daemon metrics come straight from `.stats`; don't gate them on
        // the `.config`-based mount detection. A readable/parseable
        // `.stats` is sufficient, so metrics still show even when
        // `.config` is momentarily unreadable.
        let baseline = get_daemon_metrics_from_stats(mount);
        let report = run_passes(mount, &passes).await?;
        let post = get_daemon_metrics_from_stats(mount);
        print_report(&report);
        print_metrics_delta(baseline, post);
    }
    Ok(())
}
