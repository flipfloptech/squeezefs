//! Process-wide generic file I/O via a pool of pipelined `io_uring` workers (P2-8).
//!
//! Use this for **path-based** reads/writes/fsync that are not the primary
//! block device (MetaLV sector + WAL I/O, GDS cache files, ad-hoc local
//! files). The primary block path remains [`crate::nvme_dev::NvmeBlockDev`]
//! (also io_uring).
//!
//! # Pipelined workers
//!
//! Each pool worker owns one ring and keeps **many operations in flight**:
//! requests are admitted from the shared queue in bursts, submitted together,
//! and completions are reaped as they arrive, with short read/write
//! continuations resubmitted from the completion handler. The previous
//! model (`submit_and_wait(1)` per request — one op in flight per worker,
//! two thread handoffs per 4 KiB sector) capped process-wide metadata I/O
//! at pool size and burned ~27% of daemon cycles in queue churn under
//! delete storms.
//!
//! [`write_at_batch`] lets one logical commit (WAL record + sector images)
//! travel as a single queue message that fans out into parallel SQEs.
//!
//! Staging/read-segment **mmap** paths intentionally stay on `mmap` — they are
//! already zero-syscall for the hot get/put path; flushing uses optional
//! `fdatasync` through this worker when requested.
//!
//! **Not** routed through uring (and should not be without a dedicated stack):
//! Garnet/Redis TCP, TLS/mTLS peer traffic, directory create/remove metadata.

use crate::error::{Result, SqueezefsError};
use once_cell::sync::Lazy;
use squeezefs_ipc::sqz_channel::oneshot;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// Per-worker request-queue bound — backpressure, not custody (the P1-6
/// class: small request structs; payload bytes live with the caller
/// until service). Same posture as `nvme_dev::URING_REQ_QUEUE_CAP` —
/// filed in the derivation sweep's backpressure-bounds row (scale with
/// offered concurrency; pending its saturation sweep, the shipped bound
/// stands).
const URING_FS_QUEUE_CAP: usize = 4096;
/// Ring SQ depth per worker.
const RING_ENTRIES: u32 = 512;
/// Max operations a worker keeps in flight; continuations reuse their slot,
/// so SQ pressure is bounded by this plus one burst of resubmits.
const ADMIT_CAP: usize = 256;
/// Open-file cache entries per worker: bounded by the process fd limit so
/// the pool can never EMFILE the process by itself (the pool's aggregate
/// cache stays under a quarter of RLIMIT_NOFILE), capped at 1024, floored
/// at 16. Production metadata I/O touches only a handful of distinct paths;
/// churny workloads simply re-open. The limit is the soft `RLIMIT_NOFILE`
/// **as found at startup** ([`crate::cpu::nofile_soft_as_found`]) — the
/// daemon's startup raise to the hard limit serves the cluster-wire
/// listener caps alone, so this cache is the same size on every posture
/// as before the raise existed (PR 13c review round 1, Issue 8).
fn fd_cache_cap() -> usize {
    static CAP: Lazy<usize> =
        Lazy::new(|| fd_cache_cap_from(crate::cpu::nofile_soft_as_found(), worker_count()));
    *CAP
}

/// The pure form of the per-worker open-file cache cap (`fd_cache_cap`):
/// `soft / 4 / workers`, clamped `16..=1024` — the derivation the tie test
/// reads.
pub fn fd_cache_cap_from(nofile_soft: usize, workers: usize) -> usize {
    (nofile_soft / 4 / workers.max(1)).clamp(16, 1024)
}

/// The cap in force for this process (the `Lazy` word) and the worker
/// count it derived from — the tie test's live face.
pub fn fd_cache_cap_in_force() -> (usize, usize) {
    (fd_cache_cap(), worker_count())
}

/// A write's terminal outcome plus the two worker-side instants the
/// completion-hop decomposition reads (`uring_fs_write_phase_ns`, e2e
/// audit C-2): the payload rides the oneshot the request already
/// allocates, so the stamps cost no allocation and no extra channel.
pub struct WriteOutcome {
    pub result: Result<()>,
    /// The worker took the request off the queue and pushed its SQE(s)
    /// (the queue hop's end / the device span's start). For an outcome
    /// decided at admission (fault shim, open failure) this equals
    /// `woken_at`.
    pub admitted_at: std::time::Instant,
    /// The worker reaped the request's (last) CQE and sent this outcome —
    /// the completion's wake fired (the device span's end / the wake
    /// hop's start).
    pub woken_at: std::time::Instant,
}

type WriteTx = oneshot::Sender<WriteOutcome>;

/// Send a write outcome decided NOW (no worker admission/reap happened:
/// fault-shim refusals, open failures, fallback-path completions).
fn send_now(tx: WriteTx, result: Result<()>) {
    let now = std::time::Instant::now();
    let _ = tx.send(WriteOutcome {
        result,
        admitted_at: now,
        woken_at: now,
    });
}

enum FsReq {
    WriteAll {
        path: PathBuf,
        data: bytes::Bytes,
        tx: WriteTx,
    },
    ReadAll {
        path: PathBuf,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    ReadAt {
        path: PathBuf,
        offset: u64,
        size: usize,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    WriteAt {
        path: PathBuf,
        offset: u64,
        data: bytes::Bytes,
        tx: WriteTx,
    },
    /// One logical commit: every `(offset, bytes)` lands (unordered between
    /// entries) before the single completion fires.
    WriteAtBatch {
        path: PathBuf,
        ops: Vec<(u64, bytes::Bytes)>,
        tx: WriteTx,
    },
    Fdatasync {
        path: PathBuf,
        tx: oneshot::Sender<Result<()>>,
    },
    /// A request the fault shim's latency lane re-admits after holding it
    /// for the armed device latency ([`arm_device_latency`]): admission
    /// skips the latency arm for it (every other fault still applies).
    Delayed(Box<FsReq>),
}

struct UringFsWorker {
    tx: Option<crossbeam::channel::Sender<FsReq>>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl UringFsWorker {
    fn new() -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_FS_QUEUE_CAP);
        // Pool of pipelined workers sharing one MPMC queue: a worker blocked
        // on a device flush never stalls the others, and each worker keeps up
        // to ADMIT_CAP operations in flight on its own ring.
        let count = worker_count();
        let mut threads = Vec::with_capacity(count);
        for i in 0..count {
            let rx = rx.clone();
            let t = std::thread::Builder::new()
                .name(format!("squeezefs-uring-fs-{i}"))
                .spawn(move || worker_loop(rx))
                .expect("spawn uring-fs worker");
            threads.push(t);
        }
        Self {
            tx: Some(tx),
            _threads: threads,
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<FsReq> {
        self.tx.as_ref().expect("uring-fs worker sender")
    }
}

/// Size of the io_uring file-worker pool. Override with
/// `SQUEEZEFS_URING_FS_WORKERS` (clamp 1..=64); defaults to
/// `clamp(cpus/4, 4, 64)`.
fn worker_count() -> usize {
    resolve_worker_count(
        std::env::var("SQUEEZEFS_URING_FS_WORKERS").ok().as_deref(),
        crate::cpu::process_parallelism(),
    )
}

/// Pure sizing form (2026-08-04 derivation sweep; pinned by
/// `tests/derivation_sweep_tests.rs`): env wins verbatim (clamp 1..=64,
/// the pre-existing operator sanity clamp); derived default =
/// `clamp(cpus/4, 4, 64)` — cpus/4 is the measured drain-thread slope
/// (the ingest-economy `il_sessions_default` precedent applied to the
/// sibling file-I/O pool; the retired `clamp(nproc, 4, 8)` pinned every
/// box ≥ 8 CPUs at 8 workers with no basis for the 8), floor 4 = the
/// shipped floor, ceiling 64 = env-clamp parity. `cpus` must be the
/// PROCESS parallelism ([`crate::cpu::process_parallelism`]) — the old
/// site read `available_parallelism()` from whatever thread touched the
/// Lazy first (the Hang-1 pinned-first-toucher sizing poison).
pub fn resolve_worker_count(env: Option<&str>, cpus: usize) -> usize {
    if let Some(raw) = env {
        if let Ok(n) = raw.trim().parse::<usize>() {
            return n.clamp(1, 64);
        }
    }
    (cpus / 4).clamp(4, 64)
}

impl Drop for UringFsWorker {
    fn drop(&mut self) {
        // Close the channel so every worker sees a recv error and exits.
        drop(self.tx.take());
        for h in self._threads.drain(..) {
            let _ = h.join();
        }
    }
}

static URING_FS: Lazy<Arc<UringFsWorker>> = Lazy::new(|| Arc::new(UringFsWorker::new()));

fn queue_full_err<E: std::fmt::Debug>(e: E) -> SqueezefsError {
    crate::fuse_client::METRICS
        .uring_queue_full
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SqueezefsError::InvalidOperation(format!("uring-fs queue full: {e:?}"))
}

fn worker_closed_err<E: std::fmt::Debug>(e: E) -> SqueezefsError {
    SqueezefsError::InvalidOperation(format!("uring-fs worker closed: {e:?}"))
}

/// Write `data` to `path` (create/truncate) via the process io_uring file worker.
pub async fn write_all(path: impl AsRef<Path>, data: impl Into<bytes::Bytes>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::WriteAll {
            path: path.as_ref().to_path_buf(),
            data: data.into(),
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?.result
}

/// Read entire file via the process io_uring file worker.
pub async fn read_all(path: impl AsRef<Path>) -> Result<bytes::Bytes> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::ReadAll {
            path: path.as_ref().to_path_buf(),
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
}

/// `fdatasync` an existing path via io_uring.
pub async fn fdatasync(path: impl AsRef<Path>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::Fdatasync {
            path: path.as_ref().to_path_buf(),
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
}

/// Read `size` bytes at `offset` from `path` via io_uring.
pub async fn read_at(path: impl AsRef<Path>, offset: u64, size: usize) -> Result<bytes::Bytes> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::ReadAt {
            path: path.as_ref().to_path_buf(),
            offset,
            size,
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
}

/// Write `data` at `offset` to `path` via io_uring.
pub async fn write_at(
    path: impl AsRef<Path>,
    offset: u64,
    data: impl Into<bytes::Bytes>,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::WriteAt {
            path: path.as_ref().to_path_buf(),
            offset,
            data: data.into(),
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?.result
}

// ---------------------------------------------------------------------------
// The write completion-hop decomposition (`uring_fs_write_phase_ns`, e2e
// audit C-2 — `docs/design-e2e-perf-audit.md` §3 DLM board #3).
//
// D-2 isolated the journal ring write's round trip as THE term on the
// co-located fleet: a ~1 KiB page-cache write reading 1.0–1.3 ms MEAN with
// a 50–200 µs MODE (`meta_txpass_phase_ns.journal_ring_write`). That span
// is `submit → observed`; this family splits it at the two thread
// boundaries a pooled submission crosses, exact-sum, always-on, zero
// allocation (the stamps ride the request's own oneshot payload):
//
//   queue_hop : submit (the caller's queue push) → the worker admitted it
//               (popped + SQEs pushed) — the caller→worker wake + queue
//               residence;
//   device    : admitted → the worker reaped its last CQE and sent the
//               outcome — the kernel write (an io-wq punt for a buffered
//               block-device write) + the worker's own wake out of the
//               ring wait;
//   wake_hop  : outcome sent → the awaiting task OBSERVED it — the
//               oneshot's waker → the task's lane queue → that lane
//               thread's dispatch (the run-queue hop the finding names);
//   total     : submit → observed (≡ `journal_ring_write` on the conveyor).
// ---------------------------------------------------------------------------

/// Phases of [`uring_fs_write_phase_json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum UringFsWritePhase {
    QueueHop = 0,
    Device = 1,
    WakeHop = 2,
    Total = 3,
}

const URING_FS_WRITE_PHASES: usize = 4;
const URING_FS_WRITE_PHASE_NAMES: [&str; URING_FS_WRITE_PHASES] =
    ["queue_hop", "device", "wake_hop", "total"];

static URING_FS_WRITE_PROF: Lazy<[crate::fuse_client::LatencyHistogram; URING_FS_WRITE_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| crate::fuse_client::LatencyHistogram::default()));

/// `uring_fs_write_phase_ns` stats payload — surfaced UNGATED.
pub fn uring_fs_write_phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (pi, pname) in URING_FS_WRITE_PHASE_NAMES.iter().enumerate() {
        phases.insert((*pname).to_string(), URING_FS_WRITE_PROF[pi].to_json());
    }
    serde_json::Value::Object(phases)
}

/// Exact `(sum_ns, count)` of one phase (the in-process harness's
/// instrument; the stats inode carries the same words as JSON).
pub fn uring_fs_write_phase_totals(phase: UringFsWritePhase) -> (u64, u64) {
    let h = &URING_FS_WRITE_PROF[phase as usize];
    (h.sum_ns(), h.count())
}

/// The four instants of one submitted write, retained by
/// [`WriteCompletion`] once the outcome is observed (the op-trace stamps
/// `ufs_submit` / `ufs_admit` / `ufs_wake` / `ufs_observed`).
#[derive(Debug, Clone, Copy)]
pub struct WriteStamps {
    pub submitted_at: std::time::Instant,
    pub admitted_at: std::time::Instant,
    pub woken_at: std::time::Instant,
    pub observed_at: std::time::Instant,
}

impl WriteStamps {
    /// Stamp the four instants onto every traced member of `traced`.
    pub fn stamp(&self, traced: &crate::op_trace::TracedBatch) {
        traced.stamp(crate::op_trace::Stage::UfsSubmit, self.submitted_at);
        traced.stamp(crate::op_trace::Stage::UfsAdmit, self.admitted_at);
        traced.stamp(crate::op_trace::Stage::UfsWake, self.woken_at);
        traced.stamp(crate::op_trace::Stage::UfsObserved, self.observed_at);
    }
}

/// Record one observed completion's decomposition. `submitted_at` is
/// the caller's queue push; the outcome carries the worker's two instants;
/// `observed_at` is read HERE — the one clock read the observer pays.
fn record_write_hops(submitted_at: std::time::Instant, out: &WriteOutcome) -> WriteStamps {
    let observed_at = std::time::Instant::now();
    let prof = &*URING_FS_WRITE_PROF;
    prof[UringFsWritePhase::QueueHop as usize]
        .record(out.admitted_at.saturating_duration_since(submitted_at));
    prof[UringFsWritePhase::Device as usize]
        .record(out.woken_at.saturating_duration_since(out.admitted_at));
    prof[UringFsWritePhase::WakeHop as usize]
        .record(observed_at.saturating_duration_since(out.woken_at));
    prof[UringFsWritePhase::Total as usize]
        .record(observed_at.saturating_duration_since(submitted_at));
    WriteStamps {
        submitted_at,
        admitted_at: out.admitted_at,
        woken_at: out.woken_at,
        observed_at,
    }
}

/// A write handed to the worker pool whose completion has not been
/// awaited yet ([`submit_write_at_batch`]). Submission and completion are
/// separable on purpose: the commit conveyor's apply stage SUBMITS a
/// batch's journal entries and hands this to its durability stage, which
/// awaits it — the device's completion latency never sits inside the
/// serialized apply server (D-2, e2e audit DLM #2).
pub struct WriteCompletion {
    rx: oneshot::Receiver<WriteOutcome>,
    /// The caller's queue push (`ufs_submit`).
    submitted_at: std::time::Instant,
    /// The outcome once observed (by [`Self::poll_done`] or the await),
    /// so a completion probed non-blockingly is never lost before
    /// [`Self::wait`]; the decomposition is recorded at that first
    /// observation.
    done: Option<(Result<()>, WriteStamps)>,
}

impl WriteCompletion {
    fn observe(&mut self, out: WriteOutcome) {
        let stamps = record_write_hops(self.submitted_at, &out);
        self.done = Some((out.result, stamps));
    }

    /// Non-blocking probe: `true` once the write has completed (its
    /// outcome is retained for [`Self::wait`]).
    pub fn poll_done(&mut self) -> bool {
        if self.done.is_some() {
            return true;
        }
        match self.rx.try_recv() {
            Ok(out) => {
                self.observe(out);
                true
            }
            Err(oneshot::TryRecvError::Closed) => {
                let now = std::time::Instant::now();
                self.observe(WriteOutcome {
                    result: Err(worker_closed_err("completion sender dropped")),
                    admitted_at: now,
                    woken_at: now,
                });
                true
            }
            Err(oneshot::TryRecvError::Empty) => false,
        }
    }

    /// The submission instant.
    pub fn submitted_at(&self) -> std::time::Instant {
        self.submitted_at
    }

    /// Await the write's outcome.
    pub async fn wait(self) -> Result<()> {
        self.wait_stamped().await.0
    }

    /// Await the write's outcome and hand back its four instants (the
    /// conveyor's durability lane stamps them onto its traced members).
    pub async fn wait_stamped(mut self) -> (Result<()>, WriteStamps) {
        if let Some(done) = self.done.take() {
            return done;
        }
        match (&mut self.rx).await {
            Ok(out) => self.observe(out),
            Err(e) => {
                let now = std::time::Instant::now();
                self.observe(WriteOutcome {
                    result: Err(worker_closed_err(e)),
                    admitted_at: now,
                    woken_at: now,
                });
            }
        }
        self.done.take().expect("observed above")
    }
}

/// Submit every `(offset, bytes)` pair to `path` as **one** worker message
/// (parallel SQEs on one ring; any failure fails the whole batch) and
/// return its completion to await later. Entry order is not an ordering
/// guarantee — callers needing order between batches issue separate
/// submissions. An empty batch is already complete.
pub fn submit_write_at_batch(
    path: impl AsRef<Path>,
    ops: Vec<(u64, bytes::Bytes)>,
) -> Result<WriteCompletion> {
    let (tx, rx) = oneshot::channel();
    let submitted_at = std::time::Instant::now();
    let mut ops = ops;
    let req = match ops.len() {
        0 => {
            send_now(tx, Ok(()));
            return Ok(WriteCompletion {
                rx,
                submitted_at,
                done: None,
            });
        }
        // A single op rides the single-op request (the shape it always
        // had — the worker's one-slot path, and the fault shim's single-
        // op arms).
        1 => {
            let (offset, data) = ops.pop().expect("one op");
            FsReq::WriteAt {
                path: path.as_ref().to_path_buf(),
                offset,
                data,
                tx,
            }
        }
        _ => FsReq::WriteAtBatch {
            path: path.as_ref().to_path_buf(),
            ops,
            tx,
        },
    };
    URING_FS.sender().try_send(req).map_err(queue_full_err)?;
    Ok(WriteCompletion {
        rx,
        submitted_at,
        done: None,
    })
}

/// [`submit_write_at_batch`] + await: the call completes when every entry
/// has landed.
pub async fn write_at_batch(path: impl AsRef<Path>, ops: Vec<(u64, bytes::Bytes)>) -> Result<()> {
    submit_write_at_batch(path, ops)?.wait().await
}

fn map_io(e: std::io::Error) -> SqueezefsError {
    SqueezefsError::Io(e)
}

// ---------------------------------------------------------------------------
// Shared-LUN metadata devices — O_DIRECT (PR 13i F-C1; design-symmetric-
// metadata §5.12).
//
// Every metadata read and write used to ride the issuing HOST's block-
// device page cache. One box is one cache, which is every venue the
// program had run on; two hosts over nvme-tcp are two caches of one LUN,
// and the cloud row's joiner read its kernel's stale image of a page the
// manager had rewritten (its appender page's grant word, tree 0, ring 0).
// The law (GPFS's / Lustre's shared-LUN rule): a REGISTERED metadata
// device path is opened `O_DIRECT` by every worker — every read of a
// block another host may write bypasses the cache, and every write
// reaches the device before it completes, which is also what makes the
// DUR barrier's `fdatasync` honest (it flushes the device's cache, not a
// host cache that never received the write).
//
// `O_DIRECT` needs `(offset, length, buffer)` aligned to the device's
// logical block size — DERIVED per path (`statx(STATX_DIOALIGN)`, the
// block device's sysfs `logical_block_size`, 4096 when neither answers),
// never assumed. The worker gives every shape its aligned form:
//   * reads: the span is widened to the grain into an aligned buffer and
//     the caller's window is a refcounted slice of it (no copy);
//   * writes: offset and length MUST be aligned — a misaligned direct
//     write is a CALLER bug refused loud (`meta_io_unaligned_refusals`,
//     must-stay-0), never a silent RMW; an unaligned BUFFER is bounced
//     into an aligned copy (`meta_io_bounce_bytes` — the KV layer builds
//     its images aligned so the hot paths pay none).
// The KV layer's own aligned forms (the journal ring's sector pad, the
// node append's tail-sector rewrite from its RAM image) live beside the
// shapes they align — `journal.rs`, `node.rs` — and read the grain here.
//
// The buffered path survives for exactly one venue: a REGULAR FILE on a
// filesystem that refuses `O_DIRECT` (tmpfs — the test sandboxes), a
// fallback taken LOUD and counted (`meta_io_buffered_fallback`, must-
// stay-0 on any block device), never on a block device, never silently;
// and for the red pin's control arm through the harness seam
// `SQUEEZEFS_TEST_META_BUFFERED=1`.
// ---------------------------------------------------------------------------

/// The I/O posture of one registered metadata device path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaIoMode {
    /// The alignment grain in bytes every direct read span, write offset
    /// and write length obey — the device's logical block size. `1` on
    /// the buffered posture (nothing to align; the KV layer pads nothing).
    pub grain: u64,
    /// Buffer alignment for direct I/O (statx's `stx_dio_mem_align`, else
    /// the grain).
    pub mem_align: usize,
    /// `true` = `O_DIRECT`; `false` = the buffered fallback (a regular
    /// file whose filesystem refuses direct I/O, or the harness seam).
    pub direct: bool,
}

impl MetaIoMode {
    /// The buffered posture: the pre-PR-13i behaviour verbatim.
    pub const BUFFERED: MetaIoMode = MetaIoMode {
        grain: 1,
        mem_align: 1,
        direct: false,
    };
}

/// Registered metadata device paths → their posture. Keyed by the path
/// as the KV layer names it AND by its canonical form, so every worker's
/// open — whichever spelling reaches it — finds the same answer.
static META_IO_MODES: Lazy<std::sync::RwLock<HashMap<PathBuf, MetaIoMode>>> =
    Lazy::new(|| std::sync::RwLock::new(HashMap::new()));

/// Metadata device paths registered `O_DIRECT` (`meta_io_direct_paths`).
pub static META_IO_DIRECT_PATHS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Registered metadata paths that fell back to BUFFERED I/O — a regular
/// file on a filesystem refusing `O_DIRECT`, or the harness seam
/// (`meta_io_buffered_fallback`; must-stay-0 on any block device, where
/// the fallback is refused instead).
pub static META_IO_BUFFERED_FALLBACK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Bytes of direct-write buffers COPIED into an aligned buffer because
/// the caller's was not (`meta_io_bounce_bytes`): the journal ring's runs
/// are built aligned (`AlignedBuf`), the page / node / ledger images are
/// heap `Vec`s and bounce — one copy per such write, the economy item
/// PR 14 inherits (the hot conveyor path pays none).
pub static META_IO_BOUNCE_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Direct writes REFUSED for a misaligned offset or length
/// (`meta_io_unaligned_refusals`, must-stay-0): a caller bug — the
/// aligned form belongs beside the shape, never to a silent RMW here.
pub static META_IO_UNALIGNED_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Direct reads whose caller window was widened to the grain
/// (`meta_io_read_widened`) — the cold path's bounce-free slice; the
/// hot metadata reads are RAM-authoritative (the node cache).
pub static META_IO_READ_WIDENED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The grain every metadata volume pads to when the device does not
/// say: the largest logical block size in the field (4Kn), so an
/// unknown device is never under-aligned.
pub const META_IO_DEFAULT_GRAIN: u64 = 4096;

/// The harness seam: `SQUEEZEFS_TEST_META_BUFFERED=1` keeps every
/// registered metadata path BUFFERED — the red pin's control arm.
fn test_meta_buffered() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_TEST_META_BUFFERED", false)
}

/// The posture of `path`, if registered.
pub fn meta_io_mode(path: &Path) -> Option<MetaIoMode> {
    let modes = META_IO_MODES.read().unwrap_or_else(|e| e.into_inner());
    modes.get(path).copied().or_else(|| {
        std::fs::canonicalize(path)
            .ok()
            .and_then(|c| modes.get(&c).copied())
    })
}

/// The alignment grain the KV layer's aligned forms use for `path`: the
/// registered posture's, `1` (no padding, the pre-PR-13i arithmetic) for
/// an unregistered path.
pub fn meta_io_grain(path: &Path) -> u64 {
    meta_io_mode(path).map_or(1, |m| m.grain)
}

/// `(offset_align, mem_align)` from `statx(STATX_DIOALIGN)`, `None` when
/// the kernel or filesystem does not report it.
fn statx_dio_align(path: &Path) -> Option<(u64, usize)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a zeroed `statx` out-buffer of the libc struct's size; the
    // path is a valid NUL-terminated C string; flags/mask are constants.
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_DIOALIGN,
            &mut st,
        )
    };
    if rc != 0 || (st.stx_mask & libc::STATX_DIOALIGN) == 0 || st.stx_dio_offset_align == 0 {
        return None;
    }
    Some((
        u64::from(st.stx_dio_offset_align),
        st.stx_dio_mem_align.max(1) as usize,
    ))
}

/// A block device's logical block size off sysfs (`/sys/dev/block/M:m/
/// queue/logical_block_size`, the partition's parent when the node is a
/// partition), `None` when unreadable.
fn sysfs_logical_block_size(path: &Path) -> Option<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(path).ok()?;
    if !md.file_type().is_block_device() {
        return None;
    }
    let rdev = md.rdev();
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    let dev = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    for candidate in [
        dev.join("queue/logical_block_size"),
        dev.join("../queue/logical_block_size"),
    ] {
        if let Ok(s) = std::fs::read_to_string(&candidate) {
            if let Ok(v) = s.trim().parse::<u64>() {
                if v.is_power_of_two() && v <= META_IO_DEFAULT_GRAIN {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Register `path` as a shared-LUN metadata device: probe its direct-I/O
/// alignment, prove the substrate serves `O_DIRECT`, and record the
/// posture every later `uring_fs` open of the path takes. Idempotent (a
/// registered path answers its recorded posture). Refuses a block device
/// that cannot be opened `O_DIRECT` — the buffered fallback is a regular
/// file's alone.
pub fn register_meta_device(path: &Path) -> Result<MetaIoMode> {
    if let Some(m) = meta_io_mode(path) {
        return Ok(m);
    }
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    let md = std::fs::metadata(path).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            e.kind(),
            format!("metadata device {}: cannot stat: {e}", path.display()),
        ))
    })?;
    let is_bdev = md.file_type().is_block_device();
    if !is_bdev && !md.file_type().is_file() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "metadata device {} is neither a block device nor a regular file",
            path.display()
        )));
    }
    let (grain, mem_align) = statx_dio_align(path)
        .or_else(|| sysfs_logical_block_size(path).map(|g| (g, g as usize)))
        .unwrap_or((META_IO_DEFAULT_GRAIN, META_IO_DEFAULT_GRAIN as usize));
    // A grain the ring's page arithmetic can honour: a power of two no
    // wider than the 4 KiB page (page boundaries are then always aligned).
    let grain = if grain.is_power_of_two() && grain <= META_IO_DEFAULT_GRAIN {
        grain
    } else {
        META_IO_DEFAULT_GRAIN
    };
    let mem_align = mem_align.max(grain as usize).next_power_of_two();
    let mode = if test_meta_buffered() {
        log::warn!(
            "metadata device {}: SQUEEZEFS_TEST_META_BUFFERED=1 — BUFFERED metadata I/O (the \
             harness seam; a second host's reads of this device are NOT coherent)",
            path.display()
        );
        META_IO_BUFFERED_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MetaIoMode::BUFFERED
    } else {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECT)
            .open(path)
        {
            Ok(_) => {
                META_IO_DIRECT_PATHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::info!(
                    "metadata device {}: O_DIRECT (grain {grain} B, buffer alignment {mem_align} B) — \
                     shared-LUN coherent metadata I/O (design-symmetric-metadata §5.12)",
                    path.display()
                );
                MetaIoMode {
                    grain,
                    mem_align,
                    direct: true,
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) && !is_bdev => {
                META_IO_BUFFERED_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::warn!(
                    "metadata volume {}: the filesystem refuses O_DIRECT ({e}) — falling back to \
                     BUFFERED metadata I/O for this REGULAR FILE (a dev/test venue such as tmpfs; a \
                     shared LUN is never a regular file, so no second host reads this volume). \
                     meta_io_buffered_fallback counts it",
                    path.display()
                );
                MetaIoMode::BUFFERED
            }
            Err(e) => {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "metadata device {}: O_DIRECT open failed ({e}); shared-LUN metadata I/O \
                         must bypass the host page cache (design-symmetric-metadata §5.12) and a \
                         block device that refuses it cannot serve a coherent set — REFUSING",
                        path.display()
                    ),
                )));
            }
        }
    };
    let mut modes = META_IO_MODES.write().unwrap_or_else(|e| e.into_inner());
    modes.insert(path.to_path_buf(), mode);
    if let Ok(c) = std::fs::canonicalize(path) {
        modes.insert(c, mode);
    }
    Ok(mode)
}

/// Forget every registered metadata path (the test harnesses' reset —
/// a sandbox file's path is reused across suites in one process).
pub fn clear_meta_devices() {
    META_IO_MODES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// A heap buffer with a guaranteed alignment — the direct-I/O staging
/// form (`posix_memalign`-class, freed with the same layout).
pub struct AlignedBuf {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
    layout: std::alloc::Layout,
}

// SAFETY: the buffer is uniquely owned heap memory; nothing about it is
// thread-affine.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// `len` zeroed bytes at `align` (a power of two; `len` need not be a
    /// multiple of it — the CALLER sizes direct I/O to the grain).
    pub fn zeroed(len: usize, align: usize) -> Self {
        let align = align.max(1).next_power_of_two();
        let layout =
            std::alloc::Layout::from_size_align(len.max(1), align).expect("aligned buffer layout");
        // SAFETY: a non-zero-sized layout; the allocation is checked below.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr =
            std::ptr::NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, len, layout }
    }

    /// An aligned copy of `src`.
    pub fn from_slice(src: &[u8], align: usize) -> Self {
        let mut b = Self::zeroed(src.len(), align);
        b.as_mut_slice().copy_from_slice(src);
        b
    }

    /// The buffer's length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is a live allocation of `len` initialized (zeroed
        // or copied) bytes, uniquely borrowed through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Hand the buffer to a `Bytes` (zero-copy; the allocation frees with
    /// the last reference).
    pub fn into_bytes(self) -> bytes::Bytes {
        if self.len == 0 {
            return bytes::Bytes::new();
        }
        bytes::Bytes::from_owner(self)
    }

    /// Whether `ptr` satisfies `align`.
    pub fn ptr_aligned(ptr: *const u8, align: usize) -> bool {
        align <= 1 || (ptr as usize) % align == 0
    }
}

impl AsRef<[u8]> for AlignedBuf {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: as in `as_mut_slice`, shared.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed(self.layout)` and is freed
        // exactly once here.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// The direct-write preflight: a misaligned offset or length is REFUSED
/// (the caller's bug — never a silent RMW here), an unaligned buffer is
/// bounced into an aligned copy. `None` = buffered path, nothing to do.
fn prepare_direct_write(
    mode: Option<MetaIoMode>,
    path: &Path,
    offset: u64,
    data: &mut bytes::Bytes,
) -> Result<()> {
    let Some(m) = mode.filter(|m| m.direct) else {
        return Ok(());
    };
    let len = data.len() as u64;
    if offset % m.grain != 0 || len % m.grain != 0 {
        META_IO_UNALIGNED_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "direct metadata write at offset {offset} of {len} bytes to {} is not aligned to \
                 the device grain ({} B) — a caller bug: the aligned form belongs beside the \
                 shape (meta_io_unaligned_refusals)",
                path.display(),
                m.grain
            ),
        )));
    }
    if !AlignedBuf::ptr_aligned(data.as_ptr(), m.mem_align) {
        META_IO_BOUNCE_BYTES.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        *data = AlignedBuf::from_slice(data, m.mem_align).into_bytes();
    }
    Ok(())
}

/// Zero-extend `image` to a multiple of `path`'s I/O grain — the aligned
/// form of a variable-length unit written into an extent it owns whole
/// (the slot-tails spill, PR 13i): the consumer records the unit's exact
/// length beside its address, so the trailing zeros are never read as
/// content. A buffered path (grain 1) keeps the image verbatim.
pub fn pad_to_grain(path: impl AsRef<Path>, mut image: Vec<u8>) -> bytes::Bytes {
    let grain = meta_io_grain(path.as_ref()) as usize;
    if grain > 1 {
        let padded = image.len().div_ceil(grain) * grain;
        image.resize(padded, 0);
    }
    bytes::Bytes::from(image)
}

/// **The harness's byte-planting primitive**: land `data` at ANY
/// `offset` of `path` — on a direct path by read-modify-write of the
/// covering aligned span (read fresh, patched, written whole), verbatim on
/// a buffered one. The crash and corruption contracts plant torn headers,
/// flipped padding bytes and smashed records at byte offsets; under the
/// aligned discipline those are caller-bug refusals for the PRODUCT
/// (`meta_io_unaligned_refusals`), so the harness names its intent here.
/// Never on a product path — the product's aligned forms live beside
/// their shapes.
pub async fn patch_at(
    path: impl AsRef<Path>,
    offset: u64,
    data: impl Into<bytes::Bytes>,
) -> Result<()> {
    let path = path.as_ref();
    let data: bytes::Bytes = data.into();
    if data.is_empty() {
        return Ok(());
    }
    let Some(m) = meta_io_mode(path).filter(|m| m.direct) else {
        return write_at(path, offset, data).await;
    };
    let lo = offset - offset % m.grain;
    let hi = (offset + data.len() as u64).div_ceil(m.grain) * m.grain;
    let len = (hi - lo) as usize;
    let cur = read_at(path, lo, len).await?;
    let mut image = AlignedBuf::zeroed(len, m.mem_align);
    let dst = image.as_mut_slice();
    let n = cur.len().min(len);
    dst[..n].copy_from_slice(&cur[..n]);
    let at = (offset - lo) as usize;
    dst[at..at + data.len()].copy_from_slice(&data);
    write_at(path, lo, image.into_bytes()).await
}

/// `meta_io_*` stats faces (surfaced UNGATED on the stats inode).
pub fn meta_io_stats_json() -> serde_json::Value {
    use std::sync::atomic::Ordering::Relaxed;
    serde_json::json!({
        "meta_io_direct_paths": META_IO_DIRECT_PATHS.load(Relaxed),
        "meta_io_buffered_fallback": META_IO_BUFFERED_FALLBACK.load(Relaxed),
        "meta_io_bounce_bytes": META_IO_BOUNCE_BYTES.load(Relaxed),
        "meta_io_unaligned_refusals": META_IO_UNALIGNED_REFUSALS.load(Relaxed),
        "meta_io_read_widened": META_IO_READ_WIDENED.load(Relaxed),
    })
}

// ---------------------------------------------------------------------------
// Fault-injection test support (design-wal-crash-consistency §4.7a).
//
// Live, test-exercised statics per the `nvme_dev.rs` precedent
// (`SIMULATE_CORRUPTION` / `FAIL_NEXT_WRITES`): the crash-contract and
// kill-9 suites (`tests/crash_contract_tests.rs`, `tests/crash_kill_tests.rs`)
// arm these to deterministically reproduce "power loss tore sector S
// mid-apply" and "writes since the last barrier are volatile" without root,
// KVM, or dm-flakey. Consulted at request admission (both the uring reactors
// and the blocking fallback): ONE relaxed atomic load when disarmed — the
// default — so the production hot path pays nothing.
// ---------------------------------------------------------------------------

pub struct TornWriteFault {
    /// Absolute file offset the tear triggers on (`u64::MAX` = disarmed).
    /// The first write whose byte range covers this offset is torn.
    pub offset: std::sync::atomic::AtomicU64,
    /// Bytes of the matching write to persist before the "crash" (a
    /// sub-sector prefix in practice).
    pub keep: std::sync::atomic::AtomicUsize,
}

/// After the torn write fires, every subsequent request on the SAME path
/// fails with `EIO` ("device died mid-commit") until [`clear_faults`].
pub static TORN_WRITE_FAULT: TornWriteFault = TornWriteFault {
    offset: std::sync::atomic::AtomicU64::new(u64::MAX),
    keep: std::sync::atomic::AtomicUsize::new(0),
};

/// Fast disarmed-path guard: `false` (default) ⇒ the shim is completely
/// inert and admission does no further fault work.
static FAULTS_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Persistent per-offset write error (`u64::MAX` = disarmed): every write
/// covering this offset fails `EIO` — nothing is written, nothing is
/// poisoned, and the fault STAYS armed until [`clear_faults`]. Models a
/// single bad sector (vs [`TORN_WRITE_FAULT`]'s one-shot dead-device
/// semantics); the reclaim bisect tests use it to wedge exactly one ino.
static SECTOR_WRITE_ERROR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Arm the persistent sector-write error at `offset`.
pub fn arm_sector_write_error(offset: u64) {
    SECTOR_WRITE_ERROR.store(offset, std::sync::atomic::Ordering::Relaxed);
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the armed persistent write error falls inside `[offset, offset+len)`.
fn sector_error_hits(offset: u64, len: usize) -> bool {
    let armed = SECTOR_WRITE_ERROR.load(std::sync::atomic::Ordering::Relaxed);
    armed != u64::MAX && armed >= offset && armed < offset + len as u64
}

#[derive(Default)]
struct FaultState {
    /// Paths whose "device died": every request fails `EIO`.
    poisoned: std::collections::HashSet<PathBuf>,
    /// Power-cut tracking: per path, the (offset, ORIGINAL bytes) of every
    /// write admitted since the last `fdatasync` — i.e. the volatile cache a
    /// real power loss would drop. [`power_cut`] reverts them.
    tracked: std::collections::HashMap<PathBuf, Vec<(u64, Vec<u8>)>>,
    /// Barrier fault: per path, the raw OS errno every `fdatasync` fails
    /// with while armed. Writes and reads proceed untouched — exactly the
    /// shape of a device-level write fence (NVMe reservation conflict):
    /// buffered entry writes keep succeeding into the page cache and the
    /// conflict surfaces only at the durability barrier
    /// (design-metadata-throughput §5.0 B1 pt 3, Issue 14).
    barrier_errors: std::collections::HashMap<PathBuf, i32>,
    /// Barrier stalls ([`arm_barrier_stall`]): per path, the barriers held
    /// in flight.
    barrier_stalls: std::collections::HashMap<PathBuf, HeldOps<oneshot::Sender<Result<()>>>>,
    /// Write stalls ([`arm_write_stall`]): per path, the range that parks
    /// and the writes held there.
    write_stalls: std::collections::HashMap<PathBuf, WriteStall>,
    /// Device latency ([`arm_device_latency`]): per path, how long every
    /// write / barrier is held before admission — a slow device, not a
    /// parked one.
    latency: std::collections::HashMap<PathBuf, DeviceLatency>,
}

/// An armed device latency: writes (`WriteAt` / `WriteAtBatch`) and
/// barriers (`Fdatasync`) on the path complete no earlier than this long
/// after submission.
#[derive(Clone, Copy)]
struct DeviceLatency {
    write: std::time::Duration,
    barrier: std::time::Duration,
}

/// One request parked on the latency lane until `due`.
struct DelayedReq {
    due: std::time::Instant,
    /// Admission order tiebreak (FIFO among equal deadlines).
    seq: u64,
    req: FsReq,
}

impl PartialEq for DelayedReq {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.seq == other.seq
    }
}
impl Eq for DelayedReq {}
impl PartialOrd for DelayedReq {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DelayedReq {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.due, self.seq).cmp(&(other.due, other.seq))
    }
}

/// The latency lane: a deadline heap drained by one lazily-spawned
/// thread that re-admits each request as [`FsReq::Delayed`] when its
/// deadline passes. Test-only machinery (spawned on the first arm, never
/// in production); a request's latency runs from its ORIGINAL admission,
/// so concurrent requests overlap exactly as they would on a device with
/// that service time and unbounded concurrency.
struct LatencyLane {
    heap: std::sync::Mutex<std::collections::BinaryHeap<std::cmp::Reverse<DelayedReq>>>,
    cv: std::sync::Condvar,
    next_seq: std::sync::atomic::AtomicU64,
}

static LATENCY_LANE: Lazy<LatencyLane> = Lazy::new(|| {
    std::thread::Builder::new()
        .name("sqz-uringfs-latency".to_string())
        .spawn(latency_lane_loop)
        .expect("spawn uring-fs latency lane");
    LatencyLane {
        heap: std::sync::Mutex::new(std::collections::BinaryHeap::new()),
        cv: std::sync::Condvar::new(),
        next_seq: std::sync::atomic::AtomicU64::new(0),
    }
});

fn latency_lane_loop() {
    let lane = &*LATENCY_LANE;
    let mut heap = lane.heap.lock().unwrap();
    loop {
        let Some(std::cmp::Reverse(top)) = heap.peek() else {
            heap = lane.cv.wait(heap).unwrap();
            continue;
        };
        let now = std::time::Instant::now();
        if top.due > now {
            let wait = top.due - now;
            heap = lane.cv.wait_timeout(heap, wait).unwrap().0;
            continue;
        }
        let item = heap.pop().expect("peeked").0;
        drop(heap);
        // Blocking send: the lane is the only producer that may wait on
        // queue space (a test-only thread), and dropping a held request
        // would strand its caller.
        let _ = URING_FS.sender().send(FsReq::Delayed(Box::new(item.req)));
        heap = lane.heap.lock().unwrap();
    }
}

/// Park `req` on the latency lane until `due`.
fn latency_lane_push(due: std::time::Instant, req: FsReq) {
    let lane = &*LATENCY_LANE;
    let seq = lane
        .next_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    lane.heap
        .lock()
        .unwrap()
        .push(std::cmp::Reverse(DelayedReq { due, seq, req }));
    lane.cv.notify_one();
}

/// Operations parked by a stall, plus the arrival tap a test awaits.
struct HeldOps<T> {
    arrived: squeezefs_ipc::sqz_channel::mpsc::UnboundedSender<()>,
    held: Vec<T>,
}

/// One parked write, resumable verbatim.
enum HeldWrite {
    WriteAt {
        offset: u64,
        data: bytes::Bytes,
        tx: WriteTx,
    },
    WriteAtBatch {
        ops: Vec<(u64, bytes::Bytes)>,
        tx: WriteTx,
    },
}

/// An armed write stall: the byte range that parks and its held ops.
struct WriteStall {
    range: (u64, u64),
    ops: HeldOps<HeldWrite>,
}

static FAULT_STATE: Lazy<std::sync::Mutex<FaultState>> =
    Lazy::new(|| std::sync::Mutex::new(FaultState::default()));

/// Arm the torn-write fault: the next write covering `offset` persists only
/// its first `keep` bytes, completes with `EIO`, and poisons its path.
pub fn arm_torn_write(offset: u64, keep: usize) {
    TORN_WRITE_FAULT
        .keep
        .store(keep, std::sync::atomic::Ordering::Relaxed);
    TORN_WRITE_FAULT
        .offset
        .store(offset, std::sync::atomic::Ordering::Relaxed);
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Begin volatile-cache tracking on `path`: every subsequent write is
/// captured (original bytes) until an `fdatasync` on the path marks them
/// durable. [`power_cut`] then reverts whatever is still volatile.
pub fn arm_power_cut(path: impl AsRef<Path>) {
    FAULT_STATE
        .lock()
        .unwrap()
        .tracked
        .insert(path.as_ref().to_path_buf(), Vec::new());
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Arm the barrier fault on `path`: every `fdatasync` fails with
/// `raw_os_error` until [`disarm_barrier_error`] / [`clear_faults`],
/// while writes and reads proceed untouched. This is the **barrier-layer**
/// fence-injection point the writer-guard tests require (design
/// §5.0 B1 pt 5: injecting at the entry-write layer would pass against
/// wiring the real fence never exercises — a fenced holder's buffered
/// writes succeed; only the barrier carries the reservation conflict).
pub fn arm_barrier_error(path: impl AsRef<Path>, raw_os_error: i32) {
    FAULT_STATE
        .lock()
        .unwrap()
        .barrier_errors
        .insert(path.as_ref().to_path_buf(), raw_os_error);
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Arm the **barrier stall** on `path`: an arriving `fdatasync` is held
/// IN FLIGHT — admitted (so its coverage snapshot is taken exactly where
/// a real submission would take it: writes admitted after this point stay
/// volatile) but not completed until [`release_barrier_stall`] resubmits
/// it for real.
///
/// This is the only seam that can put a third party's state push INSIDE a
/// barrier's window, which is precisely the shape spec **DUR-3** names
/// ("state a third party pushed while the barrier was in flight"). Unlike
/// [`arm_barrier_error`] nothing fails: the barrier eventually succeeds,
/// late.
///
/// Returns an arrival receiver — awaiting it proves a barrier really
/// reached the stall, so a test never races the window open.
pub fn arm_barrier_stall(
    path: impl AsRef<Path>,
) -> squeezefs_ipc::sqz_channel::mpsc::UnboundedReceiver<()> {
    let (arrived_tx, arrived_rx) = squeezefs_ipc::sqz_channel::mpsc::unbounded_channel();
    let mut st = FAULT_STATE.lock().unwrap();
    st.barrier_stalls.insert(
        path.as_ref().to_path_buf(),
        HeldOps {
            arrived: arrived_tx,
            held: Vec::new(),
        },
    );
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    arrived_rx
}

/// Disarm `path`'s barrier stall and resubmit every held barrier for real
/// (each caller's `fdatasync` completes normally, late).
pub fn release_barrier_stall(path: impl AsRef<Path>) {
    let held = match FAULT_STATE
        .lock()
        .unwrap()
        .barrier_stalls
        .remove(path.as_ref())
    {
        Some(stall) => stall.held,
        None => return,
    };
    for tx in held {
        let _ = URING_FS.sender().try_send(FsReq::Fdatasync {
            path: path.as_ref().to_path_buf(),
            tx,
        });
    }
}

/// Arm the **write stall** on `path` for writes intersecting
/// `[offset, offset + len)`: matching `write_at` / `write_at_batch`
/// requests (a batch is held whole if any of its ops intersects) are held
/// (never admitted — nothing lands) until [`release_write_stall`]
/// resubmits them. The durability sibling of [`arm_barrier_stall`]: it
/// parks a caller at a known point in its commit sequence, so a test can
/// build an exact interleaving instead of racing one.
///
/// Returns an arrival receiver, like [`arm_barrier_stall`].
pub fn arm_write_stall(
    path: impl AsRef<Path>,
    offset: u64,
    len: u64,
) -> squeezefs_ipc::sqz_channel::mpsc::UnboundedReceiver<()> {
    let (arrived_tx, arrived_rx) = squeezefs_ipc::sqz_channel::mpsc::unbounded_channel();
    let mut st = FAULT_STATE.lock().unwrap();
    st.write_stalls.insert(
        path.as_ref().to_path_buf(),
        WriteStall {
            range: (offset, offset + len),
            ops: HeldOps {
                arrived: arrived_tx,
                held: Vec::new(),
            },
        },
    );
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    arrived_rx
}

/// Disarm `path`'s write stall and resubmit every held write for real.
pub fn release_write_stall(path: impl AsRef<Path>) {
    let held = match FAULT_STATE
        .lock()
        .unwrap()
        .write_stalls
        .remove(path.as_ref())
    {
        Some(stall) => stall.ops.held,
        None => return,
    };
    for op in held {
        match op {
            HeldWrite::WriteAt { offset, data, tx } => {
                let _ = URING_FS.sender().try_send(FsReq::WriteAt {
                    path: path.as_ref().to_path_buf(),
                    offset,
                    data,
                    tx,
                });
            }
            HeldWrite::WriteAtBatch { ops, tx } => {
                let _ = URING_FS.sender().try_send(FsReq::WriteAtBatch {
                    path: path.as_ref().to_path_buf(),
                    ops,
                    tx,
                });
            }
        }
    }
}

/// Arm a **device latency** on `path`: every write (`write_at` /
/// `write_at_batch`) completes no earlier than `write` after its
/// submission and every `fdatasync` no earlier than `barrier` after its
/// — held on a deadline lane and then admitted for real, so the bytes
/// still land and concurrent requests overlap like they would on a device
/// with that service time. This is the SLOW-device seam (the stalls
/// above are PARKED-device seams): the conveyor-saturation harness
/// (`tests/conveyor_two_stage_tests.rs`) arms it to make the journal
/// write's device term a controlled constant.
pub fn arm_device_latency(
    path: impl AsRef<Path>,
    write: std::time::Duration,
    barrier: std::time::Duration,
) {
    FAULT_STATE.lock().unwrap().latency.insert(
        path.as_ref().to_path_buf(),
        DeviceLatency { write, barrier },
    );
    FAULTS_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Disarm `path`'s device latency (requests already on the lane still
/// complete late — they were admitted under the arm).
pub fn disarm_device_latency(path: impl AsRef<Path>) {
    FAULT_STATE.lock().unwrap().latency.remove(path.as_ref());
}

/// Disarm the barrier fault on `path` (the next `fdatasync` succeeds —
/// the consecutive-failure escalation's success-reset case).
pub fn disarm_barrier_error(path: impl AsRef<Path>) {
    let mut st = FAULT_STATE.lock().unwrap();
    st.barrier_errors.remove(path.as_ref());
    // FAULTS_ACTIVE stays set while other faults may be armed; harmless
    // when none are (the shim just finds nothing to do).
}

/// Simulate power loss on `path`: revert (in reverse admission order) every
/// tracked write not yet covered by an `fdatasync`. Returns how many writes
/// were reverted. The caller must have quiesced the path (no in-flight I/O),
/// exactly as a crash point does.
pub fn power_cut(path: impl AsRef<Path>) -> usize {
    use std::os::unix::fs::FileExt;
    let entries = FAULT_STATE
        .lock()
        .unwrap()
        .tracked
        .insert(path.as_ref().to_path_buf(), Vec::new())
        .unwrap_or_default();
    if entries.is_empty() {
        return 0;
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path.as_ref())
        .expect("power_cut: open tracked path");
    // Reverse order restores pre-write bytes under overlapping writes.
    let count = entries.len();
    for (offset, original) in entries.into_iter().rev() {
        f.write_all_at(&original, offset)
            .expect("power_cut: revert tracked write");
    }
    f.sync_data().expect("power_cut: settle reverted bytes");
    count
}

/// Disarm everything: torn fault, sector-write error, poisoned paths,
/// power-cut tracking.
pub fn clear_faults() {
    TORN_WRITE_FAULT
        .offset
        .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    TORN_WRITE_FAULT
        .keep
        .store(0, std::sync::atomic::Ordering::Relaxed);
    SECTOR_WRITE_ERROR.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    let mut st = FAULT_STATE.lock().unwrap();
    st.poisoned.clear();
    st.tracked.clear();
    st.barrier_errors.clear();
    // Held ops are dropped, not resumed: their callers see the closed
    // oneshot as a worker-closed error, which is the honest outcome for a
    // suite that tore its own fault state down mid-flight.
    st.barrier_stalls.clear();
    st.write_stalls.clear();
    st.latency.clear();
    FAULTS_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
}

fn fault_eio(what: &str) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(format!(
        "uring-fs fault injection: {what}"
    )))
}

/// Whether the armed tear offset falls inside `[offset, offset + len)`;
/// answers the armed `keep` — the bytes of THIS write (from its start)
/// that persist. On a coalesced aligned run (PR 13i — one write per
/// journal window) "this write" is the run: a tear inside it keeps the
/// run's prefix, which is exactly what a sector-granular device does to
/// one in-flight write.
fn tear_hits(offset: u64, len: usize) -> Option<usize> {
    let armed = TORN_WRITE_FAULT
        .offset
        .load(std::sync::atomic::Ordering::Relaxed);
    if armed == u64::MAX || armed < offset || armed >= offset + len as u64 {
        return None;
    }
    Some(
        TORN_WRITE_FAULT
            .keep
            .load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Synchronously persist `data[..keep]` at `offset` — the genuine prefix of
/// a torn write (std I/O on purpose: the "device" is failing, determinism
/// beats the uring hot path here, and this only runs with a fault armed).
fn tear_pwrite(path: &Path, offset: u64, data: &[u8], keep: usize) {
    use std::os::unix::fs::FileExt;
    let keep = keep.min(data.len());
    if keep == 0 {
        return;
    }
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.write_all_at(&data[..keep], offset);
        let _ = f.sync_data();
    }
}

/// Capture the ORIGINAL bytes of `[offset, offset + len)` for power-cut
/// revert. Short reads (beyond EOF) capture zeros — meta volumes are
/// preallocated, so writes never extend the file.
fn capture_original(path: &Path, offset: u64, len: usize) -> (u64, Vec<u8>) {
    use std::os::unix::fs::FileExt;
    let mut original = vec![0u8; len];
    if let Ok(f) = std::fs::OpenOptions::new().read(true).open(path) {
        let mut filled = 0usize;
        while filled < len {
            match f.read_at(&mut original[filled..], offset + filled as u64) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
    }
    (offset, original)
}

/// Strip the latency lane's [`FsReq::Delayed`] wrapper: `(inner, true)`
/// for a re-admitted request, `(req, false)` otherwise. Idempotent, so
/// both the shim and the disarmed admission paths can call it.
fn unwrap_delayed(req: FsReq) -> (FsReq, bool) {
    match req {
        FsReq::Delayed(inner) => (*inner, true),
        other => (other, false),
    }
}

/// Fault-shim request interception, shared by the uring reactors and the
/// blocking fallback. Returns `Some(req)` to proceed (possibly after
/// capturing power-cut originals) or `None` when the request was consumed
/// (its completion already sent an error).
fn fault_intercept(req: FsReq) -> Option<FsReq> {
    let mut st = FAULT_STATE.lock().unwrap();
    // A request re-admitted by the latency lane has served its latency;
    // every other fault below still applies to it.
    let (req, delayed) = unwrap_delayed(req);

    // Poisoned path: the device died — EVERY request fails.
    {
        let path = match &req {
            FsReq::WriteAll { path, .. }
            | FsReq::ReadAll { path, .. }
            | FsReq::ReadAt { path, .. }
            | FsReq::WriteAt { path, .. }
            | FsReq::WriteAtBatch { path, .. }
            | FsReq::Fdatasync { path, .. } => path,
            FsReq::Delayed(_) => unreachable!("unwrapped above"),
        };
        if st.poisoned.contains(path) {
            let err = || fault_eio("path poisoned (device died mid-commit)");
            match req {
                FsReq::WriteAll { tx, .. }
                | FsReq::WriteAt { tx, .. }
                | FsReq::WriteAtBatch { tx, .. } => send_now(tx, Err(err())),
                FsReq::Fdatasync { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                FsReq::ReadAll { tx, .. } | FsReq::ReadAt { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                FsReq::Delayed(_) => unreachable!("unwrapped above"),
            }
            return None;
        }
    }

    // Device latency: hold the request until its deadline, then re-admit
    // it (as `Delayed`, so it is not held twice). Checked BEFORE the other
    // write/barrier faults so they apply at the re-admission — the
    // moment the slow device would actually service the request.
    if !delayed {
        let arm = match &req {
            FsReq::WriteAt { path, .. } | FsReq::WriteAtBatch { path, .. } => {
                st.latency.get(path).map(|l| l.write)
            }
            FsReq::Fdatasync { path, .. } => st.latency.get(path).map(|l| l.barrier),
            _ => None,
        };
        if let Some(d) = arm.filter(|d| !d.is_zero()) {
            drop(st);
            latency_lane_push(std::time::Instant::now() + d, req);
            return None;
        }
    }

    match req {
        FsReq::WriteAt {
            path,
            offset,
            data,
            tx,
        } => {
            if sector_error_hits(offset, data.len()) {
                send_now(tx, Err(fault_eio("persistent sector write error")));
                return None;
            }
            if let Some(keep) = tear_hits(offset, data.len()) {
                tear_pwrite(&path, offset, &data, keep);
                TORN_WRITE_FAULT
                    .offset
                    .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
                st.poisoned.insert(path);
                send_now(tx, Err(fault_eio("write torn mid-sector, device died")));
                return None;
            }
            // Write stall: park the whole request (nothing lands) until
            // `release_write_stall` resubmits it.
            if let Some(stall) = st.write_stalls.get_mut(&path) {
                let (lo, hi) = stall.range;
                if offset < hi && offset + data.len() as u64 > lo {
                    let _ = stall.ops.arrived.send(());
                    stall.ops.held.push(HeldWrite::WriteAt { offset, data, tx });
                    return None;
                }
            }
            if let Some(log) = st.tracked.get_mut(&path) {
                let cap = capture_original(&path, offset, data.len());
                log.push(cap);
            }
            Some(FsReq::WriteAt {
                path,
                offset,
                data,
                tx,
            })
        }
        FsReq::WriteAtBatch { path, ops, tx } => {
            if ops.iter().any(|(off, d)| sector_error_hits(*off, d.len())) {
                // The whole logical commit fails loudly (batch semantics);
                // nothing is written, the fault stays armed.
                send_now(tx, Err(fault_eio("persistent sector write error in batch")));
                return None;
            }
            let torn_at = ops
                .iter()
                .position(|(off, d)| tear_hits(*off, d.len()).is_some());
            if let Some(k) = torn_at {
                // Entries admitted before the tear land fully; the matching
                // entry keeps its prefix; later entries never reach the
                // device ("died mid-commit").
                for (off, d) in &ops[..k] {
                    tear_pwrite(&path, *off, d, d.len());
                }
                let (off, d) = &ops[k];
                let keep = tear_hits(*off, d.len()).unwrap_or(0);
                tear_pwrite(&path, *off, d, keep);
                TORN_WRITE_FAULT
                    .offset
                    .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
                st.poisoned.insert(path);
                send_now(
                    tx,
                    Err(fault_eio("batch write torn mid-sector, device died")),
                );
                return None;
            }
            // Write stall: a batch is held WHOLE if any op intersects the
            // armed range (batch semantics — nothing of it lands).
            if let Some(stall) = st.write_stalls.get_mut(&path) {
                let (lo, hi) = stall.range;
                if ops
                    .iter()
                    .any(|(off, d)| *off < hi && *off + d.len() as u64 > lo)
                {
                    let _ = stall.ops.arrived.send(());
                    stall.ops.held.push(HeldWrite::WriteAtBatch { ops, tx });
                    return None;
                }
            }
            if let Some(log) = st.tracked.get_mut(&path) {
                for (off, d) in &ops {
                    let cap = capture_original(&path, *off, d.len());
                    log.push(cap);
                }
            }
            Some(FsReq::WriteAtBatch { path, ops, tx })
        }
        FsReq::Fdatasync { path, tx } => {
            // Barrier fault first: the fence rejects the flush wholesale —
            // nothing becomes durable, tracked volatile writes stay
            // volatile, and the armed errno (reservation-conflict class in
            // the guard tests) reaches the caller with its raw_os_error
            // intact.
            if let Some(code) = st.barrier_errors.get(&path) {
                let _ = tx.send(Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                    *code,
                ))));
                return None;
            }
            // The barrier makes everything admitted before it durable. The
            // caller awaits this completion before relying on durability, and
            // the serial test harness admits no concurrent writes in the
            // window, so clearing at admission is exact for its users.
            if let Some(log) = st.tracked.get_mut(&path) {
                log.clear();
            }
            // Barrier stall: the coverage snapshot above already happened
            // (this barrier IS submitted, in the model); only its
            // completion is held, so anything written from here on stays
            // volatile — the DUR-3 window, exactly.
            if let Some(stall) = st.barrier_stalls.get_mut(&path) {
                let _ = stall.arrived.send(());
                stall.held.push(tx);
                return None;
            }
            Some(FsReq::Fdatasync { path, tx })
        }
        other => Some(other),
    }
}

/// Completion sink: single ops answer their own oneshot; batch entries share
/// an aggregate that fires once when the last entry lands. Both carry the
/// request's admission instant for the outcome's stamps.
enum UnitDone {
    Single {
        tx: WriteTx,
        admitted_at: std::time::Instant,
    },
    Batch(Rc<std::cell::RefCell<BatchState>>),
}

struct BatchState {
    remaining: usize,
    first_err: Option<SqueezefsError>,
    tx: Option<WriteTx>,
    admitted_at: std::time::Instant,
}

impl UnitDone {
    /// `now` is the reap instant of the CQE that decided `res` (one clock
    /// read per reap pass, shared by every completion it delivers).
    fn complete(self, res: Result<()>, now: std::time::Instant) {
        match self {
            UnitDone::Single { tx, admitted_at } => {
                let _ = tx.send(WriteOutcome {
                    result: res,
                    admitted_at,
                    woken_at: now,
                });
            }
            UnitDone::Batch(state) => {
                let mut st = state.borrow_mut();
                if let Err(e) = res {
                    if st.first_err.is_none() {
                        st.first_err = Some(e);
                    }
                }
                st.remaining -= 1;
                if st.remaining == 0 {
                    if let Some(tx) = st.tx.take() {
                        let _ = tx.send(WriteOutcome {
                            result: match st.first_err.take() {
                                Some(e) => Err(e),
                                None => Ok(()),
                            },
                            admitted_at: st.admitted_at,
                            woken_at: now,
                        });
                    }
                }
            }
        }
    }
}

/// One in-flight operation. Holds its own `Rc<File>` so fd-cache eviction can
/// never close a file with an outstanding SQE.
enum Pending {
    Read {
        file: Rc<File>,
        /// The read's destination — aligned for direct I/O; on a direct
        /// path it covers the grain-widened span and the caller's window
        /// is `[skip, skip + want)` of it.
        buf: AlignedBuf,
        /// The device offset the buffer's byte 0 reads.
        file_offset: u64,
        filled: usize,
        /// The caller's window inside the buffer.
        skip: usize,
        want: usize,
        /// The direct grain (1 on a buffered path): a short read that
        /// ends off the grain cannot continue under `O_DIRECT`.
        grain: usize,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    Write {
        file: Rc<File>,
        data: bytes::Bytes,
        file_offset: u64,
        written: usize,
        done: UnitDone,
    },
    Fsync {
        file: Rc<File>,
        tx: oneshot::Sender<Result<()>>,
    },
}

/// Per-worker open-file cache: path → (shared fd, last-use generation).
struct FdCache {
    /// `(fd, LRU stamp, opened O_DIRECT)` — the posture travels with the
    /// entry so a path registered AFTER its first (buffered) open is
    /// re-opened direct at its next use, never served off the stale fd.
    map: HashMap<PathBuf, (Rc<File>, u64, bool)>,
    gen: u64,
}

impl FdCache {
    fn new() -> Self {
        Self {
            map: HashMap::with_capacity(fd_cache_cap()),
            gen: 0,
        }
    }

    /// The cached fd for `path` if one is open in the wanted posture; a
    /// posture mismatch drops the entry (in-flight ops hold their own Rc).
    fn touch(&mut self, path: &Path, direct: bool) -> Option<Rc<File>> {
        self.gen += 1;
        let gen = self.gen;
        match self.map.get_mut(path) {
            Some((f, g, d)) if *d == direct => {
                *g = gen;
                Some(f.clone())
            }
            Some(_) => {
                self.map.remove(path);
                None
            }
            None => None,
        }
    }

    fn insert(&mut self, path: PathBuf, file: File, direct: bool) -> Rc<File> {
        self.gen += 1;
        if self.map.len() >= fd_cache_cap() {
            // Evict the least-recently-used entry. In-flight ops hold their
            // own Rc clone, so eviction never closes a busy fd.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, g, _))| *g)
                .map(|(p, _)| p.clone())
            {
                self.map.remove(&oldest);
            }
        }
        let rc = Rc::new(file);
        self.map.insert(path, (rc.clone(), self.gen, direct));
        rc
    }
}

/// Open `path` for cached O_RDWR use (create per `create`), via the cache.
/// A registered metadata device opens `O_DIRECT` (module comment above).
fn cached_open(cache: &mut FdCache, path: &Path, create: bool) -> Result<Rc<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let direct = meta_io_mode(path).is_some_and(|m| m.direct);
    if let Some(f) = cache.touch(path, direct) {
        return Ok(f);
    }
    // O_RDWR (not O_WRONLY): this fd is cached and may later be reused by a
    // `ReadAt` on the same path. A write-only cached fd makes io_uring Read
    // return EBADF (fd not open for read).
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .custom_flags(libc::O_CLOEXEC | if direct { libc::O_DIRECT } else { 0 })
        .open(path)
        .map_err(map_io)?;
    Ok(cache.insert(path.to_path_buf(), f, direct))
}

/// The read shape for `[offset, offset + size)` on `path`: on a direct
/// path the span widened to the grain (`(span_offset, span_len, skip)`),
/// verbatim otherwise. `grain` is 1 on a buffered path.
fn read_span(path: &Path, offset: u64, size: usize) -> (u64, usize, usize, usize) {
    match meta_io_mode(path).filter(|m| m.direct) {
        Some(m) => {
            let g = m.grain;
            let start = offset - offset % g;
            let end = (offset + size as u64).div_ceil(g) * g;
            let skip = (offset - start) as usize;
            if start != offset || end != offset + size as u64 {
                META_IO_READ_WIDENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            (start, (end - start) as usize, skip, g as usize)
        }
        None => (offset, size, 0, 1),
    }
}

/// The aligned destination for a read of `len` bytes on `path` (the
/// grain's alignment on a direct path; a plain allocation otherwise).
fn read_buf_for(path: &Path, len: usize) -> AlignedBuf {
    let align = meta_io_mode(path)
        .filter(|m| m.direct)
        .map_or(1, |m| m.mem_align);
    AlignedBuf::zeroed(len, align)
}

/// Reactor state for one worker thread.
struct Reactor {
    ring: io_uring::IoUring,
    slots: Vec<Option<Pending>>,
    free: Vec<usize>,
    inflight: usize,
    cache: FdCache,
}

impl Reactor {
    fn new(ring: io_uring::IoUring) -> Self {
        Self {
            ring,
            slots: Vec::new(),
            free: Vec::new(),
            inflight: 0,
            cache: FdCache::new(),
        }
    }

    fn claim_slot(&mut self, p: Pending) -> usize {
        self.inflight += 1;
        if let Some(i) = self.free.pop() {
            self.slots[i] = Some(p);
            i
        } else {
            self.slots.push(Some(p));
            self.slots.len() - 1
        }
    }

    fn release_slot(&mut self, i: usize) -> Pending {
        self.inflight -= 1;
        self.free.push(i);
        self.slots[i].take().expect("released empty uring-fs slot")
    }

    /// Build the SQE for the current state of slot `i`.
    fn sqe_for(&self, i: usize) -> io_uring::squeue::Entry {
        use io_uring::{opcode, types};
        match self.slots[i].as_ref().expect("sqe for empty slot") {
            Pending::Read {
                file,
                buf,
                file_offset,
                filled,
                ..
            } => {
                let total = buf.len();
                let ptr = buf.as_ref()[*filled..].as_ptr() as *mut u8;
                opcode::Read::new(types::Fd(file.as_raw_fd()), ptr, (total - *filled) as u32)
                    .offset(*file_offset + *filled as u64)
                    .build()
                    .user_data(i as u64)
            }
            Pending::Write {
                file,
                data,
                file_offset,
                written,
                ..
            } => opcode::Write::new(
                types::Fd(file.as_raw_fd()),
                data[*written..].as_ptr(),
                (data.len() - *written) as u32,
            )
            .offset(*file_offset + *written as u64)
            .build()
            .user_data(i as u64),
            Pending::Fsync { file, .. } => opcode::Fsync::new(types::Fd(file.as_raw_fd()))
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .user_data(i as u64),
        }
    }

    /// Queue the SQE for slot `i`. A full SQ is normal backpressure (a
    /// single `WriteAtBatch` may carry more entries than the ring): flush
    /// the queued SQEs to the kernel with `submit()` — which consumes SQ
    /// entries immediately, independent of completions — and push again.
    /// Only a push that fails right after a successful flush is a real
    /// error (and must be loud).
    fn push_slot(&mut self, i: usize) {
        self.push_slot_flags(i, io_uring::squeue::Flags::empty());
    }

    /// [`Self::push_slot`] with SQE flags (`IO_LINK` for the slow-device
    /// seam's chained batch).
    fn push_slot_flags(&mut self, i: usize, flags: io_uring::squeue::Flags) {
        let sqe = self.sqe_for(i).flags(flags);
        // SAFETY: buffers referenced by the SQE live in `self.slots[i]`, which
        // stays untouched until this SQE's completion is reaped.
        let mut res = unsafe { self.ring.submission().push(&sqe) };
        if res.is_err() && self.ring.submit().is_ok() {
            // SAFETY: same invariant as above.
            res = unsafe { self.ring.submission().push(&sqe) };
        }
        if res.is_err() {
            let p = self.release_slot(i);
            let err =
                || SqueezefsError::Io(std::io::Error::other("uring-fs submission queue overflow"));
            match p {
                Pending::Read { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                Pending::Write { done, .. } => done.complete(Err(err()), std::time::Instant::now()),
                Pending::Fsync { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
            }
        }
    }

    /// Admit one logical write batch: every non-empty `(offset, bytes)`
    /// gets its own slot + SQE sharing one aggregate completion. With
    /// `link_after` (the slow-device seam on an owned ring — the pool
    /// models latency on its deadline lane instead) the batch is chained
    /// behind a `TIMEOUT(ETIME_SUCCESS)` SQE: the whole batch completes no
    /// earlier than that latency after submission, on the same ring, so
    /// the owning lane's slow path (a completion NOT present at submit) is
    /// exercised by a REAL late CQE. The kernel reads the timespec at
    /// submission: `link_after`'s caller submits before returning.
    fn admit_write_batch(
        &mut self,
        path: PathBuf,
        ops: Vec<(u64, bytes::Bytes)>,
        tx: WriteTx,
        admitted_at: std::time::Instant,
        link_after: Option<&io_uring::types::Timespec>,
    ) {
        let file = match cached_open(&mut self.cache, &path, true) {
            Ok(f) => f,
            Err(e) => {
                send_now(tx, Err(e));
                return;
            }
        };
        let mut entries: Vec<(u64, bytes::Bytes)> =
            ops.into_iter().filter(|(_, d)| !d.is_empty()).collect();
        if entries.is_empty() {
            send_now(tx, Ok(()));
            return;
        }
        // The direct-write preflight, per op: a misaligned op fails the
        // whole logical commit before any byte is submitted.
        let mode = meta_io_mode(&path);
        for (offset, data) in entries.iter_mut() {
            if let Err(e) = prepare_direct_write(mode, &path, *offset, data) {
                send_now(tx, Err(e));
                return;
            }
        }
        let state = Rc::new(std::cell::RefCell::new(BatchState {
            remaining: entries.len(),
            first_err: None,
            tx: Some(tx),
            admitted_at,
        }));
        let linked = link_after.is_some();
        if let Some(ts) = link_after {
            let t = io_uring::opcode::Timeout::new(ts)
                .flags(io_uring::types::TimeoutFlags::ETIME_SUCCESS)
                .build()
                .flags(io_uring::squeue::Flags::IO_LINK)
                .user_data(SEAM_TIMEOUT_UD);
            // SAFETY: the timespec outlives the submission (caller's
            // contract above); the SQE references nothing else.
            if unsafe { self.ring.submission().push(&t) }.is_err() && self.ring.submit().is_ok() {
                // SAFETY: as above.
                let _ = unsafe { self.ring.submission().push(&t) };
            }
        }
        let n = entries.len();
        for (k, (offset, data)) in entries.into_iter().enumerate() {
            let i = self.claim_slot(Pending::Write {
                file: file.clone(),
                data,
                file_offset: offset,
                written: 0,
                done: UnitDone::Batch(state.clone()),
            });
            let flags = if linked && k + 1 < n {
                io_uring::squeue::Flags::IO_LINK
            } else {
                io_uring::squeue::Flags::empty()
            };
            self.push_slot_flags(i, flags);
        }
    }

    /// Admit one request: do the (rare, blocking) opens inline, then queue
    /// its first SQE(s).
    fn admit(&mut self, req: FsReq) {
        // `ufs_admit`: one clock read per admitted request (the queue
        // hop's end); read before the open so a cold fd-cache miss counts
        // against the device span, not the queue.
        let admitted_at = std::time::Instant::now();
        // Fault-injection shim (§4.7a): one relaxed load when disarmed.
        let req = if FAULTS_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            match fault_intercept(req) {
                Some(r) => r,
                None => return, // consumed: completion already sent an error
            }
        } else {
            // A latency-lane re-admission racing `clear_faults` arrives
            // wrapped with the shim already disarmed.
            unwrap_delayed(req).0
        };
        match req {
            FsReq::Delayed(_) => unreachable!("unwrapped at admission"),
            FsReq::WriteAll { path, data, tx } => {
                let opened = (|| -> Result<Rc<File>> {
                    use std::os::unix::fs::OpenOptionsExt;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(map_io)?;
                    }
                    let f = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open(&path)
                        .map_err(map_io)?;
                    // A whole-file rewrite is the control path's (config
                    // records, never a metadata device): buffered posture.
                    Ok(self.cache.insert(path.clone(), f, false))
                })();
                match opened {
                    Ok(file) => {
                        let i = self.claim_slot(Pending::Write {
                            file,
                            data,
                            file_offset: 0,
                            written: 0,
                            done: UnitDone::Single { tx, admitted_at },
                        });
                        self.push_slot(i);
                    }
                    Err(e) => send_now(tx, Err(e)),
                }
            }
            FsReq::ReadAll { path, tx } => {
                use std::os::unix::fs::OpenOptionsExt;
                let opened = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(&path)
                    .and_then(|f| f.metadata().map(|m| (f, m.len() as usize)))
                    .map_err(map_io);
                match opened {
                    Ok((f, len)) => {
                        let i = self.claim_slot(Pending::Read {
                            file: Rc::new(f),
                            buf: AlignedBuf::zeroed(len, 1),
                            file_offset: 0,
                            filled: 0,
                            skip: 0,
                            want: len,
                            grain: 1,
                            tx,
                        });
                        if len == 0 {
                            // Nothing to read: complete immediately.
                            if let Pending::Read { tx, .. } = self.release_slot(i) {
                                let _ = tx.send(Ok(bytes::Bytes::new()));
                            }
                        } else {
                            self.push_slot(i);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            FsReq::ReadAt {
                path,
                offset,
                size,
                tx,
            } => match cached_open(&mut self.cache, &path, true) {
                Ok(file) => {
                    let (span_off, span_len, skip, grain) = read_span(&path, offset, size);
                    let i = self.claim_slot(Pending::Read {
                        file,
                        buf: read_buf_for(&path, span_len),
                        file_offset: span_off,
                        filled: 0,
                        skip,
                        want: size,
                        grain,
                        tx,
                    });
                    if size == 0 {
                        if let Pending::Read { tx, .. } = self.release_slot(i) {
                            let _ = tx.send(Ok(bytes::Bytes::new()));
                        }
                    } else {
                        self.push_slot(i);
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            },
            FsReq::WriteAt {
                path,
                offset,
                mut data,
                tx,
            } => match cached_open(&mut self.cache, &path, true) {
                Ok(file) => {
                    if data.is_empty() {
                        send_now(tx, Ok(()));
                        return;
                    }
                    if let Err(e) =
                        prepare_direct_write(meta_io_mode(&path), &path, offset, &mut data)
                    {
                        send_now(tx, Err(e));
                        return;
                    }
                    let i = self.claim_slot(Pending::Write {
                        file,
                        data,
                        file_offset: offset,
                        written: 0,
                        done: UnitDone::Single { tx, admitted_at },
                    });
                    self.push_slot(i);
                }
                Err(e) => send_now(tx, Err(e)),
            },
            FsReq::WriteAtBatch { path, ops, tx } => {
                self.admit_write_batch(path, ops, tx, admitted_at, None);
            }
            FsReq::Fdatasync { path, tx } => match cached_open(&mut self.cache, &path, false) {
                Ok(file) => {
                    let i = self.claim_slot(Pending::Fsync { file, tx });
                    self.push_slot(i);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            },
        }
    }

    /// Advance slot `i` with its completion result; returns `true` if the
    /// slot needs its next SQE pushed (short read/write continuation).
    /// `now` is the reap pass's clock read (`ufs_wake` for the outcomes
    /// it delivers).
    fn advance(&mut self, i: usize, res: i32, now: std::time::Instant) -> bool {
        enum Next {
            Done,
            Resubmit,
        }
        let next = {
            let p = self.slots[i].as_mut().expect("completion for empty slot");
            match p {
                Pending::Read {
                    filled, buf, grain, ..
                } => {
                    if res < 0 {
                        Next::Done
                    } else if res == 0 {
                        Next::Done // EOF: short read, truncate below
                    } else {
                        *filled += res as usize;
                        // A direct continuation must resume on the grain;
                        // a short read that ends off it is the file's end.
                        if *filled < buf.len() && *filled % *grain == 0 {
                            Next::Resubmit
                        } else {
                            Next::Done
                        }
                    }
                }
                Pending::Write { written, data, .. } => {
                    if res <= 0 {
                        Next::Done
                    } else {
                        *written += res as usize;
                        if *written < data.len() {
                            Next::Resubmit
                        } else {
                            Next::Done
                        }
                    }
                }
                Pending::Fsync { .. } => Next::Done,
            }
        };

        match next {
            Next::Resubmit => true,
            Next::Done => {
                let p = self.release_slot(i);
                match p {
                    Pending::Read {
                        buf,
                        filled,
                        skip,
                        want,
                        tx,
                        ..
                    } => {
                        if res < 0 {
                            let _ = tx.send(Err(map_io(std::io::Error::from_raw_os_error(-res))));
                        } else {
                            // The caller's window out of the (possibly
                            // widened) span — a refcounted slice, no copy;
                            // a short read truncates it.
                            let end = filled.min(skip + want);
                            let out = if end <= skip {
                                bytes::Bytes::new()
                            } else {
                                buf.into_bytes().slice(skip..end)
                            };
                            let _ = tx.send(Ok(out));
                        }
                    }
                    Pending::Write { done, .. } => {
                        if res < 0 {
                            done.complete(
                                Err(map_io(std::io::Error::from_raw_os_error(-res))),
                                now,
                            );
                        } else if res == 0 {
                            done.complete(
                                Err(map_io(std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    "uring write returned 0",
                                ))),
                                now,
                            );
                        } else {
                            done.complete(Ok(()), now);
                        }
                    }
                    Pending::Fsync { tx, .. } => {
                        if res < 0 {
                            let _ = tx.send(Err(map_io(std::io::Error::from_raw_os_error(-res))));
                        } else {
                            let _ = tx.send(Ok(()));
                        }
                    }
                }
                false
            }
        }
    }
}

fn worker_loop(rx: crossbeam::channel::Receiver<FsReq>) {
    let ring = match io_uring::IoUring::new(RING_ENTRIES) {
        Ok(r) => r,
        Err(e) => {
            log::error!("uring-fs: failed to create IoUring: {e:?}");
            blocking_fallback_loop(rx);
            return;
        }
    };
    let mut r = Reactor::new(ring);
    let mut disconnected = false;

    loop {
        // Admission: block only when idle; otherwise burst-drain the queue.
        if r.inflight == 0 {
            if disconnected {
                return;
            }
            match rx.recv() {
                Ok(req) => r.admit(req),
                Err(_) => return,
            }
        }
        while r.inflight < ADMIT_CAP && !disconnected {
            match rx.try_recv() {
                Ok(req) => r.admit(req),
                Err(crossbeam::channel::TryRecvError::Empty) => break,
                Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                }
            }
        }
        if r.inflight == 0 {
            continue;
        }

        // Submit everything queued and wait for at least one completion.
        match r.ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Ring is broken: fail every in-flight op loudly and drop to
                // the blocking fallback for the rest of the process lifetime.
                log::error!("uring-fs: submit_and_wait failed: {e:?}");
                r.fail_all("uring-fs ring failed");
                blocking_fallback_loop(rx);
                return;
            }
        }
        r.reap();
    }
}

/// `user_data` of the slow-device seam's chain-head `TIMEOUT` SQE (never a
/// slot index; skipped at reap).
const SEAM_TIMEOUT_UD: u64 = u64::MAX - 1;
/// `user_data` of an owned ring's wake-eventfd `READ` SQE (skipped at
/// reap; its arrival re-arms the next park).
const WAKE_UD: u64 = u64::MAX;

impl Reactor {
    /// Reap every posted completion; push continuations after the CQ
    /// borrow ends (each continuation reuses a just-reaped SQ slot). ONE
    /// clock read per reap pass stamps `ufs_wake` on every outcome it
    /// delivers. Returns `true` if the owned ring's wake CQE was among
    /// them.
    fn reap(&mut self) -> bool {
        let mut resubmit: Vec<usize> = Vec::new();
        let mut woke = false;
        {
            let mut cq = self.ring.completion();
            cq.sync();
            let completed: Vec<(u64, i32)> = (&mut cq)
                .map(|cqe| (cqe.user_data(), cqe.result()))
                .collect();
            drop(cq);
            let now = std::time::Instant::now();
            for (ud, res) in completed {
                match ud {
                    WAKE_UD => woke = true,
                    SEAM_TIMEOUT_UD => {}
                    slot => {
                        if self.advance(slot as usize, res, now) {
                            resubmit.push(slot as usize);
                        }
                    }
                }
            }
        }
        for slot in resubmit {
            self.push_slot_continue(slot);
        }
        woke
    }

    /// Fail every in-flight op loudly (the ring is broken).
    fn fail_all(&mut self, what: &'static str) {
        let now = std::time::Instant::now();
        for i in 0..self.slots.len() {
            if self.slots[i].is_some() {
                let p = self.release_slot(i);
                let err = || map_io(std::io::Error::other(what));
                match p {
                    Pending::Read { tx, .. } => {
                        let _ = tx.send(Err(err()));
                    }
                    Pending::Write { done, .. } => done.complete(Err(err()), now),
                    Pending::Fsync { tx, .. } => {
                        let _ = tx.send(Err(err()));
                    }
                }
            }
        }
    }

    /// Re-queue a continuation SQE for a slot that stays in flight (the slot
    /// was NOT released, so `inflight` is unchanged).
    fn push_slot_continue(&mut self, i: usize) {
        let sqe = self.sqe_for(i);
        // SAFETY: as in `push_slot` — the slot owns every buffer the SQE
        // references and is not touched until its completion arrives.
        let res = unsafe { self.ring.submission().push(&sqe) };
        if res.is_err() {
            let p = self.release_slot(i);
            let err = || {
                SqueezefsError::Io(std::io::Error::other(
                    "uring-fs submission queue overflow (continuation)",
                ))
            };
            match p {
                Pending::Read { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                Pending::Write { done, .. } => done.complete(Err(err()), std::time::Instant::now()),
                Pending::Fsync { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The OWNED reactor (e2e perf audit C-2): one io_uring driven by the thread
// that also awaits its completions.
//
// The pool above is a worker-per-ring pipeline behind a queue: a caller's
// write crosses two threads to complete (caller → worker, worker → the
// caller's lane) and each crossing is a scheduler wake — the hop chain
// `uring_fs_write_phase_ns` decomposes. A lane that owns its ring submits
// on its own thread (`submit_write_at_batch`: admit + `submit()`, no wait),
// parks IN the ring (`park`: `io_uring_enter` with the wake-eventfd `READ`
// SQE armed, so a task wake and a device completion arrive through one
// wait), and reaps on its own thread (`service`) — the completion's
// oneshot fires on the thread that will poll the awaiting task, and the
// only cross-thread wakes left are the kernel's own (the io-wq punt a
// buffered block-device write takes, and the ring wait's return).
//
// Same reactor, same fault shim, same continuation/short-write handling,
// same `WriteCompletion` handle as the pool — only WHO drives it differs.
// ---------------------------------------------------------------------------

/// A wake handle onto an owned reactor's eventfd (any thread): the lane
/// parker's `unpark` (`squeezefs_ipc::sqz_exec::LanePark`).
#[derive(Clone)]
pub struct RingWake {
    fd: Arc<std::os::fd::OwnedFd>,
}

impl RingWake {
    /// Bump the eventfd: a park in progress returns, a park not yet
    /// entered returns at once when it does (the counter is sticky).
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: an 8-byte write of a u64 to a live eventfd this handle
        // co-owns; EAGAIN (counter saturated) is impossible at our rates
        // and harmless (a saturated counter is already "woken").
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }
}

/// The `Send` half of an owned reactor: the ring and its wake eventfd,
/// created on any thread and handed to the thread that will drive them
/// ([`OwnedRing::into_reactor`]).
pub struct OwnedRing {
    ring: io_uring::IoUring,
    wake: RingWake,
}

impl OwnedRing {
    /// A ring of `entries` SQEs plus its wake eventfd.
    pub fn new(entries: u32) -> std::io::Result<Self> {
        let ring = io_uring::IoUring::new(entries)?;
        // SAFETY: eventfd(2) with no invalid arguments; the fd is owned.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh, valid fd we own.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        Ok(Self {
            ring,
            wake: RingWake { fd: Arc::new(fd) },
        })
    }

    /// The any-thread wake handle.
    pub fn wake_handle(&self) -> RingWake {
        self.wake.clone()
    }

    /// Bind the ring to the calling thread as its reactor.
    pub fn into_reactor(self) -> OwnedReactor {
        OwnedReactor {
            r: Reactor::new(self.ring),
            wake: self.wake,
            wake_buf: Box::new(0),
            wake_armed: false,
        }
    }
}

/// One io_uring owned and driven by ONE thread (module comment above; the
/// journal lane keeps it in a thread-local). Built from an [`OwnedRing`]
/// on the driving thread — the reactor's slot table is thread-bound.
pub struct OwnedReactor {
    r: Reactor,
    wake: RingWake,
    /// The eventfd `READ` SQE's target — boxed so its address is stable
    /// for the SQE's lifetime whatever moves this struct before the first
    /// arm.
    wake_buf: Box<u64>,
    /// Exactly one wake `READ` is pending whenever the thread parks.
    wake_armed: bool,
}

/// The shim's verdict for a write bound for an owned ring.
enum InlineVerdict {
    /// Proceed on the ring (power-cut originals captured), with the armed
    /// device latency to model as a linked timeout.
    Proceed {
        ops: Vec<(u64, bytes::Bytes)>,
        latency: Option<std::time::Duration>,
    },
    /// The shim consumed the write (failed it now, or parked it on a
    /// stall to be resubmitted through the pool): the outcome arrives on
    /// this completion.
    Consumed(WriteCompletion),
}

/// Run the fault shim for an owned-ring write. Every arm the pool's
/// admission applies applies here — poison, sector error, torn write,
/// write stall, power-cut capture — EXCEPT the device latency, which the
/// pool serves from its deadline lane and an owned ring models as a
/// linked `TIMEOUT` on the ring itself (so the owning lane's late-CQE
/// path is what a slow device exercises).
fn intercept_inline_write(path: &Path, ops: Vec<(u64, bytes::Bytes)>) -> InlineVerdict {
    if !FAULTS_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        return InlineVerdict::Proceed { ops, latency: None };
    }
    let latency = FAULT_STATE
        .lock()
        .unwrap()
        .latency
        .get(path)
        .map(|l| l.write)
        .filter(|d| !d.is_zero());
    let (tx, rx) = oneshot::channel();
    let submitted_at = std::time::Instant::now();
    // `Delayed` skips the shim's latency arm (served above); every other
    // arm applies verbatim.
    let req = FsReq::Delayed(Box::new(FsReq::WriteAtBatch {
        path: path.to_path_buf(),
        ops,
        tx,
    }));
    match fault_intercept(req) {
        Some(FsReq::WriteAtBatch { ops, .. }) => InlineVerdict::Proceed { ops, latency },
        Some(_) => unreachable!("the shim returns the request it was given"),
        None => InlineVerdict::Consumed(WriteCompletion {
            rx,
            submitted_at,
            done: None,
        }),
    }
}

impl OwnedReactor {
    /// Submit every `(offset, bytes)` pair to `path` as one batch on THIS
    /// ring — admit + `submit()`, no wait — and return its completion.
    /// The fault shim is honored (module comment); an armed device
    /// latency rides as a linked timeout.
    pub fn submit_write_at_batch(
        &mut self,
        path: &Path,
        ops: Vec<(u64, bytes::Bytes)>,
    ) -> WriteCompletion {
        let (ops, latency) = match intercept_inline_write(path, ops) {
            InlineVerdict::Proceed { ops, latency } => (ops, latency),
            InlineVerdict::Consumed(c) => return c,
        };
        let (tx, rx) = oneshot::channel();
        let submitted_at = std::time::Instant::now();
        let ts = latency.map(|d| {
            io_uring::types::Timespec::new()
                .sec(d.as_secs())
                .nsec(d.subsec_nanos())
        });
        // `ufs_admit` ≡ `ufs_submit` on an owned ring: the submitter IS
        // the admitter (queue_hop reads the SQE build, µs).
        self.r
            .admit_write_batch(path.to_path_buf(), ops, tx, submitted_at, ts.as_ref());
        // Non-blocking: hands the SQEs to the kernel (a buffered
        // block-device write punts to io-wq here; a filesystem that
        // completes it inline posts the CQE before this returns).
        if let Err(e) = self.r.ring.submit() {
            if e.kind() != std::io::ErrorKind::Interrupted {
                log::error!("uring-fs owned ring: submit failed: {e:?}");
                self.r.fail_all("uring-fs owned ring submit failed");
            }
        }
        WriteCompletion {
            rx,
            submitted_at,
            done: None,
        }
    }

    /// Park the owning thread in the ring until a completion or a wake
    /// arrives, or `tick` elapses (`true` = the tick fired). Exactly one
    /// wake `READ` SQE is kept armed across parks.
    pub fn park(&mut self, tick: std::time::Duration) -> bool {
        if !self.wake_armed {
            let sqe = io_uring::opcode::Read::new(
                io_uring::types::Fd(self.wake.fd.as_raw_fd()),
                (&mut *self.wake_buf as *mut u64).cast::<u8>(),
                std::mem::size_of::<u64>() as u32,
            )
            .build()
            .user_data(WAKE_UD);
            // SAFETY: `wake_buf` is boxed (stable address) and lives as long
            // as this reactor, which outlives every SQE it submits.
            if unsafe { self.r.ring.submission().push(&sqe) }.is_err()
                && self.r.ring.submit().is_ok()
            {
                // SAFETY: as above.
                let _ = unsafe { self.r.ring.submission().push(&sqe) };
            }
            self.wake_armed = true;
        }
        let ts = io_uring::types::Timespec::new()
            .sec(tick.as_secs())
            .nsec(tick.subsec_nanos());
        let args = io_uring::types::SubmitArgs::new().timespec(&ts);
        match self.r.ring.submitter().submit_with_args(1, &args) {
            Ok(_) => false,
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => true,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => false,
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                // No `IORING_FEAT_EXT_ARG` (pre-5.11): an untimed wait — the
                // eventfd makes wakes lossless, the tick backstop is lost.
                match self.r.ring.submit_and_wait(1) {
                    Ok(_) => false,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => false,
                    Err(e) => {
                        log::error!("uring-fs owned ring: wait failed: {e:?}");
                        self.r.fail_all("uring-fs owned ring wait failed");
                        false
                    }
                }
            }
            Err(e) => {
                log::error!("uring-fs owned ring: wait failed: {e:?}");
                self.r.fail_all("uring-fs owned ring wait failed");
                false
            }
        }
    }

    /// Reap every posted completion (the owning thread's per-iteration
    /// hook): outcomes fire their oneshots here, on the thread that polls
    /// the tasks awaiting them.
    pub fn service(&mut self) {
        if self.r.reap() {
            self.wake_armed = false;
        }
    }
}

/// Classical blocking service loop, used only when the kernel cannot give us
/// a ring (or the ring died): callers still complete, loudly logged once.
fn blocking_fallback_loop(rx: crossbeam::channel::Receiver<FsReq>) {
    use std::os::unix::fs::FileExt;
    while let Ok(req) = rx.recv() {
        // Same fault-injection admission as the uring reactors (§4.7a).
        let req = if FAULTS_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            match fault_intercept(req) {
                Some(r) => r,
                None => continue,
            }
        } else {
            unwrap_delayed(req).0
        };
        match req {
            FsReq::Delayed(_) => unreachable!("unwrapped at admission"),
            FsReq::WriteAll { path, data, tx } => {
                let res = (|| -> std::io::Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, &data)
                })();
                send_now(tx, res.map_err(map_io));
            }
            FsReq::ReadAll { path, tx } => {
                let _ = tx.send(std::fs::read(&path).map(bytes::Bytes::from).map_err(map_io));
            }
            FsReq::ReadAt {
                path,
                offset,
                size,
                tx,
            } => {
                use std::os::unix::fs::OpenOptionsExt;
                let (span_off, span_len, skip, grain) = read_span(&path, offset, size);
                let direct = meta_io_mode(&path).is_some_and(|m| m.direct);
                let res = OpenOptions::new()
                    .read(true)
                    .custom_flags(if direct { libc::O_DIRECT } else { 0 })
                    .open(&path)
                    .and_then(|f| {
                        let mut buf = read_buf_for(&path, span_len);
                        let mut filled = 0usize;
                        while filled < span_len {
                            let n = f.read_at(
                                &mut buf.as_mut_slice()[filled..],
                                span_off + filled as u64,
                            )?;
                            if n == 0 {
                                break;
                            }
                            filled += n;
                            if filled % grain != 0 {
                                break;
                            }
                        }
                        let end = filled.min(skip + size);
                        Ok(if end <= skip {
                            bytes::Bytes::new()
                        } else {
                            buf.into_bytes().slice(skip..end)
                        })
                    })
                    .map_err(map_io);
                let _ = tx.send(res);
            }
            FsReq::WriteAt {
                path,
                offset,
                mut data,
                tx,
            } => {
                use std::os::unix::fs::OpenOptionsExt;
                let mode = meta_io_mode(&path);
                let res = prepare_direct_write(mode, &path, offset, &mut data).and_then(|()| {
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .custom_flags(if mode.is_some_and(|m| m.direct) {
                            libc::O_DIRECT
                        } else {
                            0
                        })
                        .open(&path)
                        .and_then(|f| f.write_all_at(&data, offset))
                        .map_err(map_io)
                });
                send_now(tx, res);
            }
            FsReq::WriteAtBatch { path, mut ops, tx } => {
                use std::os::unix::fs::OpenOptionsExt;
                let mode = meta_io_mode(&path);
                let res = ops
                    .iter_mut()
                    .try_for_each(|(off, d)| prepare_direct_write(mode, &path, *off, d))
                    .and_then(|()| {
                        OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .custom_flags(if mode.is_some_and(|m| m.direct) {
                                libc::O_DIRECT
                            } else {
                                0
                            })
                            .open(&path)
                            .and_then(|f| {
                                for (offset, data) in &ops {
                                    f.write_all_at(data, *offset)?;
                                }
                                Ok(())
                            })
                            .map_err(map_io)
                    });
                send_now(tx, res);
            }
            FsReq::Fdatasync { path, tx } => {
                let res = OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.sync_data())
                    .map_err(map_io);
                let _ = tx.send(res);
            }
        }
    }
}

/// Expose for tests: queue capacity constant.
pub const QUEUE_CAP: usize = URING_FS_QUEUE_CAP;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_uring_fs_write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        let payload = bytes::Bytes::from(vec![0xABu8; 12_345]);
        write_all(&path, payload.clone()).await.expect("write");
        let got = read_all(&path).await.expect("read");
        assert_eq!(got, payload);
        fdatasync(&path).await.expect("fdatasync");
    }

    #[tokio::test]
    async fn test_uring_fs_write_creates_parents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.dat");
        write_all(&path, b"hi".as_slice())
            .await
            .expect("nested write");
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
    }

    /// The live worker is a pool, not a single thread — otherwise a blocked
    /// `fdatasync` serializes all meta I/O (the small-write bottleneck).
    #[test]
    fn test_uring_fs_runs_a_worker_pool() {
        assert!(
            URING_FS._threads.len() >= 4,
            "uring-fs must run a worker pool (got {} threads)",
            URING_FS._threads.len()
        );
    }

    /// Open-mode regression: `fdatasync` then `read_at` on the same path must not
    /// return EBADF. `fdatasync` caches its fd; if opened write-only, the cached
    /// fd is unreadable and io_uring Read fails. Repeated across many paths so it
    /// exercises the pooled cache-hit path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_uring_fs_fdatasync_then_read_no_ebadf() {
        let dir = tempdir().unwrap();
        let mut handles = Vec::new();
        for i in 0..64u32 {
            let path = dir.path().join(format!("fsync_then_read_{i}.bin"));
            handles.push(tokio::spawn(async move {
                let payload = bytes::Bytes::from(vec![(i % 251) as u8; 4096]);
                write_at(&path, 0, payload.clone()).await.expect("write_at");
                fdatasync(&path).await.expect("fdatasync");
                let got = read_at(&path, 0, payload.len()).await.expect("read_at");
                assert_eq!(got, payload, "read after fdatasync mismatch for {i}");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    /// Many concurrent write_at + fdatasync + read_at ops must not corrupt each
    /// other when serviced by different pool workers (own ring / open_cache each).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_uring_fs_concurrent_ops_integrity() {
        let dir = tempdir().unwrap();
        let mut handles = Vec::new();
        for i in 0..64u32 {
            let path = dir.path().join(format!("concurrent_{i}.bin"));
            handles.push(tokio::spawn(async move {
                let len = 4096 + i as usize;
                let payload = bytes::Bytes::from(vec![(i % 251) as u8; len]);
                write_at(&path, 0, payload.clone()).await.expect("write_at");
                fdatasync(&path).await.expect("fdatasync");
                let got = read_at(&path, 0, len).await.expect("read_at");
                assert_eq!(got, payload, "data corrupted for file {i} under pool");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }
}
