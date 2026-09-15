//! The **durable job fabric** (PR VL2, design-volume-lifecycle §5.1) and
//! the **VL4 movers** (§5.4 data-volume drain / §5.3-step-6 rebalance).
//!
//! Coordinator-side core: durable schema-versioned job records on
//! ino 1 (`job:` xattrs — v3 whole-tx atomicity + torn-write immunity
//! for free, offline-probe visible, behind the FUSE reserved-namespace
//! screen), a local worker pool with the percentage **duty-cycle
//! throttle** (KD-3, live-retunable), pause/resume/cancel control, a
//! bounded checkpoint cadence, and **crash-resume by plan
//! regeneration** (KD-6: a restarted fabric re-scans the records and
//! re-plans; the progress record is advisory, never correctness-
//! bearing).
//!
//! PR VL4 adds the mover job types on the same fabric:
//!
//! * [`JobType::EvacuateVolume`] — the `evacuate-data-volume` job:
//!   census-planned copy-then-republish CoW (KD-6), shared-block
//!   move-once with pre-publish refcount transfer (§5.4 step 2),
//!   quiescent-first deferral of live-buffered blocks (§5.4 step 3),
//!   re-plan convergence, checkpoint-time capacity re-verification with
//!   `paused-capacity` self-pause (§5.2), and the retire commit.
//! * [`JobType::Rebalance`] — the same mover with the §5.3-step-6
//!   bounded-pass objective (bring under-filled volumes to within 10
//!   percentage points of the set-mean fill), auto-submitted by
//!   `volume add-data` unless `--no-rebalance` (KD-12).
//!
//! Mover discipline is §5.1.5 verbatim: no inode write guard across the
//! copy (P1-8 — the copy is lock-free data plane over `NvmeBlockDev`
//! io_uring); publish via `merge_block_mappings` (`MergeExpected`) under
//! the ino's CURRENT fencing token — tokens are READ, never incremented,
//! by movers; the durable copy completes before any meta commit is
//! touched (P1-10 by construction); displaced sources free through
//! `free_block`'s clone-aware `begin_free → purge → punch → finish_free`.

use crate::fuse_client::METRICS;
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use serde::{Deserialize, Serialize};
use squeezefs_ipc::sqz_notify::Notify;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Reserved xattr prefix for fabric records on ino 1 (§5.1.2; screened
/// by the VAL-2 allowlist —
/// [`crate::meta_backend::kv::backend::xattr_name_allowed`] — at both the
/// FUSE boundary and the backend's `Metadata` xattr entry points).
pub const JOB_XATTR_PREFIX: &str = "job:";

/// Root ino: job records live on the volume root (the same home as the
/// format-config record).
const ROOT_INO: u64 = 1;

/// Checkpoint cadence (§5.1.2): every N completed tasks or every 5 s,
/// whichever first — bounded re-work on crash.
const CHECKPOINT_TASKS: u64 = 256;
const CHECKPOINT_SECS: u64 = 5;

/// TEST SEAM (RES-7, `tests/job_worker_panic_tests.rs`): panic inside a
/// `Noop` job after N tasks. One relaxed load per task, zero cost when
/// unset, no `#[cfg(test)]` fork — the `TEST_PUBLISH_PASS_DELAY_MS` /
/// `SQUEEZEFS_TEST_WRITE_STALL_MS` pattern. A worker panic is otherwise
/// only reachable through a genuine bug, and load-dependent worker loss
/// is exactly the class the guard must pin deterministically.
static TEST_JOB_PANIC_AFTER: AtomicU64 = AtomicU64::new(0);

/// Arm (`n > 0`) or disarm (`0`) the job-panic seam. Production never
/// calls it.
pub fn set_test_job_panic_after(n: u64) {
    TEST_JOB_PANIC_AFTER.store(n, Ordering::Relaxed);
}

/// Test seam (red-first repro for the lost-resume race, 2026-08-09 gate
/// strand): stretch the worker's paused-park window — the gap between
/// the durable `checkpoint_as(Paused)` and the live state flip — by N
/// ms, so a racing `resume()` deterministically lands inside it. Under
/// gate load that window is wide (the checkpoint contends with the
/// control verbs' own checkpoints on the same conveyor); in isolation
/// it is microscopic, which is exactly why the strand was
/// load-dependent. Zero cost unset — the `TEST_JOB_PANIC_AFTER` /
/// `SQUEEZEFS_TEST_WRITE_STALL_MS` pattern.
static TEST_JOB_PARK_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Paused-park arm entries (monotonic). Tests synchronize on "the
/// worker is INSIDE its park window" by polling this across a control
/// verb — there is no other honest observable for that position, since
/// both the pause verb and the worker's park write the same durable
/// state.
static JOB_PARK_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Arm (`ms > 0`) or disarm (`0`) the park-window seam. Production
/// never calls it.
pub fn set_test_job_park_delay_ms(ms: u64) {
    TEST_JOB_PARK_DELAY_MS.store(ms, Ordering::Relaxed);
}

/// Total paused-park arm entries (test synchronization instrument for
/// the park-window seam).
pub fn job_park_entries() -> u64 {
    JOB_PARK_ENTRIES.load(Ordering::Relaxed)
}

/// VL10 (G-VL-3 b): how many independent block moves an UNTHROTTLED
/// drain/rebalance pass keeps in flight (join_all window). Each move is
/// device-I/O-bound (read + write + verify-read per block); 4 overlaps
/// the round-trips without meaningfully raising the R5-gauged copy
/// budget (4 × block_size on `job_copy_buffers`). Throttled passes and
/// defrag (order-dependent D1/D2 placement floors) stay width-1.
const MOVER_PIPELINE_WIDTH: usize = 4;

/// §5.2: the in-flight mover window per worker — an in-flight moved
/// block occupies source AND destination until the post-publish free;
/// the preflight `transient` term bounds the double-count by this
/// window × the ACTUAL configured worker count.
pub const EVACUATE_INFLIGHT_WINDOW_BLOCKS: u64 = 64;

/// §5.2: the headroom floor — foreground growth slack the preflight
/// always reserves even when the write-rate estimate is zero.
pub const DRAIN_HEADROOM_FLOOR_BYTES: u64 = 1 << 30;

/// The G-VL-3(b)-derived floor rate the preflight's INITIAL
/// `drain_eta` uses (½ of a ~1 GiB/s devsub-class raw copy). The live
/// checkpoint re-verification replaces it with the measured job rate —
/// which is what actually protects the survivors (§5.2).
pub const DRAIN_FLOOR_RATE_BYTES_PER_SEC: u64 = 512 << 20;

/// KD-12: the conservative default throttle of the auto-submitted
/// rebalance pass on `volume add-data`.
pub const REBALANCE_DEFAULT_THROTTLE_PCT: u32 = 25;

/// §5.3-step-6 / §5.7 rebalance objective: bring every under-filled
/// volume to within this many percentage points of the set-mean fill.
const REBALANCE_BAND: f64 = 0.10;

/// Backoff between re-plan passes when a pass made no progress
/// (deferred blocks waiting on quiescence, in-flight victim
/// allocations waiting on publish).
const REPLAN_BACKOFF: Duration = Duration::from_millis(200);

/// PR VL7 (§5.7): the defrag re-plan pass cap — the termination belt
/// under sustained foreground churn (each pass converges toward the
/// compacted/ordered fixpoint; churn can keep minting new work forever,
/// and a maintenance job must complete best-effort, never wedge).
const DEFRAG_MAX_PASSES: u32 = 16;

/// Job kinds. `Noop` is the fabric's own test/soak vehicle (each task
/// sleeps `task_ms`, making duty cycle and progress directly
/// measurable with zero I/O); the movers are PR VL4.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    Noop {
        tasks: u64,
        task_ms: u64,
    },
    /// §5.4 `evacuate-data-volume`: drain every referenced block off the
    /// victim volume, then retire it.
    EvacuateVolume {
        volume_id: String,
    },
    /// §5.3-step-6 / §5.7 `rebalance`: one bounded pass of the same
    /// mover moving blocks from above-mean volumes toward under-filled
    /// ones (within the 10 pp band).
    Rebalance,
    /// PR VL5b (§5.5.2): the `migrate-meta-slot` job — one routing
    /// slot's records move to another meta volume via the online
    /// migration engine (bulk copy + conveyor delta tee + cutover gate
    /// + §5.5.2b flip). Crash-resume: the adopted record re-runs the
    /// engine, which is idempotent across every crash window.
    MigrateMetaSlot {
        slot: u16,
        /// Target volume index in the CANONICAL member order.
        target_volume: usize,
    },
    /// PR VL6a (§5.6): the online fsck. `scrub` adds the C7 data scrub
    /// (KD-17); `scrub_only` is the `squeezefs scrub` spelling. The
    /// verified report persists as `job:{id}:report` (probe-readable
    /// offline). Detection never mutates; PR VL6b adds `repair`
    /// (§5.6a): dry-run planning by default, `apply` executes the
    /// per-class actions on the coordinator under per-object leases
    /// (quarantine home override via `quarantine_dir`). Serde defaults
    /// keep pre-VL6b durable job records decodable.
    Fsck {
        scrub: bool,
        scrub_only: bool,
        #[serde(default)]
        repair: bool,
        #[serde(default)]
        apply: bool,
        #[serde(default)]
        quarantine_dir: Option<String>,
    },
    /// PR VL7 (§5.7 D1/D2): the data defragmenter — the SAME `move_one`
    /// engine as the drain/rebalance movers with the contiguity-aware
    /// destination pick (D1: tail blocks into low-offset same-backend
    /// gaps; D2: locality rewrites of refcount-1 quiescent blocks onto
    /// one backend, ascending). `volume_id = None` covers every
    /// placement-eligible volume. Converges by re-plan until a pass
    /// moves nothing (best-effort under foreground churn — the next
    /// invocation re-plans from current state, KD-6).
    DefragData {
        volume_id: Option<String>,
    },
    /// PK6 (design-small-file-packing §5.8 / §5.11 — the D1 PACK face):
    /// the re-pack compaction mover, `squeezefs defrag --pack`. Plan =
    /// pack blocks whose live bytes fit the derived half-chunk line
    /// (`defrag::is_pack_victim` — the own-block threshold's other face),
    /// ascending; per victim, each distinct live `off` window is read once
    /// at its `max(len)` and re-packed into the OPEN pack block
    /// (`DataRouter::repack_window`), every referencer republished to the
    /// shared destination `off'` with its own `len` under its CURRENT
    /// token, and the victim frees only when its population reaches 0
    /// through the ordinary terminal ladder. The legacy one-block-per-file
    /// population is its first customer. Gated by the packing lever (the
    /// whole arm); mover-class (the VL9 serialize-loud pin); crash-resume
    /// by plan regeneration (KD-6). `volume_id = None` covers every
    /// placement-eligible volume.
    DefragPack {
        volume_id: Option<String>,
    },
    /// PR VL7 (§5.7 D4): nudge dead-bset-heavy KV leaves through the
    /// EXISTING SMO compactor (`KvMetaBackend::defrag_compact_nodes` —
    /// never a new compactor), per meta volume.
    DefragMeta,
    /// PR VL7 (§5.7 D3): kick every parked/spilled extent block through
    /// the EXISTING W2 fold machinery (the mount-wired [`FoldHook`]).
    DefragFold,
    /// PR 6b (design-kvmap-block-map-tree §3/A2): the background
    /// truncate/unlink sweep of one kvmap ino — chunked record-true map
    /// Deletes + reference releases + reclaim enqueues, resumable from
    /// the durable `;sweep:K` head cursor (KD-6: the cursor IS the plan
    /// — submitted at the size-flip handoff AND regenerated at mount by
    /// [`adopt_kvmap_sweeps`]'s cursor-head scan). The terminal chunk
    /// clears the cursor; a corpse owner (`nlink == 0`) is then
    /// destroyed (record + xattrs, one tx).
    KvmapSweep {
        ino: u64,
    },
}

impl JobType {
    /// The a-priori task count. Movers plan by census — their totals
    /// are discovered per pass and published live to the control block.
    fn tasks_total(&self) -> u64 {
        match self {
            JobType::Noop { tasks, .. } => *tasks,
            JobType::EvacuateVolume { .. }
            | JobType::Rebalance
            | JobType::MigrateMetaSlot { .. }
            | JobType::Fsck { .. }
            | JobType::DefragData { .. }
            | JobType::DefragPack { .. }
            | JobType::DefragMeta
            | JobType::DefragFold
            | JobType::KvmapSweep { .. } => 0,
        }
    }

    /// PR VL9 (pin a): the mover-class volume scope this job mutates
    /// block placement over — `None` for non-movers (fsck, slot
    /// migration, folds, meta compaction, Noop all interleave freely).
    /// Intersecting scopes SERIALIZE at claim time: one mover-class job
    /// per volume at a time, the later job queues loud (KD-6
    /// idempotence is what makes queueing safe — a queued mover
    /// re-plans from whatever state the earlier one left).
    fn mover_scope(&self) -> Option<MoverScope> {
        match self {
            JobType::EvacuateVolume { volume_id } => Some(MoverScope::Volume(volume_id.clone())),
            JobType::DefragData { volume_id: Some(v) } | JobType::DefragPack { volume_id: Some(v) } => {
                Some(MoverScope::Volume(v.clone()))
            }
            // Whole-set movers: rebalance plans sources/destinations
            // across the set; an unscoped defrag covers every
            // placement-eligible volume (the compaction's destination —
            // the open pack — is placed set-wide too).
            JobType::Rebalance
            | JobType::DefragData { volume_id: None }
            | JobType::DefragPack { volume_id: None } => Some(MoverScope::WholeSet),
            JobType::Noop { .. }
            | JobType::MigrateMetaSlot { .. }
            | JobType::Fsck { .. }
            | JobType::DefragMeta
            | JobType::DefragFold
            // The sweep frees blocks but never PLACES any; per-ino
            // serialization is the chunk's own held 4a.
            | JobType::KvmapSweep { .. } => None,
        }
    }

    /// Whether the §5.1.6 wire may execute this job on a remote worker.
    /// The VL4 movers are LOCAL-POOL ONLY in v1.1: their publish step is
    /// coordinator-side `merge_block_mappings` per referencing ino,
    /// which the wire's single verify-then-publish shard shape does not
    /// carry yet (the copy step is wire-capable — `ShardDeviceSeam`
    /// grew `read_source` and the production `RouterShardDevice` exists
    /// — but a remote worker completing a mover job without the meta
    /// publish would be a lie, so the dispatcher must not claim them).
    /// `Fsck` is likewise coordinator-local: the C1–C6 scan reads the
    /// live daemon's RAM-authoritative state by design (§5.1.6
    /// division), and while C7 scrub reads are DESIGNED distributable,
    /// the wire dispatches whole jobs only — shipping scrub sub-shards
    /// over the read-shard seam is a stated follow-up, not silently
    /// half-shipped.
    pub(crate) fn wire_executable(&self) -> bool {
        matches!(self, JobType::Noop { .. })
    }
}

/// PR VL9 (pin a): a mover-class job's placement scope. Per-volume
/// scopes conflict only on the same volume; a whole-set mover
/// conflicts with every mover.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MoverScope {
    Volume(String),
    WholeSet,
}

impl MoverScope {
    fn conflicts(&self, other: &MoverScope) -> bool {
        match (self, other) {
            (MoverScope::Volume(a), MoverScope::Volume(b)) => a == b,
            _ => true,
        }
    }
}

/// Job lifecycle states. Terminal = `Completed`/`Cancelled`/`Failed`.
/// `PausedCapacity` is the §5.2 self-pause: the drain's checkpoint-time
/// capacity re-verification found the survivors' slack consumed and
/// parked the job loudly instead of running them to StorageFull —
/// `job resume` is the operator's call once space is freed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    #[serde(rename = "paused-capacity")]
    PausedCapacity,
    Cancelled,
    Completed,
    Failed,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Cancelled | JobState::Completed | JobState::Failed
        )
    }

    fn is_paused(self) -> bool {
        matches!(self, JobState::Paused | JobState::PausedCapacity)
    }
}

/// The durable `job:{id}` record (schema v1). One JSON value, well
/// under the xattr cap; large plans shard into `job:{id}:shard:{k}`
/// records (the wire's shape).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobRecord {
    pub schema: u32,
    pub job_id: String,
    pub job_type: JobType,
    pub state: JobState,
    pub throttle_pct: u32,
    pub created_by: String,
    pub created_ts: u64,
    /// Advisory progress (checkpointed; correctness is plan
    /// regeneration, KD-6).
    pub tasks_done: u64,
    pub tasks_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Submission parameters.
#[derive(Clone, Debug)]
pub struct JobSpec {
    pub job_type: JobType,
    /// Duty-cycle percentage (KD-3); `0`/`≥100` = unthrottled.
    pub throttle_pct: u32,
}

/// A point-in-time status view (live map when running, durable record
/// otherwise).
#[derive(Clone, Debug)]
pub struct JobStatus {
    pub job_id: String,
    pub state: JobState,
    pub tasks_done: u64,
    pub tasks_total: u64,
    pub throttle_pct: u32,
}

/// Encode a durable **Queued** job record for an offline lifecycle verb
/// to commit alongside its own transaction (§5.3: an offline
/// `volume add-data` writes the rebalance record the next mount's
/// fabric adopts). Returns `(xattr_name, json_bytes)`.
pub fn durable_queued_job_xattr(job_type: &JobType, throttle_pct: u32) -> (String, Vec<u8>) {
    let rec = JobRecord {
        schema: 1,
        job_id: uuid::Uuid::new_v4().to_string(),
        job_type: job_type.clone(),
        state: JobState::Queued,
        throttle_pct,
        created_by: format!("{}:{} (offline)", hostname_lossy(), std::process::id()),
        created_ts: unix_ts(),
        tasks_done: 0,
        tasks_total: job_type.tasks_total(),
        error: None,
    };
    let name = format!("{JOB_XATTR_PREFIX}{}", rec.job_id);
    let bytes = serde_json::to_vec(&rec).expect("job record serializes");
    (name, bytes)
}

// ---------------------------------------------------------------------------
// §5.2 capacity preflight — the closed form (G-VL-3 e)
// ---------------------------------------------------------------------------

/// The §5.2 preflight terms for removing a data volume. The closed
/// form is normative: **refuse iff
/// `avail < needed + transient + headroom`** — and a refusal prints the
/// exact numbers (honest refusal).
#[derive(Clone, Copy, Debug)]
pub struct DrainPreflight {
    /// Deduped census of referenced bytes on the victim (a clone-shared
    /// block counts ONCE — the §5.4 move-once design).
    pub needed_bytes: u64,
    /// Σ free over healthy, `active` survivors (the victim excluded).
    pub avail_bytes: u64,
    /// The in-flight double-count window: 64 blocks × ACTUAL workers.
    pub transient_bytes: u64,
    /// `max(write_rate_est × drain_eta, 1 GiB)` — foreground growth
    /// slack, re-verified at every checkpoint.
    pub headroom_bytes: u64,
}

impl DrainPreflight {
    pub fn required_bytes(&self) -> u64 {
        self.needed_bytes
            .saturating_add(self.transient_bytes)
            .saturating_add(self.headroom_bytes)
    }

    pub fn admits(&self) -> bool {
        self.avail_bytes >= self.required_bytes()
    }

    /// The honest refusal: every §5.2 term with its exact number.
    pub fn refusal(&self) -> String {
        format!(
            "capacity preflight refused: avail {} B < needed {} B + transient {} B + \
             headroom {} B (= {} B required) — design-volume-lifecycle §5.2",
            self.avail_bytes,
            self.needed_bytes,
            self.transient_bytes,
            self.headroom_bytes,
            self.required_bytes()
        )
    }
}

/// §5.2 `transient`: copies live on both sides until the post-publish
/// free — 64 blocks × the ACTUAL configured worker count.
pub fn drain_transient_bytes(workers: usize, block_size: u64) -> u64 {
    EVACUATE_INFLIGHT_WINDOW_BLOCKS
        .saturating_mul(workers as u64)
        .saturating_mul(block_size)
}

/// §5.2 `headroom = max(write_rate_est × drain_eta, 1 GiB)`.
pub fn drain_headroom_bytes(write_rate_bytes_per_sec: u64, drain_eta_secs: u64) -> u64 {
    write_rate_bytes_per_sec
        .saturating_mul(drain_eta_secs)
        .max(DRAIN_HEADROOM_FLOOR_BYTES)
}

/// The §5.2 `write_rate_est` instrument: trailing mean of foreground
/// block consumption from the stats counters —
/// `Δ(write_through_blocks) + Δ(extent_spills)` blocks over the trailing
/// window (≤ 60 s), sampled at the coordinator's checkpoint cadence.
/// (The design also names staged-flush promotions; those land in
/// `write_through_blocks`-adjacent accounting and are covered by the
/// headroom floor — stated honestly in the PR record.)
pub struct WriteRateEstimator {
    samples: parking_lot::Mutex<std::collections::VecDeque<(std::time::Instant, u64)>>,
}

impl WriteRateEstimator {
    pub fn new() -> Self {
        Self {
            samples: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    fn counter_now() -> u64 {
        METRICS
            .write_through_blocks
            .load(Ordering::Relaxed)
            .saturating_add(METRICS.extent_spills.load(Ordering::Relaxed))
    }

    /// Record one sample (checkpoint cadence).
    pub fn observe(&self) {
        let mut s = self.samples.lock();
        let now = std::time::Instant::now();
        s.push_back((now, Self::counter_now()));
        while let Some((t, _)) = s.front() {
            if now.duration_since(*t) > Duration::from_secs(60) && s.len() > 2 {
                s.pop_front();
            } else {
                break;
            }
        }
    }

    /// Trailing-window mean, in bytes/s. Zero until two samples exist.
    pub fn bytes_per_sec(&self, block_size: u64) -> u64 {
        let s = self.samples.lock();
        let (Some((t0, c0)), Some((t1, c1))) = (s.front(), s.back()) else {
            return 0;
        };
        let secs = t1.duration_since(*t0).as_secs_f64();
        if secs < 0.001 {
            return 0;
        }
        let blocks = c1.saturating_sub(*c0) as f64;
        (blocks * block_size as f64 / secs) as u64
    }
}

impl Default for WriteRateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Mover context + test hook
// ---------------------------------------------------------------------------

/// Quiescence probe (§5.4 step 3): `true` = block `(ino, block_idx)`
/// carries NO live active buffer, parked extents, or spilled
/// `active_block_ext:` record — safe to move now.
pub type QuiesceProbe = Arc<dyn Fn(u64, u64) -> bool + Send + Sync>;

/// PR VL7 (§5.7 D3): the fold surface the mount wires into the fabric —
/// defrag DRIVES the existing W2 fold machinery, never reimplements it.
/// `targets` enumerates the current parked/spilled extent blocks;
/// `kick` runs one block's fold to completion
/// (`SqueezefsFilesystem::fold_extent_block` — `Ok(true)` = a fold ran).
pub struct FoldHook {
    pub targets: Arc<dyn Fn() -> Vec<(u64, u32)> + Send + Sync>,
    pub kick: Arc<
        dyn Fn(u64, u32) -> futures::future::BoxFuture<'static, Result<bool, String>> + Send + Sync,
    >,
}

/// What the movers need beyond the meta backend: the data router (block
/// I/O, merges, DLM token reads) and the quiescence probe. The mount
/// wires the FUSE layer's probe (RAM active buffers included) plus the
/// VL7 [`FoldHook`]; [`MoverCtx::router_only`] is the offline-coordinator
/// shape (staging records only — nothing else is live by construction,
/// and fold custody is mount-owned so no hook exists there).
pub struct MoverCtx {
    pub router: crate::routing::DataRouter,
    pub quiesce: QuiesceProbe,
    /// The §5.2 write-rate estimator, sampled at checkpoint cadence.
    pub write_rate: WriteRateEstimator,
    /// The D3 fold surface (`None` = no mounted FUSE layer: offline
    /// coordinators and fabric-only tests — `DefragFold` fails loud).
    pub fold: Option<FoldHook>,
}

impl MoverCtx {
    pub fn new(router: crate::routing::DataRouter, quiesce: QuiesceProbe) -> Self {
        Self {
            router,
            quiesce,
            write_rate: WriteRateEstimator::new(),
            fold: None,
        }
    }

    /// Wire the mount's D3 fold surface (the VL7 mount posture).
    pub fn with_fold(mut self, fold: FoldHook) -> Self {
        self.fold = Some(fold);
        self
    }

    /// The offline-coordinator probe: staging-visible state only (a
    /// D0-guarded coordinator process has no FUSE layer, so no RAM
    /// active buffers exist by construction).
    pub fn router_only(router: crate::routing::DataRouter) -> Self {
        let probe_router = router.clone();
        let quiesce: QuiesceProbe = Arc::new(move |ino, b| {
            // A staged-layout tenant with a RESIDENT ring entry is about
            // to be superseded (design-small-file-packing §5.11) — the
            // mover would copy dead bytes.
            if probe_router.staged_tenant_ring_resident(ino, b) {
                METRICS
                    .pack_mover_resident_defers
                    .fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let key = crate::keys::active_block(ino, b).to_string();
            let ext = crate::keys::active_block_ext(ino, b).to_string();
            !probe_router.cache.nvme.has_staged_active_block(&key)
                && !probe_router.cache.nvme.has_staged_extent_record(&ext)
        });
        Self::new(router, quiesce)
    }
}

/// Test hook: invoked with `(ino, block_idx)` immediately before each
/// mover publish attempt (after the copy, before the flush-lock/merge) —
/// the deterministic window the supersession and crash-injection tests
/// need. `None` in production; invoking a set hook may block the worker
/// (tests park it deliberately).
static EVAC_PRE_PUBLISH_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(u64, u32) + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_evacuate_pre_publish_hook(hook: Arc<dyn Fn(u64, u32) + Send + Sync>) {
    *EVAC_PRE_PUBLISH_HOOK.write() = Some(hook);
}

pub fn clear_evacuate_pre_publish_hook() {
    *EVAC_PRE_PUBLISH_HOOK.write() = None;
}

/// Hooks may PARK (the tests' crash/supersession windows), so a set hook
/// runs on the blocking pool and is awaited — the worker future stays
/// abortable at this await (a kill-9 analog can drop it mid-park).
async fn fire_pre_publish_hook(ino: u64, block_idx: u32) {
    let hook = EVAC_PRE_PUBLISH_HOOK.read().clone();
    if let Some(h) = hook {
        squeezefs_ipc::sqz_blocking::run_blocking(move || h(ino, block_idx)).await;
    }
}

// ---------------------------------------------------------------------------
// PR 6b: the kvmap A2 sweep's submit surface (design §3/A2)
// ---------------------------------------------------------------------------

/// The routing layer's handoff→fabric seam (the `EVAC_PRE_PUBLISH_HOOK`
/// registration shape): `truncate_layout`/`delete_file` cannot hold the
/// fabric (the router predates it at mount), so the mount wires this to
/// [`JobFabric::submit`]. Unwired (offline verbs, unit fixtures) the
/// durable cursor alone carries the plan — the next mount's
/// [`adopt_kvmap_sweeps`] regenerates the job (KD-6).
#[allow(clippy::type_complexity)]
static KVMAP_SWEEP_SUBMIT_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(u64) + Send + Sync>>> =
    parking_lot::RwLock::new(None);

/// Submit a [`JobType::KvmapSweep`] for `ino` through the wired fabric
/// (a no-op when unwired — the durable cursor is the crash-safe plan).
pub fn submit_kvmap_sweep(ino: u64) {
    let hook = KVMAP_SWEEP_SUBMIT_HOOK.read().clone();
    if let Some(h) = hook {
        h(ino);
    }
}

/// Wire [`submit_kvmap_sweep`] to `fabric` (the mount, and the test
/// venues). Deduped on live non-terminal jobs for the same ino —
/// duplicate sweeps are SAFE (every chunk re-reads the durable cursor
/// under the ino's held 4a and deletes record-true) but pointless.
pub fn wire_kvmap_sweep_submit(fabric: &Arc<JobFabric>) {
    let weak = Arc::downgrade(fabric);
    *KVMAP_SWEEP_SUBMIT_HOOK.write() = Some(Arc::new(move |ino: u64| {
        let Some(fabric) = weak.upgrade() else {
            return;
        };
        let dup = fabric
            .jobs_matching(|t| matches!(t, JobType::KvmapSweep { ino: i } if *i == ino))
            .into_iter()
            .any(|(_, st)| !st.is_terminal());
        if dup {
            return;
        }
        crate::meta_exec::spawn_meta("kvmap_sweep_submit", async move {
            if let Err(e) = fabric
                .submit(JobSpec {
                    job_type: JobType::KvmapSweep { ino },
                    throttle_pct: 0,
                })
                .await
            {
                log::error!(
                    "kvmap sweep submit for ino {ino} failed: {e} — the durable cursor \
                     stays the plan; the next mount's adoption scan regenerates the job"
                );
            }
        });
    }));
}

/// Unwire the submit hook (test hygiene; the mount never unwires).
pub fn clear_kvmap_sweep_submit() {
    *KVMAP_SWEEP_SUBMIT_HOOK.write() = None;
}

/// PR 6b (KD-6 — crash-resume by plan regeneration): the mount-time
/// **cursor-head scan**. The durable `;sweep:K` cursor IS the plan, so a
/// crash anywhere between the handoff commit and the job's terminal
/// chunk resumes here: one tree-7 owner SKIP-scan per volume (O(distinct
/// kvmap owners), never O(records)), each owner's durable head read for
/// a live cursor, and a [`JobType::KvmapSweep`] submitted for every
/// cursor no live/durable job already covers (the fabric's own record
/// adoption re-queues jobs that were durably submitted — this scan
/// closes the window where the cursor committed and the submit did not).
/// Corpses whose sweep already emptied the tree carry no cursor scan hit
/// and stay the mount corpse sweep's (their `delete_file` re-plants).
pub async fn adopt_kvmap_sweeps(
    fabric: &Arc<JobFabric>,
    router: &crate::routing::DataRouter,
) -> crate::error::Result<u64> {
    let meta = fabric.meta_handle();
    let mut resumed = 0u64;
    for (v_idx, kv) in meta.volumes.iter().enumerate() {
        if !kv.block_map_tree_engaged() {
            continue;
        }
        let owners = kv.block_map_owner_scan().await.map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "kvmap sweep adoption: tree-7 owner scan failed on vol {v_idx}: {e}"
            ))
        })?;
        for local_ino in owners {
            let Some(ino) = meta.try_make_global_ino(local_ino, v_idx) else {
                continue; // guest/control records — never sweep candidates
            };
            let Ok(Some(bytes)) = kv.getxattr(local_ino, "layout").await else {
                continue;
            };
            let cursor_live = crate::layout_wire::decode_layout_any(&bytes)
                .ok()
                .and_then(|l| l.block_map_id)
                .and_then(|id| crate::meta_backend::kv::block_map::parse_kvmap_head(&id).ok())
                .is_some_and(|h| h.sweep_cursor.is_some());
            if !cursor_live {
                continue;
            }
            let covered = fabric
                .jobs_matching(|t| matches!(t, JobType::KvmapSweep { ino: i } if *i == ino))
                .into_iter()
                .any(|(_, st)| !st.is_terminal());
            if covered {
                continue;
            }
            // A corpse handed off by a PRIOR era re-arms its
            // destroy-withholding mark too (the reclaim/mount-sweep
            // callers consult it).
            if matches!(meta.getattr(ino).await, Ok(inode) if inode.nlink == 0) {
                router.kvmap_sweep_corpse_mark(ino);
            }
            fabric
                .submit(JobSpec {
                    job_type: JobType::KvmapSweep { ino },
                    throttle_pct: 0,
                })
                .await?;
            resumed += 1;
            METRICS.map_sweep_resumed.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(resumed)
}

// ---------------------------------------------------------------------------
// Census planner (§5.4 step 1)
// ---------------------------------------------------------------------------

/// One referencer of a victim block.
#[derive(Clone, Debug)]
pub(crate) struct MoveRef {
    pub(crate) ino: u64,
    pub(crate) block_idx: u32,
    /// The mapping string VERBATIM as persisted (decoration included).
    pub(crate) mapping: String,
    /// The referencer's layout is the STAGED layout (a promoted/spilled
    /// small file whose whole payload is `block_map[0]`) — the population
    /// the pack occupancy face measures and the compaction mover moves
    /// (design-small-file-packing §5.8; a striped file's blocks are never
    /// pack tenants — its tail is a live write target, Non-Goals).
    pub(crate) staged: bool,
}

/// One move task: a distinct source base offset and every referencer —
/// a shared block moves ONCE (§5.4 step 2).
pub(crate) struct MoveTask {
    /// Clean base key (`clean_block_key` form) on the source volume.
    pub(crate) base_key: String,
    /// The source volume the base key parses to (rebalance uses it to
    /// exclude the source from destination picks).
    pub(crate) src_id: String,
    /// The source's parsed device byte offset (the VL7 compaction
    /// objective bounds its destination pick by it).
    pub(crate) src_offset: u64,
    pub(crate) refs: Vec<MoveRef>,
    /// Rebalance: the planned destination volume id (`None` = the
    /// lowest-fill eligible pick).
    dest_hint: Option<String>,
    /// PR VL7 (§5.7): the destination objective — `Balance` is the
    /// drain/rebalance behavior verbatim; the defrag objectives pick
    /// contiguity-aware destinations.
    dest_pick: DestPick,
}

/// PR VL7 (§5.7 D1/D2): the mover's destination objective.
#[derive(Clone, Debug)]
enum DestPick {
    /// Fill-balance (drain/rebalance): `dest_hint` or the lowest-fill
    /// eligible survivor.
    Balance,
    /// D1 compaction: the LOWEST free block on the SAME backend, strictly
    /// below the source offset — never a fresh tail mint (that would grow
    /// the tail being reclaimed). No such gap ⇒ the move is a no-op this
    /// pass. Tasks execute in (ino, logical-idx) order, so the ascending
    /// gap consumption preserves per-file physical order (no D2
    /// re-trigger).
    CompactLow,
    /// D2 locality rewrite: the named backend, lowest free at-or-above
    /// the file's shared ascending FLOOR (fresh tail fallback) — the
    /// floor is what makes one pass leave the file same-backend
    /// ascending, i.e. what makes the rewrite CONVERGE instead of
    /// chasing free-list order forever.
    BackendAscending {
        be_id: String,
        floor: Arc<AtomicU64>,
    },
}

/// A census pass over the durable inode trees.
pub(crate) struct Census {
    pub(crate) tasks: Vec<MoveTask>,
    /// Global inos whose INDIRECT block-map blob lives on a source
    /// volume — relocated by an empty merge (the save path reallocates
    /// blobs off non-active volumes).
    blob_relocations: Vec<u64>,
    /// Distinct source blocks (tasks + blobs) — the §5.2 `needed`
    /// census.
    distinct_blocks: u64,
}

/// Whether a parsed backend id denotes `victim`, honoring the
/// `backend_0`/legacy default-slot aliases exactly like allocator
/// recovery does.
fn key_owned_by(be_id: &str, victim: &str, victim_is_default_slot: bool) -> bool {
    be_id == victim || ((be_id == "backend_0" || be_id == "squeezefs") && victim_is_default_slot)
}

/// Walk every meta volume's live inode tree and collect the blocks
/// whose keys parse to one of `sources` (the `df` census walk shape —
/// tree-walk-derived ground truth, deduped by offset: the §5.2 dedupe
/// census and the §5.4 move-once grouping in one pass). Also the pack
/// occupancy face's input (`crate::defrag::measure_pack_occupancy`, PK6)
/// — one walk, every referencer of every block with its mapping verbatim.
pub(crate) async fn census_for(
    meta: &Arc<RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
    sources: &[String],
) -> crate::error::Result<Census> {
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};

    let br = &router.backend_router;
    let default_slot_sources: std::collections::HashSet<&str> = sources
        .iter()
        .filter(|id| {
            br.backends.get(id.as_str()).is_some_and(|be| {
                Arc::ptr_eq(&be.device, &br.default_device)
                    && Arc::ptr_eq(&be.block_allocator, &br.default_allocator)
            })
        })
        .map(|s| s.as_str())
        .collect();
    let owner_of = |mapping: &str| -> Option<(String, String, u64)> {
        let clean = crate::routing::clean_block_key(mapping);
        let (be_id, offset) = br.parse_block_key(&clean).ok()?;
        for src in sources {
            if key_owned_by(&be_id, src, default_slot_sources.contains(src.as_str())) {
                return Some((src.clone(), clean, offset));
            }
        }
        None
    };

    let block_size = router.block_size.load(Ordering::Relaxed) as usize;
    let mut by_offset: HashMap<String, MoveTask> = HashMap::new();
    let mut blob_relocations = Vec::new();
    let mut blob_blocks = 0u64;

    for (vol_idx, kv) in meta.volumes.iter().enumerate() {
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = kv
                .range_kind(
                    crate::meta_backend::kv::record::TREE_INODES,
                    &cursor,
                    &end,
                    512,
                )
                .await
                .map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "evacuation census inode walk failed: {e}"
                    ))
                })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(local_ino) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                if val.nlink == 0 {
                    continue;
                }
                let Ok(Some(bytes)) = kv.getxattr(local_ino, "layout").await else {
                    continue;
                };
                let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
                    serde_json::from_slice(&bytes).ok()
                } else {
                    bincode::deserialize(&bytes).ok()
                };
                let Some(layout) = layout else { continue };
                // Guest-only members carry raw CONTROL records with no
                // global encoding — skip them (VL9 soak-found panic).
                let Some(global_ino) = meta.try_make_global_ino(local_ino, vol_idx) else {
                    continue;
                };

                // The block-map entries: inline, rehydrated from the
                // indirect blob (whose own block may also need moving), or
                // paged out of tree 7 for a `kvmap:` head.
                let mut entries: Vec<(u32, String)> = Vec::new();
                let mut indirect_on_source = false;
                if let Some(ref map_id) = layout.block_map_id {
                    if let Some(blob_key) = map_id.strip_prefix("indirect:") {
                        if owner_of(blob_key).is_some() {
                            indirect_on_source = true;
                        }
                        match br.read_block(blob_key, block_size).await {
                            Ok(raw) => {
                                if let Ok(decoded) = crate::routing::decode_indirect_block_map(&raw)
                                {
                                    entries = decoded;
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "evacuation census: ino {global_ino} indirect map at \
                                     '{blob_key}' unreadable: {e} — revisited next pass"
                                );
                            }
                        }
                    } else if map_id
                        .starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX)
                    {
                        // PR 4 (kvmap, design §3 walkers): tree-7 entries via
                        // the SHARED extraction (Rev 1.1 #3). No blob to
                        // relocate — map records are metadata, and the
                        // move_one republish rides merge_block_mappings,
                        // whose sticky-head save IS the kvmap arm (A10), so
                        // a moved mapping's record rewrite needs nothing
                        // here. A census that saw zero kvmap blocks would
                        // retire a volume with LIVE data still on it.
                        entries = br.kvmap_layout_entries(kv, local_ino).await;
                    }
                }
                if entries.is_empty() {
                    if let Some(ref bm) = layout.block_map {
                        entries = bm.iter().map(|(&b, key)| (b, key.clone())).collect();
                    }
                }
                if indirect_on_source {
                    blob_relocations.push(global_ino);
                    blob_blocks += 1;
                }
                let staged = layout.file_type == "staged";
                for (b, mapping) in entries {
                    let Some((src_id, clean, offset)) = owner_of(&mapping) else {
                        continue;
                    };
                    by_offset
                        .entry(clean.clone())
                        .or_insert_with(|| MoveTask {
                            base_key: clean,
                            src_id,
                            src_offset: offset,
                            refs: Vec::new(),
                            dest_hint: None,
                            dest_pick: DestPick::Balance,
                        })
                        .refs
                        .push(MoveRef {
                            ino: global_ino,
                            block_idx: b,
                            mapping,
                            staged,
                        });
                }
            }
        }
    }

    let mut tasks: Vec<MoveTask> = by_offset.into_values().collect();
    for t in &mut tasks {
        // Ascending-ino publish order (§5.4 step 2).
        t.refs.sort_by_key(|r| (r.ino, r.block_idx));
    }
    let distinct_blocks = tasks.len() as u64 + blob_blocks;
    Ok(Census {
        tasks,
        blob_relocations,
        distinct_blocks,
    })
}

/// §5.2 `avail`: Σ free over placement-eligible survivors (durable
/// state `active`, healthy), the sources excluded. Unbounded allocators
/// (capacity 0 — offline tools) contribute nothing: conservative.
fn avail_elsewhere(router: &crate::routing::DataRouter, exclude: &[String]) -> u64 {
    let br = &router.backend_router;
    let mut avail = 0u64;
    for entry in br.backends.iter() {
        let be_id = entry.key();
        if exclude.iter().any(|e| e == be_id) || !br.placement_eligible(be_id) {
            continue;
        }
        let alloc = &entry.value().block_allocator;
        let cap = alloc.capacity_bytes();
        if cap == 0 {
            log::warn!(
                "capacity preflight: volume '{be_id}' has no capacity bound — \
                 contributing 0 B to avail (conservative)"
            );
            continue;
        }
        avail = avail.saturating_add(
            cap.saturating_sub(alloc.get_used_blocks().saturating_mul(alloc.chunk_size())),
        );
    }
    avail
}

/// The full §5.2 preflight for removing `volume_id`: dedupe census on
/// the victim, survivor availability, the worker-window transient, and
/// the write-rate/ETA headroom. `workers` is the ACTUAL configured
/// worker count.
pub async fn drain_preflight(
    meta: &Arc<RoutedMetaBackend>,
    ctx: &MoverCtx,
    volume_id: &str,
    workers: usize,
) -> crate::error::Result<DrainPreflight> {
    let block_size = ctx.router.block_size.load(Ordering::Relaxed);
    let census = census_for(meta, &ctx.router, &[volume_id.to_string()]).await?;
    let needed_bytes = census.distinct_blocks.saturating_mul(block_size);
    ctx.write_rate.observe();
    let rate = ctx.write_rate.bytes_per_sec(block_size);
    let eta_secs = needed_bytes / DRAIN_FLOOR_RATE_BYTES_PER_SEC.max(1) + 1;
    Ok(DrainPreflight {
        needed_bytes,
        avail_bytes: avail_elsewhere(&ctx.router, &[volume_id.to_string()]),
        transient_bytes: drain_transient_bytes(workers, block_size),
        headroom_bytes: drain_headroom_bytes(rate, eta_secs),
    })
}

/// Charge scope for the copy window: `job_copy_buffer_bytes` (the R5
/// component) + `evacuate_inflight_bytes`, released on drop so an
/// aborted worker can never leak the gauges.
struct CopyCharge(u64);

impl CopyCharge {
    fn new(bytes: u64) -> Self {
        METRICS
            .job_copy_buffer_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        METRICS
            .evacuate_inflight_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Self(bytes)
    }
}

impl Drop for CopyCharge {
    fn drop(&mut self) {
        METRICS
            .job_copy_buffer_bytes
            .fetch_sub(self.0, Ordering::Relaxed);
        METRICS
            .evacuate_inflight_bytes
            .fetch_sub(self.0, Ordering::Relaxed);
    }
}

/// Live per-job control block: the workers' and control verbs' shared
/// truth between checkpoints. `pub(crate)` since PR VL2b: the §5.1.6
/// wire's coordinator is the second population driving the same claims
/// and completions (KD-1: one protocol, two transports).
pub(crate) struct JobCtl {
    pub(crate) job_type: JobType,
    paused: AtomicBool,
    cancelled: AtomicBool,
    /// Live-retunable duty cycle (workers re-read per task).
    pub(crate) throttle: AtomicU32,
    /// Tasks completed (live; checkpointed on cadence).
    pub(crate) done: AtomicU64,
    /// Planned tasks. Static for `Noop`; the movers publish their
    /// census-discovered totals here per pass (advisory progress).
    pub(crate) tasks_total: AtomicU64,
    /// One worker owns a job at a time (shard-level parallelism is the
    /// wire's business).
    claimed: AtomicBool,
    /// PR VL9 (pin a): whether this deferral episode was already
    /// counted/logged — `job_serialized_waits` counts episodes, not the
    /// worker pool's claim-poll cadence. Reset on claim.
    serialize_noted: AtomicBool,
    state: parking_lot::Mutex<JobState>,
    terminal: Notify,
}

/// RES-7: release-on-unwind for the worker-held claim — the `PassGuard`
/// shape the publish conveyor uses, applied to the one detached executor
/// that never had it. `claimed` is the VL9 pin-(a) mover-serialization
/// token, so leaking it wedges a whole volume scope permanently.
struct ClaimGuard {
    ctl: Arc<JobCtl>,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        self.ctl.claimed.store(false, Ordering::SeqCst);
    }
}

impl JobCtl {
    pub(crate) fn state(&self) -> JobState {
        *self.state.lock()
    }
    /// Terminal states never regress (2026-08-09 gate strand): the
    /// first terminal state wins — a worker park arm's or vehicle's
    /// stale flip landing after a `cancel()` must not overwrite it
    /// (the check-then-act `!is_terminal()` guards at the call sites
    /// raced the verbs; the refusal belongs under the state lock).
    fn set_state(&self, s: JobState) {
        let mut st = self.state.lock();
        if st.is_terminal() {
            return;
        }
        *st = s;
        if s.is_terminal() {
            self.terminal.notify_waiters();
        }
    }
    /// Settle the paused-park state flip against racing control verbs
    /// (the 2026-08-09 lost-resume strand): under the state lock, the
    /// `Paused` flip holds only while the pause intent is still
    /// standing — a `resume()` that landed inside the worker's
    /// checkpoint window wins (`Queued`, so the pool re-claims), and a
    /// terminal state is never regressed. Returns the state that holds.
    fn settle_park(&self) -> JobState {
        let mut st = self.state.lock();
        if st.is_terminal() {
            return *st;
        }
        if self.paused.load(Ordering::SeqCst) {
            let s = if *st == JobState::PausedCapacity {
                JobState::PausedCapacity
            } else {
                JobState::Paused
            };
            *st = s;
            s
        } else {
            *st = JobState::Queued;
            JobState::Queued
        }
    }
}

/// The coordinator-side fabric. One per mounted daemon (or per offline
/// D0-guarded coordinator process).
/// VL2 fabric worker default, pure form (tie-tested in the derivation
/// sweep; moved out of `main.rs`'s mount arm as KD-MW-14 rung 3c):
/// `(cpus / 4).clamp(2, 8)` — the L4 service posture's slope, floored so
/// a small box still runs a coordinator pair and ceilinged so a big one
/// does not spawn a worker farm for a duty-cycled maintenance plane.
/// `cpus` is the fleet-share-DIVIDED sizing root; the retired direct
/// `available_parallelism()` read in the mount arm ran on core-pinned
/// tokio workers (the Hang-1 pinned-first-toucher class).
pub fn fabric_workers_default(cpus: usize) -> usize {
    cpus.div_euclid(4).clamp(2, 8)
}

// ---------------------------------------------------------------------------
// KD-MW-16 (rung 10c, `docs/design-mw-fleet-jobs.md`): the fleet
// read-shard dispatch seam between the fabric's job executors and the
// §5.1.6 wire host.
// ---------------------------------------------------------------------------

/// One fleet shard's terminal outcome, delivered to the dispatching
/// executor: `Some(payload)` = the worker's fencing-checked proposal
/// (the shard report bytes); `None` = the shard was LOST (lease expiry,
/// worker abandon, refused/malformed proposal) and the caller must
/// re-lease it — re-dispatch or run the residue locally.
#[derive(Debug)]
pub struct FleetOutcome {
    pub shard: u32,
    pub payload: Option<Vec<u8>>,
    /// **KD-PV-16 / §5.8.2 clause 2**: the lease HOLDER's `worker_id`,
    /// filled by the wire from its OWN lease table — never from the
    /// payload. This is what makes the coordinator's inode-plane
    /// admission predicate evidence-based instead of self-declared: a
    /// fleet worker's `worker_id` IS its durable KD-MW-2 enrollment id
    /// (`cowriter::node_member_id`), the same identity space as
    /// `claim_set.owner` and `PeerOwner::peer_id`, so it is one owner-map
    /// lookup on an id the coordinator already trusts. `None` when no
    /// lease was held (a shard that never dispatched).
    pub worker_id: Option<String>,
}

/// The outcome channel a fleet shard set collects on (unbounded: a
/// shard set is bounded by the enrolled-worker population, and the
/// sender side runs on wire OS threads that must never park on a
/// backpressured executor channel).
pub type FleetOutcomeTx = squeezefs_ipc::sqz_channel::mpsc::UnboundedSender<FleetOutcome>;

/// The fleet read-shard dispatch seam (KD-MW-16): implemented by
/// [`crate::job_wire::JobWireHost`] and registered on the fabric at
/// wire start, consumed by the fsck fleet fan-out
/// (`crate::fsck::run_fleet`). A trait, deliberately: the fabric owns
/// jobs and the wire owns sessions/leases — this seam is the ONE edge
/// between them, and the in-process contracts drive it through the real
/// host.
pub trait FleetDispatch: Send + Sync {
    /// Idle, non-expired sessions advertising `CAP_FLEET_READ` — the
    /// population a fleet fan-out may shard across right now.
    fn read_capacity(&self) -> usize;
    /// Assign one ino-residue read shard (`shard_no` of `shard_count`)
    /// to an idle read-capable session. Returns `false` when no such
    /// session exists (the caller runs the residue locally). The shard
    /// rides the full lease law: TTL + heartbeats, fencing bump on
    /// expiry, stale proposals refused.
    fn dispatch_read_shard(
        &self,
        job_id: &str,
        shard_no: u32,
        shard_count: u32,
        job_type: &JobType,
        throttle_pct: u32,
        tx: &FleetOutcomeTx,
    ) -> bool;
    /// **KD-PV-16**: assign one INODE-PLANE shard to the session whose
    /// `worker_id` is `worker_id` — the owner of the volumes this shard
    /// is being asked about. Targeted, deliberately: the census fan-out
    /// picks any idle capable member because a residue is
    /// ownership-blind, while the inode plane's candidate scope IS
    /// ownership (KD-PV-7), so "some idle member" is never the right
    /// venue. Returns `false` when that owner has no idle, non-expired,
    /// read-capable session — the coordinator then reports its volumes
    /// UNCOVERED rather than judging them here (a peer's inos judged
    /// locally is the false-positive generator the scoping refuses).
    fn dispatch_inode_plane_shard(
        &self,
        job_id: &str,
        shard_no: u32,
        worker_id: &str,
        job_type: &JobType,
        throttle_pct: u32,
        tx: &FleetOutcomeTx,
    ) -> bool;
    /// Drop the job's fleet shard state at pass end (the coordinator's
    /// live map is per-`(job, shard)`; without retirement every fleet
    /// pass would grow it by N-1 forever). A zombie's LATE proposal
    /// still refuses stale after retirement — the unknown-shard arm is
    /// the same refusal class.
    fn retire_fleet_shards(&self, job_id: &str);
}

/// The shard-number space KD-PV-16's inode-plane shards live in, disjoint
/// from the census residues by construction: those are `1..n` where `n`
/// is one more than the enrolled read-capable SESSION population, which
/// the claim set's own member cap (≤ 16 writers) and any plausible fleet
/// keep many orders of magnitude below 2^20. Keeping one number space
/// means the wire's shard map, lease law, fencing identity and durable
/// `job:{id}:shard:{k}` records are the EXISTING ones, unchanged.
pub const INODE_PLANE_SHARD_BASE: u32 = 1 << 20;

/// **KD-PV-14** (`docs/design-per-volume-claim-admission.md` §5.4b): the
/// maintenance coordinator is the owner of the volume hosting **slot 0**
/// — D20's set authority, which is also where the `job:` records live
/// (they are written through the routed `setxattr(ROOT_INO, …)`, so
/// `daemon_verb_router` already ships them there). One predicate over
/// already-durable state; no election.
///
/// `None` = this node may coordinate, which is EVERY unarmed mount (the
/// shipped posture: `owner_map()` is `None`, one relaxed load, and the
/// answer is unchanged). `Some(refusal)` names the set authority, its
/// endpoint, and — because the two are easy to confuse — WHICH of the two
/// acts the operator hit: coordinator-class acts (minting a job record,
/// planning shards, applying repairs) are refused, an owner's
/// participation as a detection SHARD is not (KD-PV-16 fans the inode
/// plane out to every owner precisely because the alternative is 1/K
/// coverage; a shard is a fencing-checked result proposal on the existing
/// wire, not a second coordinator).
///
/// Why it exists: under the recipe EVERY partial authority holds a D0
/// claim on some volume, so the pre-recipe "do I hold the claim?"
/// predicate is true on all K nodes — K concurrent coordinators over one
/// set.
pub fn maintenance_coordinator_refusal() -> Option<String> {
    // PR 8 (design-symmetric-metadata §5.5): under the armed symmetric
    // plane the coordinator is VOLUME 0's MANAGER — one predicate ahead
    // of the per-volume-owner map; `None` on every unarmed mount (one
    // relaxed load).
    if let Some(refusal) = crate::meta_backend::kv::alloc_lease::symmetric_coordinator_refusal() {
        return Some(refusal);
    }
    let map = crate::meta_ship::owners::owner_map()?;
    if !map.multi_owner() || map.owns_slot_0() {
        return None;
    }
    let authority = map
        .set_authority()
        .map(|p| format!("'{}' at {}", p.peer_id, p.endpoint))
        .unwrap_or_else(|| "the owner of the slot-0 volume (unknown to this map)".to_string());
    Some(format!(
        "refusing to COORDINATE maintenance on this volume set: the maintenance coordinator is \
         the SET AUTHORITY — the owner of the volume hosting metadata slot 0 — and that is \
         {authority}, not this node (design-per-volume-claim-admission KD-PV-14/D20). Submit \
         fsck/defrag/job verbs there (`squeezefs volume get-owners` renders the map). This \
         refusal covers COORDINATOR-CLASS acts only — minting a job record, planning shards, \
         applying repairs; this node still SERVES the inode-plane and census shards the \
         coordinator leases to it (KD-PV-16), which is participation, not a second coordinator"
    ))
}

pub struct JobFabric {
    meta: Arc<RoutedMetaBackend>,
    jobs: parking_lot::Mutex<HashMap<String, Arc<JobCtl>>>,
    default_throttle: u32,
    workers: usize,
    /// The mover context (`None` = fabric-only wiring: unit tests, the
    /// pre-VL4 remote-wire tests — mover jobs FAIL loud without it).
    mover: Option<MoverCtx>,
    work: Notify,
    shutdown: AtomicBool,
    /// The OWNED worker pool (first-party cancel-gated set on the
    /// sqz-meta lanes): `shutdown_abrupt` cancels it — a cancelled
    /// worker's future is dropped at its next poll boundary, so its
    /// current await point never resumes and nothing later persists
    /// (the documented kill-9 analog the crash-resume soak proves).
    workers_set: squeezefs_ipc::sqz_taskset::OwnedSet,
    /// KD-MW-16: the fleet read-shard dispatch seam, registered by the
    /// wire host at start. `Weak`, deliberately — the host holds the
    /// fabric (`JobWireHost.fabric`), so a strong edge here would be an
    /// Arc cycle that leaks both past teardown (the `defrag_fold_hook`
    /// cycle-hygiene precedent).
    fleet: parking_lot::Mutex<Option<std::sync::Weak<dyn FleetDispatch>>>,
}

impl JobFabric {
    /// Start the fabric: adopt durable non-terminal records (crash-
    /// resume by plan regeneration), then spawn `workers` local pool
    /// tasks. `default_throttle` is the mount's `--job-cpu-limit`
    /// default applied when a job is submitted without an explicit
    /// throttle. `mover` wires the VL4 movers (the mount and the
    /// offline coordinator pass it; fabric-only tests pass `None`).
    pub async fn start(
        meta: Arc<RoutedMetaBackend>,
        workers: usize,
        default_throttle: u32,
        mover: Option<MoverCtx>,
    ) -> crate::error::Result<Arc<Self>> {
        let fabric = Arc::new(Self {
            meta,
            jobs: parking_lot::Mutex::new(HashMap::new()),
            default_throttle,
            workers: workers.max(1),
            mover,
            work: Notify::new(),
            shutdown: AtomicBool::new(false),
            workers_set: squeezefs_ipc::sqz_taskset::OwnedSet::new(
                "job_worker",
                crate::meta_exec::spawn_meta,
            ),
            fleet: parking_lot::Mutex::new(None),
        });

        // Crash-resume (KD-6): every durable non-terminal record is
        // adopted. Running/Queued re-queue (the plan regenerates from
        // the record); Paused stays paused (operator intent survives
        // the crash — capacity self-pauses included).
        for rec in Self::list_records(&fabric.meta).await? {
            if rec.state.is_terminal() {
                continue;
            }
            log::info!(
                "job fabric: adopting durable job {} (state {:?}, {}/{} tasks)",
                rec.job_id,
                rec.state,
                rec.tasks_done,
                rec.tasks_total
            );
            let adopted_state = if rec.state.is_paused() {
                rec.state
            } else {
                JobState::Queued
            };
            let ctl = Arc::new(JobCtl {
                job_type: rec.job_type.clone(),
                paused: AtomicBool::new(adopted_state.is_paused()),
                cancelled: AtomicBool::new(false),
                throttle: AtomicU32::new(rec.throttle_pct),
                // Plan regeneration, not trust: Noop's "current state"
                // is the checkpointed cursor (advisory); the movers
                // re-census (KD-6).
                done: AtomicU64::new(rec.tasks_done),
                tasks_total: AtomicU64::new(rec.tasks_total),
                claimed: AtomicBool::new(false),
                serialize_noted: AtomicBool::new(false),
                state: parking_lot::Mutex::new(adopted_state),
                terminal: Notify::new(),
            });
            fabric.jobs.lock().insert(rec.job_id.clone(), ctl);
        }

        // `workers == 0` is the remote-only posture (no local pool —
        // every shard rides the §5.1.6 wire; test/soak shape). Mounts
        // always pass the clamped L4-style pool size.
        for idx in 0..workers {
            let f = Arc::clone(&fabric);
            fabric
                .workers_set
                .spawn(async move { f.worker_loop(idx).await });
        }
        fabric.work.notify_waiters();
        Ok(fabric)
    }

    /// Submit a job: durable record first (whole-tx), then enqueue.
    pub async fn submit(&self, spec: JobSpec) -> crate::error::Result<String> {
        // KD-PV-14: minting a job record IS the coordinator-class act, so
        // this is where a second coordinator over one set is refused.
        if let Some(refusal) = maintenance_coordinator_refusal() {
            return Err(crate::error::SqueezefsError::InvalidOperation(refusal));
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let throttle = if spec.throttle_pct == 0 {
            self.default_throttle
        } else {
            spec.throttle_pct
        };
        let rec = JobRecord {
            schema: 1,
            job_id: job_id.clone(),
            job_type: spec.job_type.clone(),
            state: JobState::Queued,
            throttle_pct: throttle,
            created_by: format!("{}:{}", hostname_lossy(), std::process::id()),
            created_ts: unix_ts(),
            tasks_done: 0,
            tasks_total: spec.job_type.tasks_total(),
            error: None,
        };
        self.persist(&rec).await?;
        let ctl = Arc::new(JobCtl {
            job_type: rec.job_type.clone(),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            throttle: AtomicU32::new(throttle),
            done: AtomicU64::new(0),
            tasks_total: AtomicU64::new(rec.tasks_total),
            claimed: AtomicBool::new(false),
            serialize_noted: AtomicBool::new(false),
            state: parking_lot::Mutex::new(JobState::Queued),
            terminal: Notify::new(),
        });
        self.jobs.lock().insert(job_id.clone(), ctl);
        METRICS.job_submitted.fetch_add(1, Ordering::Relaxed);
        self.work.notify_waiters();
        Ok(job_id)
    }

    /// Point-in-time status: live map first, durable record otherwise
    /// (the offline-probe shape).
    pub async fn status(&self, job_id: &str) -> crate::error::Result<Option<JobStatus>> {
        if let Some(ctl) = self.jobs.lock().get(job_id).cloned() {
            return Ok(Some(JobStatus {
                job_id: job_id.to_string(),
                state: ctl.state(),
                tasks_done: ctl.done.load(Ordering::Relaxed),
                tasks_total: ctl.tasks_total.load(Ordering::Relaxed),
                throttle_pct: ctl.throttle.load(Ordering::Relaxed),
            }));
        }
        Ok(Self::read_record(&self.meta, job_id)
            .await?
            .map(|r| JobStatus {
                job_id: r.job_id,
                state: r.state,
                tasks_done: r.tasks_done,
                tasks_total: r.tasks_total,
                throttle_pct: r.throttle_pct,
            }))
    }

    /// Every live control block of a given job type (the undrain path
    /// cancels the victim's evacuation jobs through this).
    pub fn jobs_matching(&self, pred: impl Fn(&JobType) -> bool) -> Vec<(String, JobState)> {
        self.jobs
            .lock()
            .iter()
            .filter(|(_, ctl)| pred(&ctl.job_type))
            .map(|(id, ctl)| (id.clone(), ctl.state()))
            .collect()
    }

    /// Pause: workers stop pulling tasks after the in-flight one; the
    /// state change is durable.
    pub async fn pause(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.paused.store(true, Ordering::SeqCst);
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Paused);
        }
        self.checkpoint(job_id, &ctl).await
    }

    /// Resume a paused job (operator pauses AND `paused-capacity`
    /// self-pauses — freeing space and resuming is the §5.2 recovery).
    pub async fn resume(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.paused.store(false, Ordering::SeqCst);
        if ctl.state().is_paused() {
            ctl.set_state(JobState::Queued);
        }
        self.checkpoint(job_id, &ctl).await?;
        self.work.notify_waiters();
        Ok(())
    }

    /// Cancel: terminal, durable.
    pub async fn cancel(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.cancelled.store(true, Ordering::SeqCst);
        ctl.paused.store(false, Ordering::SeqCst); // unpark a paused job so it terminates
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Cancelled);
        }
        METRICS.job_cancelled.fetch_add(1, Ordering::Relaxed);
        self.checkpoint(job_id, &ctl).await?;
        self.work.notify_waiters();
        Ok(())
    }

    /// Live rethrottle (KD-3): workers re-read per task; durable.
    pub async fn throttle(&self, job_id: &str, pct: u32) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.throttle.store(pct, Ordering::SeqCst);
        self.checkpoint(job_id, &ctl).await
    }

    /// Await a terminal state (test/CLI convenience; polls the live
    /// notify with a deadline).
    pub async fn wait_terminal(
        &self,
        job_id: &str,
        timeout: Duration,
    ) -> crate::error::Result<JobState> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let Some(st) = self.status(job_id).await? else {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "unknown job {job_id}"
                )));
            };
            if st.state.is_terminal() {
                return Ok(st.state);
            }
            if std::time::Instant::now() >= deadline {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "job {job_id} not terminal within {timeout:?}"
                )));
            }
            // Bounded poll slice; the terminal notify shortens the tail.
            let notified = async {
                let ctl = self.jobs.lock().get(job_id).cloned();
                match ctl {
                    Some(c) => c.terminal.notified().await,
                    None => std::future::pending().await,
                }
            };
            let _ = squeezefs_ipc::sqz_time::timeout(Duration::from_millis(50), notified).await;
        }
    }

    /// "Crash" shutdown for soak/tests: abort workers mid-task, persist
    /// NOTHING — durable records stay non-terminal exactly as a kill-9
    /// leaves them. Abort = cancel + quiesce on the owned worker set:
    /// a cancelled worker's future is dropped at its next poll boundary
    /// — its current await point never resumes, so nothing later
    /// persists (the kill-9 analog preserved; the mid-poll window is
    /// the same non-instant one `JoinHandle::abort` had).
    pub async fn shutdown_abrupt(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.workers_set.cancel_all();
        // Wake parked workers so their next poll (where the cancel
        // gate fires) happens now, not at the park's timeout slice.
        self.work.notify_waiters();
        self.workers_set.quiesce().await;
    }

    /// Offline-probe-shaped list: decode every top-level `job:{id}`
    /// record on ino 1 (shard/progress sub-records are skipped by the
    /// single-segment filter).
    pub async fn list_records(
        meta: &Arc<RoutedMetaBackend>,
    ) -> crate::error::Result<Vec<JobRecord>> {
        let mut out = Vec::new();
        for name in meta.listxattr(ROOT_INO).await? {
            let Some(rest) = name.strip_prefix(JOB_XATTR_PREFIX) else {
                continue;
            };
            if rest.contains(':') {
                continue; // shard/progress sub-record
            }
            if name == crate::job_wire::JOB_ENROLL_XATTR {
                // The storage-trust secret record — a CONTROL record
                // sharing the prefix by design (the VL2 reserved-name
                // screen covers it), never a job: skipped by identity so
                // every enrolled fleet's census stops logging it as an
                // undecodable record.
                continue;
            }
            if let Some(bytes) = meta.getxattr(ROOT_INO, &name).await? {
                match serde_json::from_slice::<JobRecord>(&bytes) {
                    Ok(rec) if rec.schema == 1 => out.push(rec),
                    Ok(rec) => log::warn!(
                        "job fabric: skipping job {} with unknown schema {}",
                        rec.job_id,
                        rec.schema
                    ),
                    Err(e) => log::warn!("job fabric: undecodable record {name}: {e}"),
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------

    /// The fabric's meta handle (admin sink / offline probes reuse it).
    pub fn meta_handle(&self) -> &Arc<RoutedMetaBackend> {
        &self.meta
    }

    /// KD-MW-16: register the fleet read-shard dispatch seam (the wire
    /// host, at start). `Weak` — see the field note.
    pub fn set_fleet_dispatch(&self, dispatch: std::sync::Weak<dyn FleetDispatch>) {
        *self.fleet.lock() = Some(dispatch);
    }

    /// The registered fleet dispatch, if the wire host is still alive.
    pub fn fleet_dispatch(&self) -> Option<Arc<dyn FleetDispatch>> {
        self.fleet.lock().as_ref().and_then(|w| w.upgrade())
    }

    /// The configured local worker count — the §5.2 `transient` term's
    /// "ACTUAL configured workers".
    pub fn worker_count(&self) -> usize {
        self.workers
    }

    /// The mover context, when wired (the admin remove-data preflight
    /// runs through it).
    pub fn mover_ctx(&self) -> Option<&MoverCtx> {
        self.mover.as_ref()
    }

    /// R5 shed hook for the `job_copy_buffers` component (§5.1.5): under
    /// memory pressure, pause every running job — loud, durable via the
    /// workers' pause checkpoints, and deliberately NOT self-resuming
    /// (`job resume` is the operator's call once pressure clears). The
    /// gauge is worker copy-buffer bytes, so this fires only when real
    /// buffers exist.
    pub fn shed_to(&self, target: u64) {
        let gauge = METRICS.job_copy_buffer_bytes.load(Ordering::Relaxed);
        if gauge <= target {
            return;
        }
        for (id, ctl) in self.jobs.lock().iter() {
            if ctl.state() == JobState::Running {
                ctl.paused.store(true, Ordering::SeqCst);
                METRICS
                    .job_paused_mem_pressure
                    .fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "job fabric: paused job {id} under memory pressure \
                     (job_copy_buffers {gauge} > target {target})"
                );
            }
        }
    }

    fn require(&self, job_id: &str) -> crate::error::Result<Arc<JobCtl>> {
        self.jobs.lock().get(job_id).cloned().ok_or_else(|| {
            crate::error::SqueezefsError::InvalidOperation(format!("unknown job {job_id}"))
        })
    }

    async fn read_record(
        meta: &Arc<RoutedMetaBackend>,
        job_id: &str,
    ) -> crate::error::Result<Option<JobRecord>> {
        let name = format!("{JOB_XATTR_PREFIX}{job_id}");
        match meta.getxattr(ROOT_INO, &name).await? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            None => Ok(None),
        }
    }

    async fn persist(&self, rec: &JobRecord) -> crate::error::Result<()> {
        let name = format!("{JOB_XATTR_PREFIX}{}", rec.job_id);
        let bytes = serde_json::to_vec(rec).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!("job record encode: {e}"))
        })?;
        self.meta.setxattr(ROOT_INO, &name, &bytes).await?;
        METRICS
            .job_checkpoint_writes
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Write the current live state into the durable record.
    async fn checkpoint(&self, job_id: &str, ctl: &JobCtl) -> crate::error::Result<()> {
        self.checkpoint_as(job_id, ctl, ctl.state()).await
    }

    /// Checkpoint with an explicit state — the terminal path persists
    /// BEFORE the live state flips (durable-then-visible: a waiter woken
    /// by the terminal notify must find the terminal record on disk).
    async fn checkpoint_as(
        &self,
        job_id: &str,
        ctl: &JobCtl,
        state: JobState,
    ) -> crate::error::Result<()> {
        let Some(mut rec) = Self::read_record(&self.meta, job_id).await? else {
            return Ok(()); // record vanished (foreign cleanup) — advisory only
        };
        rec.state = state;
        rec.tasks_done = ctl.done.load(Ordering::Relaxed);
        rec.tasks_total = ctl.tasks_total.load(Ordering::Relaxed);
        rec.throttle_pct = ctl.throttle.load(Ordering::Relaxed);
        self.persist(&rec).await
    }

    /// The ONE paused-park ceremony (2026-08-09 gate strand): durable
    /// checkpoint, then the settle — the state flip re-checks the pause
    /// intent under the state lock, so a `resume()` that landed inside
    /// the checkpoint window wins instead of being overwritten by the
    /// stale `Paused` flip (the lost-resume strand: `claim_next` claims
    /// `Queued` only, so the overwrite left `paused = false` with a
    /// `Paused` state — unclaimable forever). A settled `Queued`
    /// re-asserts the durable record and wakes the pool.
    async fn park_paused(&self, job_id: &str, ctl: &JobCtl) {
        JOB_PARK_ENTRIES.fetch_add(1, Ordering::Relaxed);
        let st = if ctl.state() == JobState::PausedCapacity {
            JobState::PausedCapacity
        } else {
            JobState::Paused
        };
        let _ = self.checkpoint_as(job_id, ctl, st).await;
        // Park-window seam: stretch the checkpoint→flip gap the way
        // gate-load checkpoint contention does (red-first repro;
        // tests/job_fabric_tests.rs park-window tests).
        let park_delay = TEST_JOB_PARK_DELAY_MS.load(Ordering::Relaxed);
        if park_delay > 0 {
            squeezefs_ipc::sqz_time::sleep(Duration::from_millis(park_delay)).await;
        }
        if ctl.settle_park() == JobState::Queued {
            // The resume won inside the window: re-assert Queued
            // durably (the checkpoint above wrote Paused) and wake the
            // pool — the job keeps running instead of stranding.
            let _ = self.checkpoint(job_id, ctl).await;
            self.work.notify_waiters();
        }
    }

    /// KD-3 duty park, interruptible (2026-08-09 gate strand): the duty
    /// debt is served in bounded slices, re-deriving the remaining debt
    /// from the LIVE throttle and re-checking the control flags per
    /// slice — a live rethrottle / pause / cancel reaches a parked
    /// worker within one slice instead of at the end of a one-shot
    /// sleep (at 1 % a 2 s task priced a 198 s un-interruptible park).
    /// The TOTAL park for a fixed pct is unchanged, so the G-VL-7
    /// duty-adherence gate holds.
    async fn duty_park(ctl: &JobCtl, task_elapsed: Duration) {
        const SLICE: Duration = Duration::from_millis(50);
        let started = std::time::Instant::now();
        loop {
            if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                return; // the vehicle's park/cancel arm owns the exit
            }
            let pct = ctl.throttle.load(Ordering::Relaxed);
            let Some(total) = job_throttle_sleep(task_elapsed, pct) else {
                return; // rethrottled to unthrottled — debt forgiven live
            };
            let served = started.elapsed();
            if served >= total {
                return;
            }
            squeezefs_ipc::sqz_time::sleep((total - served).min(SLICE)).await;
        }
    }

    /// Claim the next runnable job (Queued, unclaimed). `for_wire`
    /// restricts the pick to wire-executable job types — the §5.1.6
    /// dispatcher must never claim a mover (its meta publish is
    /// coordinator-local; a remote "completion" without it would be a
    /// lie). The local pool claims everything.
    pub(crate) fn claim_next(&self, for_wire: bool) -> Option<(String, Arc<JobCtl>)> {
        let jobs = self.jobs.lock();
        for (id, ctl) in jobs.iter() {
            if (for_wire && !ctl.job_type.wire_executable())
                || ctl.state() != JobState::Queued
                || ctl.paused.load(Ordering::SeqCst)
            {
                continue;
            }
            // PR VL9 (pin a): mover-class serialization — one
            // mover-class job per volume at a time. A candidate whose
            // scope intersects a CLAIMED (in-execution) mover stays
            // Queued: `claimed` covers the whole worker-held window
            // (set at claim, cleared when the worker returns — a
            // paused/terminal mover releases its scope), so the
            // Queued→Running visibility gap cannot double-claim.
            if let Some(scope) = ctl.job_type.mover_scope() {
                let conflict = jobs.iter().any(|(other_id, other)| {
                    other_id != id
                        && other.claimed.load(Ordering::SeqCst)
                        && other
                            .job_type
                            .mover_scope()
                            .is_some_and(|o| o.conflicts(&scope))
                });
                if conflict {
                    if !ctl.serialize_noted.swap(true, Ordering::SeqCst) {
                        METRICS.job_serialized_waits.fetch_add(1, Ordering::Relaxed);
                        log::info!(
                            "job fabric: mover job {id} ({:?}) queued behind a running \
                             mover with an intersecting volume scope — one mover-class \
                             job per volume at a time (PR VL9 pin; KD-6 makes queueing \
                             safe, it runs when the scope frees)",
                            ctl.job_type
                        );
                    }
                    continue;
                }
            }
            if ctl
                .claimed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                ctl.serialize_noted.store(false, Ordering::SeqCst);
                return Some((id.clone(), Arc::clone(ctl)));
            }
        }
        None
    }

    /// Mark a remotely-executed job Running (durable-then-visible, the
    /// same order the local pool uses) — called by the wire dispatcher
    /// right after a shard is assigned.
    pub(crate) async fn remote_running(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        ctl.set_state(JobState::Running);
        let _ = self.checkpoint(job_id, ctl).await;
    }

    /// A verified remote submission completes the job: tasks are
    /// accounted, the durable record flips terminal BEFORE the live
    /// state (the run_job durable-then-visible law), and the terminal
    /// notify fires. A job that went terminal meanwhile (cancel) is
    /// left alone — the submission's effects were already refused or
    /// are contractually moot for Noop shards.
    pub(crate) async fn remote_complete(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        if ctl.state().is_terminal() {
            return;
        }
        let total = ctl.tasks_total.load(Ordering::Relaxed);
        let executed = total.saturating_sub(ctl.done.load(Ordering::Relaxed));
        ctl.done.store(total, Ordering::Relaxed);
        METRICS
            .job_tasks_done
            .fetch_add(executed, Ordering::Relaxed);
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// Return an expired remote shard's job to the queue (lease-expiry
    /// reassignment, §5.1.6): any population — local pool or another
    /// remote worker — may claim it again; the wire's bumped
    /// shard_fencing is what keeps the old holder's late submission out.
    pub(crate) async fn requeue_remote(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Queued);
            let _ = self.checkpoint(job_id, ctl).await;
        }
        ctl.claimed.store(false, Ordering::SeqCst);
        self.work.notify_waiters();
    }

    /// One pending-work wake for the wire dispatcher (the same notify
    /// the local pool parks on).
    pub(crate) fn work_notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.work.notified()
    }

    async fn worker_loop(self: Arc<Self>, idx: usize) {
        log::debug!("job fabric: worker {idx} up");
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let Some((job_id, ctl)) = self.claim_next(false) else {
                // Park until submitted/resumed work (bounded so a lost
                // notify cannot strand the pool).
                let _ = squeezefs_ipc::sqz_time::timeout(
                    Duration::from_millis(500),
                    self.work.notified(),
                )
                .await;
                continue;
            };
            ctl.set_state(JobState::Running);
            let _ = self.checkpoint(&job_id, &ctl).await;
            // RES-7 (pre-RC engineering spec §7): the claim is released
            // on EVERY exit, unwind included — the `PassSentinel` /
            // `PassGuard` shape the KV commit and publish conveyors
            // already use (spec §8). Without it a panicking job left
            // `claimed = true` forever and — by the VL9 pin-(a) mover
            // serialization — permanently blocked every mover-class job
            // whose volume scope intersects it.
            let claim = ClaimGuard { ctl: ctl.clone() };
            // ...and the unwind is CAUGHT, so the pool does not shrink by
            // one worker per panic. `worker_loop`'s task is spawned
            // fire-and-forget (the owned set observes resolution, not
            // output), so a lost worker was silent — the last one dying
            // meant no job could ever run again.
            let panicked = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                self.run_job(&job_id, &ctl),
            ))
            .await
            .is_err();
            drop(claim);
            if panicked {
                METRICS.job_worker_panics.fetch_add(1, Ordering::Relaxed);
                self.fail_job(
                    &job_id,
                    &ctl,
                    "job execution PANICKED (the worker survived and released its \
                     claim — see job_worker_panics); the panic itself is a bug",
                )
                .await;
            }
        }
    }

    /// Execute one job until terminal/paused.
    ///
    /// A panic escaping here is contained by `worker_loop`'s
    /// `catch_unwind` + [`ClaimGuard`] (RES-7).
    async fn run_job(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        match ctl.job_type.clone() {
            JobType::Noop { .. } => self.run_noop(job_id, ctl).await,
            JobType::EvacuateVolume { volume_id } => {
                self.run_mover(job_id, ctl, MoverObjective::Drain { volume_id })
                    .await
            }
            JobType::Rebalance => self.run_mover(job_id, ctl, MoverObjective::Rebalance).await,
            JobType::DefragData { volume_id } => {
                self.run_mover(job_id, ctl, MoverObjective::Defrag { volume_id })
                    .await
            }
            JobType::DefragPack { volume_id } => {
                self.run_defrag_pack(job_id, ctl, volume_id.as_deref())
                    .await
            }
            JobType::DefragMeta => self.run_defrag_meta(job_id, ctl).await,
            JobType::DefragFold => self.run_defrag_fold(job_id, ctl).await,
            JobType::KvmapSweep { ino } => self.run_kvmap_sweep(job_id, ctl, ino).await,
            JobType::MigrateMetaSlot {
                slot,
                target_volume,
            } => {
                self.run_meta_slot_migration(job_id, ctl, slot, target_volume)
                    .await
            }
            JobType::Fsck {
                scrub,
                scrub_only,
                repair,
                apply,
                quarantine_dir,
            } => {
                let ropts = repair.then(|| crate::fsck::RepairOptions {
                    apply,
                    quarantine_dir: quarantine_dir.map(std::path::PathBuf::from),
                    // Filled from this mount's own ownership map inside
                    // `run_fsck_job` (KD-PV-8) — the executor is where
                    // the live posture is read.
                    multi_owner: false,
                });
                self.run_fsck_job(job_id, ctl, scrub, scrub_only, ropts)
                    .await
            }
        }
    }

    /// PR VL6a: drive the §5.6 detection engine as a fabric job (LOCAL
    /// POOL ONLY — see `wire_executable`). The verified report persists
    /// as `job:{id}:report` beside the job record; findings > 0 leave
    /// the job **Completed** (detection succeeded; the findings are its
    /// output — the CLI owns exit-code semantics) but log loudly.
    async fn run_fsck_job(
        &self,
        job_id: &str,
        ctl: &Arc<JobCtl>,
        scrub: bool,
        scrub_only: bool,
        repair_opts: Option<crate::fsck::RepairOptions>,
    ) {
        let Some(ctx) = self.mover.as_ref() else {
            log::error!("job {job_id}: fsck needs a mover context (not wired on this fabric)");
            if let Ok(Some(mut rec)) = Self::read_record(&self.meta, job_id).await {
                rec.state = JobState::Failed;
                rec.error = Some("fsck needs a mover context".to_string());
                let _ = self.persist(&rec).await;
            }
            METRICS.job_failed.fetch_add(1, Ordering::Relaxed);
            ctl.set_state(JobState::Failed);
            return;
        };
        let mut opts = crate::fsck::FsckOptions::online();
        opts.scrub = scrub;
        opts.scrub_only = scrub_only;
        opts.throttle_pct = ctl.throttle.load(Ordering::Relaxed);
        // KD-PV-7/KD-PV-8: this mount's inode-plane posture, derived from
        // its OWN ownership map (unarmed ⇒ `None`/`false`, i.e. the
        // shipped whole-set pass, byte-identical).
        if let Some(map) = crate::meta_ship::owners::owner_map().filter(|m| m.multi_owner()) {
            opts.multi_owner = true;
            opts.owned_volumes = Some(
                (0..map.volume_count())
                    .filter(|v| map.owner_of_volume(*v).is_none())
                    .collect(),
            );
        }
        let repair_opts = repair_opts.map(|r| crate::fsck::RepairOptions {
            multi_owner: opts.multi_owner,
            ..r
        });
        let fsck_ctx = crate::fsck::FsckCtx {
            meta: self.meta.clone(),
            router: ctx.router.clone(),
            staging_dirs: ctx.router.cache.nvme.staging_dirs().to_vec(),
            expected_generation: Some(crate::fsck::volume_generation(&self.meta)),
        };
        // Cancellation bridge: mirror the ctl flags into the engine's
        // cooperative flag (pause is treated as cancel — a detection run
        // re-submits cheaply; there is no partial-resume state).
        let cancel = opts.cancel.clone();
        // Stop latch (the D0 heartbeat precedent — sqz-meta tasks are
        // never aborted mid-poll): the watcher exits within one 100 ms
        // slice of the run finishing; a late `cancel.store` against a
        // finished run's flag is harmless (the flag is per-run).
        let watcher_stop = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            let ctl = Arc::clone(ctl);
            let stop = Arc::clone(&watcher_stop);
            crate::meta_exec::spawn_meta("fsck_cancel_watcher", async move {
                loop {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                        cancel.store(true, Ordering::SeqCst);
                        return;
                    }
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
        // KD-MW-16 (rung 10c): the detect pass fans out across enrolled
        // fleet read workers when the wire host has any; with zero
        // capacity (every single-writer mount) `run_fleet` IS `run` —
        // the identity is pinned. `SQUEEZEFS_FLEET_JOBS=0` disarms the
        // fan-out at the coordinator.
        let fleet = if crate::env_knobs::fleet_jobs_enabled() {
            self.fleet_dispatch()
        } else {
            None
        };
        let outcome = crate::fsck::run_fleet(&fsck_ctx, &opts, fleet, job_id).await;
        // PR VL6b (§5.6a): repair consumes the run's VERIFIED findings on
        // the coordinator — dry-run plans only; apply executes each action
        // under the object's lease. A repair error fails the job loudly
        // (detection results are preserved in the log above).
        let outcome = match outcome {
            Ok(mut report) => match &repair_opts {
                Some(ropts) => match crate::fsck::repair(&fsck_ctx, &report, ropts).await {
                    Ok(rep) => {
                        log::info!(
                            "job {job_id}: fsck repair ({}) — {} planned, {} applied, \
                             {} refused",
                            if ropts.apply { "apply" } else { "dry-run" },
                            rep.counters.planned,
                            rep.counters.applied,
                            rep.counters.refused
                        );
                        report.repair = Some(rep);
                        Ok(report)
                    }
                    Err(e) => Err(e),
                },
                None => Ok(report),
            },
            Err(e) => Err(e),
        };
        watcher_stop.store(true, Ordering::Release);
        if ctl.cancelled.load(Ordering::SeqCst) {
            let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
            ctl.set_state(JobState::Cancelled);
            return;
        }
        if ctl.paused.load(Ordering::SeqCst) {
            self.park_paused(job_id, ctl).await;
            return;
        }
        match outcome {
            Ok(report) => {
                if report.has_findings() {
                    log::error!(
                        "job {job_id}: fsck VERIFIED {} finding(s) — {}",
                        report.findings.len(),
                        serde_json::to_string(&report.findings).unwrap_or_default()
                    );
                } else {
                    log::info!(
                        "job {job_id}: fsck clean ({} inodes, {} blocks, {} suspects cleared)",
                        report.counters.inodes_scanned,
                        report.counters.blocks_checked,
                        report.counters.suspects_cleared
                    );
                }
                // Persist the report beside the record (probe-readable).
                // Oversize reports truncate the findings list LOUDLY
                // (healthy reports are tiny — findings == 0).
                let name = format!("{JOB_XATTR_PREFIX}{job_id}:report");
                let cap = self.meta.xattr_value_cap(ROOT_INO).saturating_sub(1024);
                let mut to_store = report.clone();
                // KD-MW-16: the partial census is MERGE-INPUT data (the
                // shard union's refs + mapping identities) — meaningless
                // in the persisted artifact and, with the rung-10c
                // mapping identities, large enough to blow the xattr cap
                // on any real volume (found live: a 512-block corpus's
                // report was 66 KB against the 64 KiB cap, so NOTHING
                // persisted and the CLI read "no report"). Findings and
                // counters are the record; the partial never persists.
                to_store.partial = None;
                let mut truncated = 0usize;
                let mut bytes = serde_json::to_vec(&to_store).unwrap_or_default();
                while bytes.len() > cap && !to_store.findings.is_empty() {
                    to_store.findings.pop();
                    truncated += 1;
                    // COUNTED into the record itself (the admin view's
                    // `findings_elided` law): the CLI names what the
                    // stored artifact could not carry, and
                    // `has_findings` keeps the exit-1 verdict for the
                    // elided part.
                    to_store.findings_elided = truncated as u64;
                    bytes = serde_json::to_vec(&to_store).unwrap_or_default();
                }
                if truncated > 0 {
                    log::error!(
                        "job {job_id}: fsck report truncated {truncated} finding(s) to fit \
                         the xattr cap — the full list is in the daemon log above"
                    );
                }
                if let Err(e) = self.meta.setxattr(ROOT_INO, &name, &bytes).await {
                    log::warn!("job {job_id}: fsck report persist failed: {e}");
                }
                ctl.done.store(1, Ordering::Relaxed);
                ctl.tasks_total.store(1, Ordering::Relaxed);
                let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Completed);
            }
            Err(e) => {
                log::error!("job {job_id}: fsck failed: {e}");
                if let Ok(Some(mut rec)) = Self::read_record(&self.meta, job_id).await {
                    rec.state = JobState::Failed;
                    rec.error = Some(format!("fsck failed: {e}"));
                    let _ = self.persist(&rec).await;
                }
                METRICS.job_failed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Failed);
            }
        }
    }

    /// PR VL5b: drive the online slot-migration engine as a fabric job
    /// (LOCAL POOL ONLY — the engine holds the coordinator's routed
    /// backend; the wire's shard shape does not carry it, exactly like
    /// the VL4 movers). Terminal states ride the durable record like
    /// every job; a kill-9 resume re-runs the idempotent engine.
    async fn run_meta_slot_migration(
        &self,
        job_id: &str,
        ctl: &JobCtl,
        slot: u16,
        target_volume: usize,
    ) {
        if ctl.cancelled.load(Ordering::SeqCst) {
            let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
            ctl.set_state(JobState::Cancelled);
            return;
        }
        let out = crate::meta_backend::slot_migration::migrate_slot(
            &self.meta,
            slot,
            target_volume,
            &crate::meta_backend::slot_migration::MigrationOptions::default(),
            &crate::meta_backend::slot_migration::MigrationTestHooks::default(),
        )
        .await;
        match out {
            Ok(report) => {
                ctl.done.store(1, Ordering::Relaxed);
                ctl.tasks_total.store(1, Ordering::Relaxed);
                let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Completed);
                log::info!(
                    "migrate-meta-slot {job_id}: slot {slot} → volume {target_volume}                      ({} records, {} delta keys, {} overflows, cutover {} ms)",
                    report.records_copied,
                    report.delta_keys,
                    report.overflows,
                    report.cutover_ms
                );
            }
            Err(e) => {
                log::error!("migrate-meta-slot {job_id} failed: {e}");
                if let Ok(Some(mut rec)) = Self::read_record(&self.meta, job_id).await {
                    rec.state = JobState::Failed;
                    rec.error = Some(format!("slot migration failed: {e}"));
                    let _ = self.persist(&rec).await;
                }
                METRICS.job_failed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Failed);
            }
        }
    }

    /// Fail a job loudly: durable `Failed` record with the error, then
    /// the live flip (the shared shape of every inline failure arm).
    async fn fail_job(&self, job_id: &str, ctl: &JobCtl, err: &str) {
        log::error!("job {job_id}: {err}");
        if let Ok(Some(mut rec)) = Self::read_record(&self.meta, job_id).await {
            rec.state = JobState::Failed;
            rec.error = Some(err.to_string());
            let _ = self.persist(&rec).await;
        }
        METRICS.job_failed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Failed);
    }

    /// PR VL7 (§5.7 D4): per meta volume, page the trees resident, take
    /// the dead-bset census, and nudge every dead-carrying leaf through
    /// the EXISTING SMO compactor (`defrag_compact_nodes` — serialized
    /// with the checkpoint task on the SMO mutex; never a new
    /// compactor). Duty-cycle throttled per nudge batch; crash-resume is
    /// plan regeneration (a re-run re-censuses — compaction is
    /// idempotent, an already-folded leaf is no longer a candidate).
    async fn run_defrag_meta(&self, job_id: &str, ctl: &JobCtl) {
        let mut last_checkpoint = std::time::Instant::now();
        for kv in self.meta.volumes.iter() {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return;
            }
            if let Err(e) = crate::defrag::page_in_leaves(kv).await {
                self.fail_job(job_id, ctl, &format!("defrag-meta leaf paging failed: {e}"))
                    .await;
                return;
            }
            let census = kv.dead_bset_census();
            log::info!(
                "job {job_id}: defrag-meta on {:?} — {} leaves, {} indexed / {} live \
                 records, {} candidate(s)",
                kv.device_path(),
                census.leaves,
                census.records_indexed,
                census.records_live,
                census.candidates.len()
            );
            ctl.tasks_total
                .fetch_add(census.candidates.len() as u64, Ordering::Relaxed);
            for chunk in census.candidates.chunks(8) {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                    break;
                }
                let start = std::time::Instant::now();
                match kv.defrag_compact_nodes(chunk).await {
                    Ok(kicked) => {
                        METRICS
                            .defrag_meta_compactions_kicked
                            .fetch_add(kicked, Ordering::Relaxed);
                        ctl.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        METRICS
                            .job_tasks_done
                            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        self.fail_job(job_id, ctl, &format!("defrag-meta nudge failed: {e}"))
                            .await;
                        return;
                    }
                }
                if last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS) {
                    let _ = self.checkpoint(job_id, ctl).await;
                    last_checkpoint = std::time::Instant::now();
                }
                Self::duty_park(ctl, start.elapsed()).await;
            }
            // The §4.6a merge arm (design-cow-kv-metadata §4.6a (e), the
            // third trigger, finalized): the volume's merge sweep driven to
            // ONE whole lap in bounded chunks — each chunk is one sweep call
            // under the checkpoint's own budget (`merge_sweep_budget_ms`,
            // finding 49's drain law) and the duty cycle parks between
            // chunks, so the census IS the bounded, cursor-resumed walk the
            // heap-full sweep runs (no separate O(leaves) pass) and a 1 %
            // throttle stretches the merge pass like every other nudge. A
            // compaction-floor refusal ends the lap's merging (the
            // checkpoint cycles return the extents; a re-run continues);
            // the lap's count phase still publishes the exact candidates.
            let mut chunks = 0u64;
            loop {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                    break;
                }
                let start = std::time::Instant::now();
                let deadline = start + Duration::from_millis(kv.merge_sweep_budget_ms());
                match kv.defrag_merge_sweep(Some(deadline)).await {
                    Ok(report) => {
                        let merged = report.merges + report.root_collapses;
                        METRICS
                            .defrag_meta_merges
                            .fetch_add(merged, Ordering::Relaxed);
                        // Advisory progress: one task per chunk, plus the
                        // lap's exact candidate count as the total once
                        // known.
                        chunks += 1;
                        ctl.tasks_total.fetch_add(1, Ordering::Relaxed);
                        ctl.done.fetch_add(1, Ordering::Relaxed);
                        METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                        if report.lap_complete {
                            log::info!(
                                "job {job_id}: defrag-meta merge lap on {:?} — {} merges ({} \
                                 interior), {} collapses, {} underfull leaves remain, {chunks} \
                                 chunk(s)",
                                kv.device_path(),
                                report.merges,
                                report.interior_merges,
                                report.root_collapses,
                                report.candidates
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        self.fail_job(job_id, ctl, &format!("defrag-meta merge failed: {e}"))
                            .await;
                        return;
                    }
                }
                if last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS) {
                    let _ = self.checkpoint(job_id, ctl).await;
                    last_checkpoint = std::time::Instant::now();
                }
                Self::duty_park(ctl, start.elapsed()).await;
            }
        }
        if ctl.cancelled.load(Ordering::SeqCst) {
            let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
            ctl.set_state(JobState::Cancelled);
            return;
        }
        if ctl.paused.load(Ordering::SeqCst) {
            self.park_paused(job_id, ctl).await;
            return;
        }
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// PR VL7 (§5.7 D3): kick every parked/spilled extent block through
    /// the mount's fold hook — the EXISTING W2 fold machinery run to
    /// completion. Kick failures are logged and left parked (never-lossy:
    /// the extents stay custody; the fsync/pressure drains own the error
    /// surface — exactly the background fold worker's law).
    async fn run_defrag_fold(&self, job_id: &str, ctl: &JobCtl) {
        let Some(hook) = self.mover.as_ref().and_then(|m| m.fold.as_ref()) else {
            self.fail_job(
                job_id,
                ctl,
                "defrag-fold needs the mount's fold surface: fold custody is \
                 mount-owned (offline coordinators cannot kick folds — run \
                 `squeezefs defrag <mountpoint> --fold` against the live mount)",
            )
            .await;
            return;
        };
        let targets = (hook.targets)();
        ctl.tasks_total
            .store(targets.len() as u64, Ordering::Relaxed);
        let mut last_checkpoint = std::time::Instant::now();
        for (ino, b) in targets {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return;
            }
            let start = std::time::Instant::now();
            match (hook.kick)(ino, b).await {
                Ok(true) => {
                    METRICS.defrag_folds_kicked.fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {} // vanished / full-repr: the flush machinery owns it
                Err(e) => log::warn!(
                    "job {job_id}: defrag-fold kick failed ({e}) — the extents stay \
                     parked custody (never-lossy); the fsync/pressure drains own the \
                     error surface"
                ),
            }
            ctl.done.fetch_add(1, Ordering::Relaxed);
            METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
            if last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS) {
                let _ = self.checkpoint(job_id, ctl).await;
                last_checkpoint = std::time::Instant::now();
            }
            Self::duty_park(ctl, start.elapsed()).await;
        }
        if let Some(ctx) = self.mover.as_ref() {
            let router = ctx.router.clone();
            squeezefs_ipc::sqz_blocking::run_blocking(move || {
                crate::defrag::refresh_d1_d3_gauges(&router)
            })
            .await;
        }
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// PR 6b (design §3/A2): the kvmap background sweep — one
    /// [`crate::routing::DataRouter::kvmap_sweep_chunk`] per task
    /// (throttled, KD-3), crash-resume by the durable cursor (KD-6: a
    /// re-run re-reads it under the ino's held 4a and continues
    /// record-true — no double deletes, no double frees). The TERMINAL
    /// chunk cleared the cursor; a corpse owner (`nlink == 0`,
    /// unreachable — the `delete_file` handoff's shape) is then
    /// destroyed here: record + xattrs in one tx (the C9
    /// quarantine-then-destroy shape's destroy half — the corpse is a
    /// planned teardown, not a repair, so nothing quarantines), and the
    /// reclaim callers' destroy-withholding mark clears with it.
    async fn run_kvmap_sweep(&self, job_id: &str, ctl: &JobCtl, ino: u64) {
        let Some(ctx) = self.mover.as_ref() else {
            self.fail_job(
                job_id,
                ctl,
                "kvmap sweep needs a mover context (the router resolves records to \
                 block keys and owns the reclaim enqueue) — not wired on this fabric",
            )
            .await;
            return;
        };
        let chunk = crate::routing::map_migrate_chunk();
        let mut last_checkpoint = std::time::Instant::now();
        loop {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return;
            }
            let start = std::time::Instant::now();
            let terminal = match ctx.router.kvmap_sweep_chunk(ino, chunk).await {
                // No cursor: nothing owed — a publish's extend barrier
                // absorbed the span, a duplicate job won, or the plan
                // completed under a prior era.
                Ok(crate::routing::KvmapSweepProgress::NoCursor) => true,
                Ok(crate::routing::KvmapSweepProgress::Progress { .. }) => {
                    ctl.done.fetch_add(1, Ordering::Relaxed);
                    METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                    false
                }
                Ok(crate::routing::KvmapSweepProgress::Terminal { .. }) => {
                    ctl.done.fetch_add(1, Ordering::Relaxed);
                    METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(e) => {
                    // Loud + resumable: the durable cursor survives, so a
                    // resume (or the next mount's adoption scan) re-plans
                    // from exactly where this chunk refused.
                    self.fail_job(job_id, ctl, &format!("kvmap sweep chunk failed: {e}"))
                        .await;
                    return;
                }
            };
            if terminal {
                break;
            }
            if last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS) {
                let _ = self.checkpoint(job_id, ctl).await;
                last_checkpoint = std::time::Instant::now();
            }
            Self::duty_park(ctl, start.elapsed()).await;
        }
        // The corpse's terminal destroy (the delete_file handoff kept the
        // record + head alive as the durable plan carrier).
        let corpse = matches!(self.meta.getattr(ino).await, Ok(inode) if inode.nlink == 0);
        if corpse {
            if let Err(e) = crate::meta_ship::publish::destroy_inodes(&self.meta, &[ino]).await {
                self.fail_job(
                    job_id,
                    ctl,
                    &format!(
                        "kvmap sweep: records drained but the corpse destroy failed: {e} \
                         — resumable (the mount corpse sweep also owns a cursor-less \
                         empty corpse)"
                    ),
                )
                .await;
                return;
            }
            ctx.router.kvmap_sweep_corpse_clear(ino);
        }
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// The Noop vehicle: duty-cycle throttle after every task (KD-3,
    /// live-re-read); checkpoint on the §5.1.2 cadence.
    async fn run_noop(&self, job_id: &str, ctl: &JobCtl) {
        let mut since_checkpoint = 0u64;
        let mut last_checkpoint = std::time::Instant::now();
        loop {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return;
            }
            let done = ctl.done.load(Ordering::Relaxed);
            if done >= ctl.tasks_total.load(Ordering::Relaxed) {
                // Durable-then-visible: the record flips terminal on
                // disk before any waiter can observe it live.
                let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Completed);
                return;
            }

            let start = std::time::Instant::now();
            match &ctl.job_type {
                JobType::Noop { task_ms, .. } => {
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(*task_ms)).await;
                }
                _ => unreachable!("run_noop only executes Noop jobs"),
            }
            ctl.done.fetch_add(1, Ordering::Relaxed);
            METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
            since_checkpoint += 1;
            // RES-7 test seam (see TEST_JOB_PANIC_AFTER).
            let panic_after = TEST_JOB_PANIC_AFTER.load(Ordering::Relaxed);
            if panic_after > 0 && ctl.done.load(Ordering::Relaxed) >= panic_after {
                panic!("RES-7 test seam: injected job-worker panic");
            }

            if since_checkpoint >= CHECKPOINT_TASKS
                || last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS)
            {
                let _ = self.checkpoint(job_id, ctl).await;
                since_checkpoint = 0;
                last_checkpoint = std::time::Instant::now();
            }

            // KD-3: duty-cycle throttle, live re-read per task.
            Self::duty_park(ctl, start.elapsed()).await;
        }
    }

    // -----------------------------------------------------------------
    // The VL4 movers (§5.4 / §5.7-rebalance)
    // -----------------------------------------------------------------

    /// Run one mover job to a terminal/paused state, translating errors
    /// into a loud durable `Failed`.
    async fn run_mover(&self, job_id: &str, ctl: &JobCtl, objective: MoverObjective) {
        match self.mover_body(job_id, ctl, &objective).await {
            Ok(()) => {}
            Err(e) => {
                log::error!("job {job_id}: mover failed: {e}");
                if !ctl.state().is_terminal() && !ctl.state().is_paused() {
                    if let Ok(Some(mut rec)) = Self::read_record(&self.meta, job_id).await {
                        rec.state = JobState::Failed;
                        rec.error = Some(e.clone());
                        rec.tasks_done = ctl.done.load(Ordering::Relaxed);
                        let _ = self.persist(&rec).await;
                    }
                    METRICS.job_failed.fetch_add(1, Ordering::Relaxed);
                    ctl.set_state(JobState::Failed);
                }
            }
        }
        // The drain gauges describe the ACTIVE drain only.
        METRICS.evacuate_needed_bytes.store(0, Ordering::Relaxed);
        METRICS.evacuate_avail_bytes.store(0, Ordering::Relaxed);
        METRICS.evacuate_transient_bytes.store(0, Ordering::Relaxed);
    }

    async fn mover_body(
        &self,
        job_id: &str,
        ctl: &JobCtl,
        objective: &MoverObjective,
    ) -> Result<(), String> {
        let ctx = self.mover.as_ref().ok_or_else(|| {
            "mover jobs need a mover context (not wired on this fabric)".to_string()
        })?;
        let block_size = ctx.router.block_size.load(Ordering::Relaxed);
        let mut last_checkpoint = std::time::Instant::now();
        let mut since_checkpoint = 0u64;
        let started = std::time::Instant::now();
        let moved_at_start = METRICS.evacuate_bytes_moved.load(Ordering::Relaxed);
        let mut defrag_passes = 0u32;

        loop {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return Ok(());
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return Ok(());
            }

            // Async block-reclaim settle (per pass, BEFORE planning): the
            // planners and the contiguity-aware destination picks read
            // free-list state, and displaced-source frees ride the
            // background queue — planning against queued (not yet
            // finish_freed) state defers moves spuriously and converges
            // a defrag on a fragmented shape. Mover cadence, never the
            // write path.
            ctx.router.backend_router.reclaim_drain().await;

            // Plan regeneration per pass (KD-6): census from CURRENT
            // durable state — idempotent by construction.
            let (census, victim) = match objective {
                MoverObjective::Drain { volume_id } => {
                    // An externally-undrained volume ends the job as
                    // cancelled (the undrain verb also cancels
                    // explicitly; this covers offline/adopted races).
                    match ctx.router.backend_router.volume_state(volume_id) {
                        Some(state) if state == crate::VOL_STATE_DRAINING => {}
                        other => {
                            log::warn!(
                                "job {job_id}: volume '{volume_id}' is {:?}, not draining — \
                                 ending the evacuation as cancelled",
                                other
                            );
                            ctl.cancelled.store(true, Ordering::SeqCst);
                            continue;
                        }
                    }
                    (
                        census_for(&self.meta, &ctx.router, std::slice::from_ref(volume_id))
                            .await
                            .map_err(|e| format!("census failed: {e}"))?,
                        Some(volume_id.clone()),
                    )
                }
                MoverObjective::Rebalance => {
                    let plan = plan_rebalance(&ctx.router);
                    if plan.is_empty() {
                        // Balanced (or nothing eligible): the bounded
                        // pass has nothing to do.
                        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                        ctl.set_state(JobState::Completed);
                        return Ok(());
                    }
                    let sources: Vec<String> = plan.iter().map(|p| p.source.clone()).collect();
                    let mut census = census_for(&self.meta, &ctx.router, &sources)
                        .await
                        .map_err(|e| format!("census failed: {e}"))?;
                    bound_rebalance_census(&mut census, &plan, block_size);
                    (census, None)
                }
                MoverObjective::Defrag { volume_id } => (
                    plan_defrag(&self.meta, ctx, volume_id.as_deref()).await?,
                    None,
                ),
            };

            // Publish the discovered totals (advisory progress).
            let remaining = census.tasks.len() as u64 + census.blob_relocations.len() as u64;
            ctl.tasks_total.store(
                ctl.done.load(Ordering::Relaxed) + remaining,
                Ordering::Relaxed,
            );

            // §5.2 capacity re-verification (drains): needed = the
            // remaining census; avail from the survivors; headroom from
            // the LIVE write-rate estimate and the measured job rate.
            if let Some(victim_id) = victim.as_deref() {
                ctx.write_rate.observe();
                let needed_bytes = census.distinct_blocks.saturating_mul(block_size);
                let rate = ctx.write_rate.bytes_per_sec(block_size);
                // drain_eta = needed / measured_job_rate from the first
                // checkpoint onward; the initial estimate uses the
                // G-VL-3(b) floor rate (§5.2).
                let moved = METRICS
                    .evacuate_bytes_moved
                    .load(Ordering::Relaxed)
                    .saturating_sub(moved_at_start);
                let elapsed = started.elapsed().as_secs().max(1);
                let job_rate = if moved > 0 {
                    (moved / elapsed).max(1)
                } else {
                    DRAIN_FLOOR_RATE_BYTES_PER_SEC
                };
                let eta = needed_bytes / job_rate + 1;
                let pf = DrainPreflight {
                    needed_bytes,
                    avail_bytes: avail_elsewhere(&ctx.router, &[victim_id.to_string()]),
                    transient_bytes: drain_transient_bytes(self.workers, block_size),
                    headroom_bytes: drain_headroom_bytes(rate, eta),
                };
                METRICS
                    .evacuate_needed_bytes
                    .store(pf.needed_bytes, Ordering::Relaxed);
                METRICS
                    .evacuate_avail_bytes
                    .store(pf.avail_bytes, Ordering::Relaxed);
                METRICS
                    .evacuate_transient_bytes
                    .store(pf.transient_bytes, Ordering::Relaxed);
                if !pf.admits() {
                    METRICS.job_paused_capacity.fetch_add(1, Ordering::Relaxed);
                    log::error!(
                        "job {job_id}: drain of '{victim_id}' self-paused (paused-capacity) — \
                         foreground writes consumed the slack: {}",
                        pf.refusal()
                    );
                    ctl.paused.store(true, Ordering::SeqCst);
                    ctl.set_state(JobState::PausedCapacity);
                    let _ = self
                        .checkpoint_as(job_id, ctl, JobState::PausedCapacity)
                        .await;
                    return Ok(());
                }
            }

            // Convergence check: census empty (and, for drains, the
            // victim's allocator drained — an in-flight foreground write
            // that pre-dates the draining flip still holds an allocated
            // offset until its publish; retire must wait for it).
            let mut deferred_this_pass = 0u64;
            let mut moved_this_pass = 0u64;
            if census.tasks.is_empty() && census.blob_relocations.is_empty() {
                // Mover-class convergence implies space RETURNED, not
                // merely queued: displaced-source frees ride the
                // background reclaim queue (src/block_reclaim.rs), so
                // drain it before adjudicating — the D1 gauges an
                // operator reads at Completed, and the Drain arm's
                // `victim_used == 0` retire gate, must observe the
                // vacated blocks actually finish_freed.
                ctx.router.backend_router.reclaim_drain().await;
                match objective {
                    MoverObjective::Rebalance | MoverObjective::Defrag { .. } => {
                        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                        ctl.set_state(JobState::Completed);
                        return Ok(());
                    }
                    MoverObjective::Drain { volume_id } => {
                        let victim_used = ctx
                            .router
                            .backend_router
                            .backends
                            .get(volume_id)
                            .map(|be| be.block_allocator.get_used_blocks())
                            .unwrap_or(0);
                        if victim_used == 0 {
                            self.retire_volume(volume_id, ctx).await?;
                            let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                            METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                            ctl.set_state(JobState::Completed);
                            log::info!("job {job_id}: volume '{volume_id}' evacuated and retired");
                            return Ok(());
                        }
                        log::info!(
                            "job {job_id}: census empty but '{volume_id}' still holds \
                             {victim_used} allocated block(s) (in-flight writes or leaks \
                             pending remount reclaim) — re-planning"
                        );
                        deferred_this_pass += victim_used;
                    }
                }
            }

            // Execute the pass. VL10 (G-VL-3 b): independent block moves
            // OVERLAP when unthrottled — each move pays 3× its bytes in
            // serialized device round-trips (read → write → verify →
            // publish), and the serial per-block loop measured exactly
            // the ½-of-raw-copy floor. Drain/rebalance moves are
            // independent (per-block flush locks; allocator picks are
            // atomic; per-ino publish commits serialize on the 4a lease
            // they already take), so a bounded window of them runs
            // concurrently via join_all. Defrag stays serial: its D1/D2
            // picks thread an ascending-floor invariant through the
            // task sequence (`DestPick::BackendAscending`'s floor
            // update is order-dependent). Throttled runs stay serial —
            // KD-3's duty cycle is per WORKER, and a concurrent window
            // would consume a multiple of the granted duty budget.
            let mut task_i = 0usize;
            while task_i < census.tasks.len() {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                    break;
                }
                // Live re-read per window: a mid-job rethrottle collapses
                // the window back to serial on the next iteration.
                let pct = ctl.throttle.load(Ordering::Relaxed);
                let unthrottled = pct == 0 || pct >= 100;
                let width = if unthrottled
                    && matches!(
                        objective,
                        MoverObjective::Drain { .. } | MoverObjective::Rebalance
                    ) {
                    MOVER_PIPELINE_WIDTH
                } else {
                    1
                };
                let window = &census.tasks[task_i..(task_i + width).min(census.tasks.len())];
                task_i += window.len();
                let start = std::time::Instant::now();
                let outcomes = futures::future::join_all(
                    window
                        .iter()
                        .map(|task| self.move_one(ctx, task, victim.as_deref())),
                )
                .await;
                for outcome in outcomes {
                    match outcome {
                        MoveOutcome::Moved => {
                            ctl.done.fetch_add(1, Ordering::Relaxed);
                            METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                            moved_this_pass += 1;
                            if matches!(objective, MoverObjective::Defrag { .. }) {
                                // §10: the defrag-engagement pair (a defrag
                                // move also counts the shared evacuate_*
                                // family at the move_one site).
                                METRICS.defrag_blocks_moved.fetch_add(1, Ordering::Relaxed);
                                METRICS
                                    .defrag_bytes_moved
                                    .fetch_add(block_size, Ordering::Relaxed);
                            }
                        }
                        MoveOutcome::Deferred => deferred_this_pass += 1,
                        MoveOutcome::Superseded => {
                            // Counted at the publish site; re-plan revisits.
                        }
                    }
                }
                since_checkpoint += window.len() as u64;
                if since_checkpoint >= CHECKPOINT_TASKS
                    || last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS)
                {
                    let _ = self.checkpoint(job_id, ctl).await;
                    ctx.write_rate.observe();
                    since_checkpoint = 0;
                    last_checkpoint = std::time::Instant::now();
                }
                // KD-3: duty-cycle throttle, live re-read per task
                // (width is 1 whenever the throttle is active).
                Self::duty_park(ctl, start.elapsed()).await;
            }

            // Indirect blob relocations: one empty merge under the ino's
            // CURRENT token — the save path reallocates a blob living on
            // a non-active volume and frees the old block.
            for &ino in &census.blob_relocations {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                    break;
                }
                let token = ctx.router.dlm.get_fencing_token_ino(ino);
                match ctx
                    .router
                    .merge_block_mappings(
                        ino,
                        crate::routing::BlockMapOp::Merge(&[]),
                        0,
                        crate::routing::LayoutFlip::KeepLayout,
                        token,
                    )
                    .await
                {
                    Ok(_) => {
                        ctl.done.fetch_add(1, Ordering::Relaxed);
                        METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        log::warn!("mover: indirect-blob relocation for ino {ino} deferred: {e}");
                        deferred_this_pass += 1;
                    }
                }
            }

            let _ = self.checkpoint(job_id, ctl).await;
            METRICS.evacuate_replans.fetch_add(1, Ordering::Relaxed);

            // The rebalance objective is a BOUNDED PASS (§5.3 step 6 /
            // KD-12): one plan, one execution, then done — deferred or
            // superseded tasks are simply not retried (the next add /
            // operator invocation runs a fresh pass; convergence-by-
            // re-plan is the DRAIN's law, not rebalance's).
            if matches!(objective, MoverObjective::Rebalance)
                && !ctl.cancelled.load(Ordering::SeqCst)
                && !ctl.paused.load(Ordering::SeqCst)
            {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Completed);
                return Ok(());
            }

            // The defrag objective converges by re-plan while passes make
            // progress and completes best-effort at the first no-progress
            // pass (§5.7: foreground churn keeps minting new work — the
            // next invocation re-plans from current state, KD-6; an empty
            // census completes through the convergence check above). The
            // pass cap is the termination BELT under sustained churn: a
            // defrag never wedges the fabric chasing a moving target.
            // The freshly-moved state re-gauges before the verdict.
            if let MoverObjective::Defrag { .. } = objective {
                defrag_passes += 1;
                let router = ctx.router.clone();
                squeezefs_ipc::sqz_blocking::run_blocking(move || {
                    crate::defrag::refresh_d1_d3_gauges(&router)
                })
                .await;
                if !ctl.cancelled.load(Ordering::SeqCst) && !ctl.paused.load(Ordering::SeqCst) {
                    if moved_this_pass == 0 {
                        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                        ctl.set_state(JobState::Completed);
                        return Ok(());
                    }
                    if defrag_passes >= DEFRAG_MAX_PASSES {
                        log::info!(
                            "job {job_id}: defrag completed at the {DEFRAG_MAX_PASSES}-pass \
                             cap with work remaining (foreground churn keeps minting it) — \
                             re-invoke to continue (KD-6 re-plan)"
                        );
                        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
                        ctl.set_state(JobState::Completed);
                        return Ok(());
                    }
                }
            }

            if deferred_this_pass > 0 || census.tasks.is_empty() {
                // No forward progress possible right now (quiescence
                // waits / in-flight victim allocations): bounded backoff
                // before the next idempotent re-plan.
                squeezefs_ipc::sqz_time::sleep(REPLAN_BACKOFF).await;
            }
        }
    }

    /// §5.4 step 5 — the retire commit: durable record → `Retired` with
    /// the path cleared (one tx through the live conveyor), then the
    /// runtime deregistration (reads of a straggler key now fail loud).
    async fn retire_volume(&self, volume_id: &str, ctx: &MoverCtx) -> Result<(), String> {
        let raw = self
            .meta
            .getxattr(
                ROOT_INO,
                crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            )
            .await
            .map_err(|e| format!("format config read failed: {e}"))?
            .ok_or_else(|| "format config not found".to_string())?;
        let mut cfg: crate::FormatConfig =
            serde_json::from_slice(&raw).map_err(|e| format!("format config undecodable: {e}"))?;
        let mut records = cfg.resolved_data_volumes();
        let rec = records
            .iter_mut()
            .find(|r| r.id == volume_id)
            .ok_or_else(|| format!("volume '{volume_id}' vanished from the record set"))?;
        rec.state = crate::VOL_STATE_RETIRED.to_string();
        rec.backing_dev = String::new(); // path cleared; id kept forever (KD-5)
        cfg.data_lv = Some(
            records
                .iter()
                .filter(|r| r.state != crate::VOL_STATE_RETIRED)
                .map(|r| r.backing_dev.clone())
                .collect(),
        );
        cfg.data_volumes = Some(records.clone());
        let bytes = serde_json::to_vec(&cfg).map_err(|e| format!("config serialize: {e}"))?;
        self.meta
            .setxattr(
                ROOT_INO,
                crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
                &bytes,
            )
            .await
            .map_err(|e| format!("retire commit failed: {e}"))?;

        // Runtime AFTER the durable commit: snapshot + deregistration.
        ctx.router.backend_router.set_volume_records(records);
        ctx.router
            .backend_router
            .retire_backend(volume_id)
            .map_err(|e| format!("retire deregistration failed: {e}"))?;
        Ok(())
    }

    /// Move ONE distinct source block (§5.1.5 discipline + §5.4 step 2
    /// shared-block protocol). Copy is lock-free data plane; publish is
    /// per-referencer `MergeExpected` under the ino's CURRENT token with
    /// the block's flush lock held for the final quiescence
    /// check-and-move; displaced sources free clone-aware.
    async fn move_one(
        &self,
        ctx: &MoverCtx,
        task: &MoveTask,
        drain_victim: Option<&str>,
    ) -> MoveOutcome {
        let router = &ctx.router;
        let br = &router.backend_router;
        let block_size = router.block_size.load(Ordering::Relaxed);

        // An OPEN pack block defers (design-small-file-packing §5.11): the
        // pack-open ledger names its base while the packer's pin is live,
        // and a tenant between reserve and commit holds an in-flight
        // registration on it. A copy now would republish the committed
        // tenants elsewhere, drop the source to the packer's pin, and the
        // packer's next tenant would land in a block this pass just tried
        // to vacate — correct, but the pass's work wasted. One relaxed
        // probe per candidate. When the source is the DRAIN's victim the
        // pack is sealed here (the pin released, the ledger left) so the
        // next re-plan moves it: left open, a draining volume's pack keeps
        // taking tenants and a quiet mount's never seals.
        if pack_ledger_contains(&task.base_key)
            || br
                .allocator_for_key(&task.base_key)
                .is_some_and(|(alloc, offset)| alloc.inflight_contains(offset))
        {
            METRICS
                .pack_mover_open_defers
                .fetch_add(1, Ordering::Relaxed);
            METRICS
                .evacuate_deferred_staged_blocks
                .fetch_add(1, Ordering::Relaxed);
            if drain_victim == Some(task.src_id.as_str())
                && router.seal_open_pack_block(&task.base_key).await
            {
                log::info!(
                    "mover: sealed the open pack block {} on draining volume '{}' — moved at \
                     the next re-plan",
                    task.base_key,
                    task.src_id
                );
            }
            return MoveOutcome::Deferred;
        }

        // Quiescent-first (§5.4 step 3): every referencer must be free
        // of live buffers / parked extents / spilled records.
        for r in &task.refs {
            if !(ctx.quiesce)(r.ino, r.block_idx as u64) {
                METRICS
                    .evacuate_deferred_staged_blocks
                    .fetch_add(1, Ordering::Relaxed);
                return MoveOutcome::Deferred;
            }
        }

        // Pin the source (the clone/patch fence, §5.1 of
        // design-random-small-writes): the pin makes the copy window
        // patch-free (a sole-owner patch needs refcount 1) and validates
        // no patch is mid-DMA. Refused splits two ways: a TRACKED offset
        // refused was freed concurrently (census stale — the re-plan no
        // longer sees it); an UNTRACKED offset (a mapping the allocator
        // walk does not account — the staged-promoted class) has no
        // patch/free exposure at all and moves pin-less, its frees
        // no-oping naturally.
        let mut pinned = true;
        match br.pin_block_validated(&task.base_key) {
            crate::block_allocator::PinOutcome::Pinned => {}
            crate::block_allocator::PinOutcome::PinnedUnstable => {
                let _ = br.free_block(&task.base_key).await; // undo the pin
                METRICS
                    .evacuate_deferred_staged_blocks
                    .fetch_add(1, Ordering::Relaxed);
                return MoveOutcome::Deferred;
            }
            crate::block_allocator::PinOutcome::Refused => {
                if br.block_refcount(&task.base_key).is_some() {
                    return MoveOutcome::Superseded;
                }
                log::info!(
                    "mover: source {} is allocator-untracked (staged-promoted class) — \
                     moving pin-less; the physical source block is not reclaimable here \
                     (remount allocator rebuild owns it)",
                    task.base_key
                );
                pinned = false;
            }
        }
        // The pinned source rides the mover ledger for the whole
        // copy+publish window (PR VL9 pin b): the pin holds refcount =
        // census-refs + 1, which an overlapping online fsck would
        // otherwise report as a C3 refcount mismatch — a false positive
        // on live mover state (G-VL-5(a) FP=0 with an active drain).
        if pinned {
            mover_ledger_insert(&task.base_key);
        }
        // From here on a taken pin MUST be released on every path.
        let unpin = |key: String| async move {
            if pinned {
                mover_ledger_remove(&key);
                let _ = br.free_block(&key).await;
            }
        };

        // 1. Copy: read the stored image (raw, transform-opaque —
        //    movers copy stored bytes verbatim, §9), write to the
        //    placement-chosen destination, verify by device read-back.
        let _charge = CopyCharge::new(block_size);
        let data = match br.read_block(&task.base_key, block_size as usize).await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("mover: source read of {} failed: {e}", task.base_key);
                unpin(task.base_key.clone()).await;
                return MoveOutcome::Deferred;
            }
        };
        let src_hash = xxhash_rust::xxh3::xxh3_64(&data);

        // Destination + allocation per the task's objective (§5.7): the
        // drain/rebalance fill-balance pick verbatim, or the VL7
        // contiguity-aware picks.
        let (dst_id, dst_alloc, dst_dev, dst_off) = match &task.dest_pick {
            DestPick::Balance => {
                let dest = match task.dest_hint.as_deref() {
                    Some(hint) if br.placement_eligible(hint) => br
                        .get_backend(hint)
                        .map(|(alloc, dev)| (hint.to_string(), alloc, dev)),
                    _ => br.pick_fill_destination(drain_victim.unwrap_or(&task.src_id)),
                };
                let (dst_id, dst_alloc, dst_dev) = match dest {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("mover: no destination for {}: {e}", task.base_key);
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Deferred;
                    }
                };
                match dst_alloc.allocate_block().await {
                    Ok(o) => (dst_id, dst_alloc, dst_dev, o),
                    Err(e) => {
                        log::warn!("mover: destination allocation on '{dst_id}' failed: {e}");
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Deferred;
                    }
                }
            }
            DestPick::CompactLow => {
                // D1: the lowest same-backend gap strictly below the
                // source — no gap means nothing to gain this pass (the
                // re-plan revisits; never a fresh tail mint).
                let (alloc, dev) = match br.get_backend(&task.src_id) {
                    Ok(x) => x,
                    Err(e) => {
                        log::warn!("mover: compaction backend '{}' gone: {e}", task.src_id);
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Deferred;
                    }
                };
                let below = task.src_offset / alloc.chunk_size().max(1);
                match alloc.allocate_block_below(below) {
                    Some(o) => (task.src_id.clone(), alloc, dev, o),
                    None => {
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Superseded;
                    }
                }
            }
            DestPick::BackendAscending { be_id, floor } => {
                // D2: the named backend, lowest free at-or-above the
                // file's ascending floor (fresh-tail fallback keeps the
                // sequence strictly ascending — the convergence
                // invariant: one pass leaves the file same-backend
                // ascending, so the next plan finds nothing).
                if !br.placement_eligible(be_id) {
                    unpin(task.base_key.clone()).await;
                    return MoveOutcome::Deferred;
                }
                let (alloc, dev) = match br.get_backend(be_id) {
                    Ok(x) => x,
                    Err(e) => {
                        log::warn!("mover: locality backend '{be_id}' gone: {e}");
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Deferred;
                    }
                };
                let min_idx = floor.load(Ordering::Relaxed);
                let off = match alloc.allocate_block_at_or_above(min_idx) {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("mover: locality allocation on '{be_id}' failed: {e}");
                        unpin(task.base_key.clone()).await;
                        return MoveOutcome::Deferred;
                    }
                };
                floor.store(off / alloc.chunk_size().max(1) + 1, Ordering::Relaxed);
                (be_id.clone(), alloc, dev, off)
            }
        };
        // PR VL6a: register this task as the destination's live owner in
        // the in-flight allocation registry (fsck C2 exemption) for the
        // whole copy→publish window; the guard drops after every
        // referencer published (or the failure path freed the block).
        let _dst_inflight = dst_alloc.inflight_register(dst_off);
        // The destination undo is a never-published cleanup (the offset was
        // pre-allocated and its publish never happened) — co-writer-aware
        // through the abandon arm (belt-and-suspenders: movers run on the
        // D0 coordinator, which a co-writer never is).
        let fail_dst = |off: u64, alloc: Arc<crate::block_allocator::BlockAllocator>| async move {
            let _ = alloc.abandon_unpublished_offset(off).await;
        };
        if let Err(e) = dst_dev.write_block(dst_off, data.clone()).await {
            log::warn!("mover: destination write on '{dst_id}' failed: {e}");
            fail_dst(dst_off, dst_alloc.clone()).await;
            unpin(task.base_key.clone()).await;
            return MoveOutcome::Deferred;
        }
        // Verify: device read-back against the source image hash.
        match dst_dev.read_block(dst_off, block_size as usize).await {
            Ok(back) if xxhash_rust::xxh3::xxh3_64(&back) == src_hash => {}
            Ok(_) => {
                log::error!(
                    "mover: verify MISMATCH on '{dst_id}' offset {dst_off} — destination \
                     freed, source untouched"
                );
                fail_dst(dst_off, dst_alloc.clone()).await;
                unpin(task.base_key.clone()).await;
                return MoveOutcome::Deferred;
            }
            Err(e) => {
                log::warn!("mover: verify read on '{dst_id}' failed: {e}");
                fail_dst(dst_off, dst_alloc.clone()).await;
                unpin(task.base_key.clone()).await;
                return MoveOutcome::Deferred;
            }
        }
        dst_alloc.publish_block(dst_off);
        let dst_base = br.persist_block_key(&dst_id, dst_off);

        // 2. Pre-publish refcount transfer (§5.4 step 2): raise the
        //    destination to the full reference count BEFORE any
        //    referencer publishes — no window where dst refcount <
        //    published references. The raised offsets are recorded in
        //    the pre-publish refcount ledger (coordinator-visible; the
        //    future fsck C3 consults it while a mover job is active).
        let shared = task.refs.len() > 1;
        for _ in 1..task.refs.len() {
            if !br.increment_refcount(&dst_base) {
                log::error!("mover: pre-publish refcount raise on {dst_base} refused");
                let _ = br.free_block(&dst_base).await;
                unpin(task.base_key.clone()).await;
                return MoveOutcome::Deferred;
            }
        }
        mover_ledger_insert(&dst_base);

        // 3. Publish per referencer, ascending ino (§5.4 step 2), each
        //    under the block's flush lock (lattice 3 — the final
        //    check-and-move) and the ino's CURRENT fencing token (read,
        //    never incremented — §5.1.5.5).
        let mut published = 0usize;
        for r in &task.refs {
            fire_pre_publish_hook(r.ino, r.block_idx).await;
            let flush_lock = crate::fuse_client::BLOCK_FLUSH_LOCKS.get_lock(r.ino, r.block_idx);
            let _flush_guard = flush_lock.lock().await;
            let superseded = if !(ctx.quiesce)(r.ino, r.block_idx as u64) {
                // Went non-quiescent since the gate: leave it for the
                // re-plan; the raised dst reference is released below.
                METRICS
                    .evacuate_deferred_staged_blocks
                    .fetch_add(1, Ordering::Relaxed);
                true
            } else {
                let suffix = decoration_suffix(&r.mapping, &task.base_key);
                let new_mapping = format!("{dst_base}{suffix}");
                let entries = [(r.block_idx, r.mapping.clone(), new_mapping)];
                let token = router.dlm.get_fencing_token_ino(r.ino);
                match router
                    .merge_block_mappings(
                        r.ino,
                        crate::routing::BlockMapOp::MergeExpected(&entries),
                        0,
                        crate::routing::LayoutFlip::KeepLayout,
                        token,
                    )
                    .await
                {
                    Ok(displaced) if displaced.iter().any(|d| d == &r.mapping) => {
                        // Free the displaced source reference —
                        // clone-aware decrement; the terminal decrement
                        // punches (§5.1.5 step 4).
                        let _ = br.free_block(&r.mapping).await;
                        published += 1;
                        false
                    }
                    Ok(_) => {
                        // Expected-mismatch: a foreground write replaced
                        // the mapping (or a truncate pruned it) — the
                        // FIND-M11-A contractual no-op.
                        METRICS
                            .evacuate_stale_token_noops
                            .fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    Err(crate::error::SqueezefsError::FencingTokenExpired { .. }) => {
                        METRICS
                            .evacuate_stale_token_noops
                            .fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    Err(e) => {
                        log::warn!(
                            "mover: publish for ino {} block {} deferred: {e}",
                            r.ino,
                            r.block_idx
                        );
                        true
                    }
                }
            };
            if superseded {
                // Release the reference raised for this referencer.
                let _ = br.free_block(&dst_base).await;
            }
        }
        mover_ledger_remove(&dst_base);

        // 4. Undo the mover's own pin on the source. Every published
        //    referencer already freed one source reference; once the
        //    last reference (this pin) drops, the terminal free punches.
        unpin(task.base_key.clone()).await;

        if published > 0 {
            METRICS
                .evacuate_blocks_moved
                .fetch_add(1, Ordering::Relaxed);
            METRICS
                .evacuate_bytes_moved
                .fetch_add(block_size, Ordering::Relaxed);
            if shared {
                METRICS
                    .evacuate_shared_blocks_moved
                    .fetch_add(1, Ordering::Relaxed);
            }
            MoveOutcome::Moved
        } else {
            // Nothing published: the destination's references have all
            // been released (terminal free punched it) — the §5.4
            // supersession outcome, or a deferral.
            MoveOutcome::Superseded
        }
    }

    // -----------------------------------------------------------------
    // PK6: the re-pack compaction mover (design-small-file-packing §5.8)
    // -----------------------------------------------------------------

    /// `squeezefs defrag --pack` — one victim at a time, throttled (KD-3),
    /// checkpointed on the fabric cadence, converging by re-plan (KD-6:
    /// the plan is regenerated from CURRENT durable state every pass —
    /// idempotent by construction, so a kill-9 mid-pass costs nothing but
    /// the pass) and completing best-effort at the first no-progress pass
    /// or the defrag pass cap. Gated by the packing lever: the WHOLE arm —
    /// a lever-OFF mount refuses loud (an adopted record from a lever-ON
    /// era included) and moves nothing.
    async fn run_defrag_pack(&self, job_id: &str, ctl: &JobCtl, volume_id: Option<&str>) {
        let Some(ctx) = self.mover.as_ref() else {
            self.fail_job(
                job_id,
                ctl,
                "defrag --pack needs a mover context (not wired on this fabric)",
            )
            .await;
            return;
        };
        if !crate::routing::small_file_packing_enabled() {
            self.fail_job(
                job_id,
                ctl,
                "defrag --pack refused: SQUEEZEFS_SMALL_FILE_PACKING is off on this mount — the \
                 compaction arm is gated by the packing lever (design-small-file-packing §6, \
                 PK6); nothing moved",
            )
            .await;
            return;
        }
        let mut last_checkpoint = std::time::Instant::now();
        let mut since_checkpoint = 0u64;
        let mut passes = 0u32;
        loop {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                self.park_paused(job_id, ctl).await;
                return;
            }
            // The planner reads free-list state through the census's
            // refcounts; displaced-source frees ride the reclaim queue —
            // settle them first (the mover cadence, never the write path).
            ctx.router.backend_router.reclaim_drain().await;
            // A compaction pass IS a promotion batch of the packer's: an
            // OQ-1 `StorageFull` stop from an earlier batch re-arms here
            // (the pass frees space as it goes).
            ctx.router.packer.begin_promotion_batch();
            let plan = match plan_pack_compaction(&self.meta, ctx, volume_id).await {
                Ok(p) => p,
                Err(e) => {
                    self.fail_job(job_id, ctl, &format!("defrag --pack plan failed: {e}"))
                        .await;
                    return;
                }
            };
            ctl.tasks_total.store(
                ctl.done.load(Ordering::Relaxed) + plan.victims.len() as u64,
                Ordering::Relaxed,
            );
            if plan.victims.is_empty() {
                if plan.skipped_lone_victim {
                    log::info!(
                        "job {job_id}: defrag --pack found one victim and no open pack with \
                         room for it — re-packing it into a fresh block would free nothing \
                         this pass (design-small-file-packing §5.8); nothing moved"
                    );
                }
                break;
            }
            METRICS.pack_compactions.fetch_add(1, Ordering::Relaxed);
            log::info!(
                "job {job_id}: defrag --pack pass {}: {} victim block(s), {} live B to re-pack, \
                 {} B reclaimable",
                passes + 1,
                plan.victims.len(),
                plan.victims.iter().map(|v| v.live_bytes).sum::<u64>(),
                plan.victims
                    .iter()
                    .map(|v| crate::block_allocator::CHUNK_SIZE - v.live_bytes)
                    .sum::<u64>()
            );
            let mut moved_this_pass = 0u64;
            let mut deferred_this_pass = 0u64;
            for victim in &plan.victims {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                    break;
                }
                let start = std::time::Instant::now();
                match self.compact_one(ctx, victim).await {
                    MoveOutcome::Moved => {
                        moved_this_pass += 1;
                        ctl.done.fetch_add(1, Ordering::Relaxed);
                        METRICS.job_tasks_done.fetch_add(1, Ordering::Relaxed);
                    }
                    MoveOutcome::Deferred => {
                        deferred_this_pass += 1;
                        METRICS
                            .pack_compaction_deferred
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    MoveOutcome::Superseded => {}
                }
                since_checkpoint += 1;
                if since_checkpoint >= CHECKPOINT_TASKS
                    || last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS)
                {
                    let _ = self.checkpoint(job_id, ctl).await;
                    since_checkpoint = 0;
                    last_checkpoint = std::time::Instant::now();
                }
                // KD-3: duty-cycle throttle, live re-read per victim.
                Self::duty_park(ctl, start.elapsed()).await;
            }
            // Space RETURNED, not merely queued, before the gauges re-read.
            ctx.router.backend_router.reclaim_drain().await;
            let _ = self.checkpoint(job_id, ctl).await;
            passes += 1;
            if ctl.cancelled.load(Ordering::SeqCst) || ctl.paused.load(Ordering::SeqCst) {
                continue;
            }
            if moved_this_pass == 0 || passes >= DEFRAG_MAX_PASSES {
                if passes >= DEFRAG_MAX_PASSES {
                    log::info!(
                        "job {job_id}: defrag --pack completed at the {DEFRAG_MAX_PASSES}-pass \
                         cap with work remaining — re-invoke to continue (KD-6 re-plan)"
                    );
                }
                break;
            }
            if deferred_this_pass > 0 {
                squeezefs_ipc::sqz_time::sleep(REPLAN_BACKOFF).await;
            }
        }
        if ctl.cancelled.load(Ordering::SeqCst) {
            let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
            ctl.set_state(JobState::Cancelled);
            return;
        }
        if ctl.paused.load(Ordering::SeqCst) {
            self.park_paused(job_id, ctl).await;
            return;
        }
        // The pack face re-gauges from the moved state (walk-priced, the
        // D2/D4 precedent) beside the cheap D1/D3 refresh.
        if let Err(e) = crate::defrag::measure_pack(&self.meta, &ctx.router).await {
            log::warn!("job {job_id}: pack occupancy re-measure after compaction failed: {e}");
        }
        let router = ctx.router.clone();
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::defrag::refresh_d1_d3_gauges(&router)
        })
        .await;
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        METRICS.job_completed.fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// Compact ONE victim pack block (design-small-file-packing §5.8 —
    /// `move_one`'s discipline with the three stated deviations): the
    /// open-pack and quiescence gates, the source pin (the mover ledger
    /// covers it), then per distinct live `off` window one ranged read at
    /// `max(len)` re-packed into the open pack through the router's
    /// primitive, the destination's reference count raised to the window's
    /// REFERENCER count before any publish, every referencer republished
    /// `MergeExpected(old → dst:off':len_i)` under its flush lock + the
    /// quiesce re-check + its CURRENT token (ascending ino), each published
    /// referencer freeing its source reference (nonterminal — the pin holds
    /// the victim) and each refused one releasing one destination
    /// reference; the pin's release is the victim's LAST reference iff every
    /// tenant left — the terminal free through the ordinary ladder (never a
    /// direct free). Every reference release runs outside the 3.5 guard
    /// (RES-1: the merge primitive drops it before returning).
    async fn compact_one(&self, ctx: &MoverCtx, victim: &PackVictim) -> MoveOutcome {
        let router = &ctx.router;
        let br = &router.backend_router;
        let chunk = crate::block_allocator::CHUNK_SIZE;

        // §5.8 (3): never an OPEN pack — the packer is still filling it
        // (the pass's own destination included).
        if pack_ledger_contains(&victim.base_key)
            || br
                .allocator_for_key(&victim.base_key)
                .is_some_and(|(alloc, offset)| alloc.inflight_contains(offset))
        {
            METRICS
                .pack_mover_open_defers
                .fetch_add(1, Ordering::Relaxed);
            return MoveOutcome::Deferred;
        }
        // §5.8 (3): every referencer quiescent — the mount's probe carries
        // the resident-ring clause (`pack_mover_resident_defers`).
        for r in &victim.refs {
            if !(ctx.quiesce)(r.ino, r.block_idx as u64) {
                return MoveOutcome::Deferred;
            }
        }
        // Pin the source for the copy window (`move_one`'s discipline);
        // an unstable word defers, a freed-meanwhile block is superseded,
        // an untracked one moves pin-less.
        let mut pinned = true;
        match br.pin_block_validated(&victim.base_key) {
            crate::block_allocator::PinOutcome::Pinned => {}
            crate::block_allocator::PinOutcome::PinnedUnstable => {
                let _ = br.free_block(&victim.base_key).await;
                return MoveOutcome::Deferred;
            }
            crate::block_allocator::PinOutcome::Refused => {
                if br.block_refcount(&victim.base_key).is_some() {
                    return MoveOutcome::Superseded;
                }
                log::info!(
                    "defrag --pack: victim {} is allocator-untracked — re-packing pin-less \
                     (the physical block is not reclaimable here)",
                    victim.base_key
                );
                pinned = false;
            }
        }
        if pinned {
            mover_ledger_insert(&victim.base_key);
        }
        let _charge = CopyCharge::new(victim.live_bytes);

        let mut published_total = 0usize;
        let mut deferred = false;
        for w in &victim.windows {
            let longest = &victim.refs[w.longest];
            let repacked = match router.repack_window(longest.ino, &longest.mapping).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    // OQ-1: the pack arm is stopped (`StorageFull`) — the
                    // rest of this victim waits for the next pass.
                    deferred = true;
                    break;
                }
                Err(e) => {
                    log::warn!(
                        "defrag --pack: re-packing window {}:{} of {} deferred: {e}",
                        w.off,
                        w.max_len,
                        victim.base_key
                    );
                    deferred = true;
                    continue;
                }
            };
            // Pre-publish reference transfer (§5.8 deviation 2): the
            // destination carries one reference per REFERENCER before any
            // publish — the handle's own plus `refs − 1` raised. The pack's
            // pin is live (the block is open), so a refusal is structural.
            let mut raised = 0usize;
            for _ in 1..w.refs.len() {
                if br.increment_refcount(&repacked.base_key) {
                    raised += 1;
                } else {
                    break;
                }
            }
            if raised + 1 < w.refs.len() {
                crate::note_invariant_tripwire(
                    "pack_compaction_raise_refused",
                    &format!(
                        "the open pack {} refused a pre-publish reference raise",
                        repacked.base_key
                    ),
                );
                for _ in 0..raised {
                    let _ = repacked
                        .tenant
                        .pack
                        .allocator
                        .release_pack_reference(
                            br,
                            &repacked.base_key,
                            crate::block_allocator::PackPublishOutcome::Known,
                        )
                        .await;
                }
                router.abandon_repacked_window(repacked).await;
                deferred = true;
                continue;
            }
            // Publish per referencer, ascending ino, each with its OWN len
            // at the shared destination slot.
            let mut published = 0usize;
            let mut released = 0usize;
            for &ri in &w.refs {
                let r = &victim.refs[ri];
                let len_i = match router.parse_block_mapping(&r.mapping) {
                    Ok((_, _, sz, true)) => sz as u64,
                    _ => {
                        released += 1;
                        continue;
                    }
                };
                fire_pre_publish_hook(r.ino, r.block_idx).await;
                let flush_lock = crate::fuse_client::BLOCK_FLUSH_LOCKS.get_lock(r.ino, r.block_idx);
                let _flush_guard = flush_lock.lock().await;
                let ok = if !(ctx.quiesce)(r.ino, r.block_idx as u64) {
                    false
                } else {
                    let new_mapping = format!("{}:{}:{len_i}", repacked.base_key, repacked.off);
                    let entries = [(r.block_idx, r.mapping.clone(), new_mapping)];
                    let token = router.dlm.get_fencing_token_ino(r.ino);
                    match router
                        .merge_block_mappings(
                            r.ino,
                            crate::routing::BlockMapOp::MergeExpected(&entries),
                            0,
                            crate::routing::LayoutFlip::KeepLayout,
                            token,
                        )
                        .await
                    {
                        Ok(displaced) if displaced.iter().any(|d| d == &r.mapping) => {
                            // The source reference moves: nonterminal
                            // while the pin (and any sibling) lives.
                            let _ = br.free_block(&r.mapping).await;
                            true
                        }
                        Ok(_) => {
                            METRICS
                                .evacuate_stale_token_noops
                                .fetch_add(1, Ordering::Relaxed);
                            false
                        }
                        Err(crate::error::SqueezefsError::FencingTokenExpired { .. }) => {
                            METRICS
                                .evacuate_stale_token_noops
                                .fetch_add(1, Ordering::Relaxed);
                            false
                        }
                        Err(e) => {
                            log::warn!("defrag --pack: publish for ino {} deferred: {e}", r.ino);
                            false
                        }
                    }
                };
                drop(_flush_guard);
                if ok {
                    published += 1;
                } else {
                    released += 1;
                }
            }
            // Settle the destination references the refused referencers
            // did not take — OUTSIDE the flush lock and the merge's 3.5
            // guard (RES-1). The handle's own reference is the LAST one
            // released (a window nobody took is an abandoned slot).
            for _ in 0..released.min(raised) {
                let _ = repacked
                    .tenant
                    .pack
                    .allocator
                    .release_pack_reference(
                        br,
                        &repacked.base_key,
                        crate::block_allocator::PackPublishOutcome::Known,
                    )
                    .await;
            }
            if published == 0 {
                router.abandon_repacked_window(repacked).await;
                deferred = true;
                continue;
            }
            repacked.tenant.pack.note_committed();
            METRICS
                .pack_compaction_windows_copied
                .fetch_add(1, Ordering::Relaxed);
            METRICS
                .pack_compaction_bytes_copied
                .fetch_add(w.max_len, Ordering::Relaxed);
            published_total += published;
            // The committed window's handle drops here — its in-flight
            // registration ends after the publishes are visible; the
            // reference it stood for is the layouts' now.
            drop(repacked);
        }
        METRICS
            .pack_compaction_tenants_moved
            .fetch_add(published_total as u64, Ordering::Relaxed);

        // The pin's release: the victim's LAST reference iff every tenant
        // left (moved here or deleted meanwhile) — the terminal free through
        // the ordinary ladder, reclaim queue and grace composed.
        if pinned {
            mover_ledger_remove(&victim.base_key);
            match br.free_block_verdict(&victim.base_key).await {
                Ok(true) => {
                    METRICS
                        .pack_compaction_blocks_freed
                        .fetch_add(1, Ordering::Relaxed);
                    METRICS
                        .pack_compaction_bytes_reclaimed
                        .fetch_add(chunk - victim.live_bytes, Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(e) => log::warn!(
                    "defrag --pack: releasing the source pin on {} failed: {e}",
                    victim.base_key
                ),
            }
        }
        if published_total > 0 {
            MoveOutcome::Moved
        } else if deferred {
            MoveOutcome::Deferred
        } else {
            MoveOutcome::Superseded
        }
    }
}

/// One compaction victim (PK6): a pack block at or below the half-chunk
/// line, its live windows and every referencer.
struct PackVictim {
    /// Clean base key (`clean_block_key` form).
    base_key: String,
    /// `Σ slot` over the live windows.
    live_bytes: u64,
    windows: Vec<crate::defrag::PackWindow>,
    refs: Vec<MoveRef>,
}

/// A compaction plan: the victims (ascending live bytes) and whether a
/// lone victim was left alone because moving it would free nothing.
struct PackPlan {
    victims: Vec<PackVictim>,
    skipped_lone_victim: bool,
}

/// Plan one compaction pass (design-small-file-packing §5.8): the census
/// over the scope (the named volume or every placement-eligible one), the
/// pack-shaped blocks folded by `off` at `max(len)`, victims = those at
/// or below the derived half-chunk line, ascending. A plan executes only
/// if it frees ≥ 1 block: two or more victims always net one (each is ≤
/// half a chunk, so two fit one block); a single victim frees one iff the
/// open pack already has room for its live windows — otherwise re-packing
/// it into a fresh block would trade one low block for another.
async fn plan_pack_compaction(
    meta: &Arc<RoutedMetaBackend>,
    ctx: &MoverCtx,
    volume_id: Option<&str>,
) -> Result<PackPlan, String> {
    let br = &ctx.router.backend_router;
    let mut scope: Vec<String> = Vec::new();
    match volume_id {
        Some(v) => {
            if br.backends.get(v).is_none() {
                return Err(format!(
                    "unknown data volume '{v}' (see `squeezefs volume list`)"
                ));
            }
            if !br.placement_eligible(v) {
                return Err(format!(
                    "volume '{v}' is not placement-eligible \
                     (draining/retired/unhealthy) — defrag --pack needs a writable volume"
                ));
            }
            scope.push(v.to_string());
        }
        None => {
            for entry in br.backends.iter() {
                if br.placement_eligible(entry.key()) {
                    scope.push(entry.key().clone());
                }
            }
            scope.sort();
        }
    }
    let mut plan = PackPlan {
        victims: Vec::new(),
        skipped_lone_victim: false,
    };
    if scope.is_empty() {
        return Ok(plan);
    }
    let census = census_for(meta, &ctx.router, &scope)
        .await
        .map_err(|e| format!("defrag --pack census failed: {e}"))?;
    for t in census.tasks {
        let Some(windows) = crate::defrag::pack_windows(&ctx.router, &t.refs) else {
            continue;
        };
        let live = crate::defrag::pack_live_bytes(&windows);
        if !crate::defrag::is_pack_victim(live) {
            continue;
        }
        // §5.8 (3) / §5.11: an OPEN pack is never a victim — the packer is
        // still filling it (this pass's own destination included), so it
        // is deferred at the plan, counted; `compact_one` re-checks as the
        // belt for a pack that opens between plan and move.
        if pack_ledger_contains(&t.base_key)
            || br
                .allocator_for_key(&t.base_key)
                .is_some_and(|(alloc, offset)| alloc.inflight_contains(offset))
        {
            METRICS
                .pack_mover_open_defers
                .fetch_add(1, Ordering::Relaxed);
            METRICS
                .pack_compaction_deferred
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }
        plan.victims.push(PackVictim {
            base_key: t.base_key,
            live_bytes: live,
            windows,
            refs: t.refs,
        });
    }
    plan.victims.sort_by(|a, b| {
        a.live_bytes
            .cmp(&b.live_bytes)
            .then(a.base_key.cmp(&b.base_key))
    });
    if plan.victims.len() == 1 && ctx.router.packer.open_room_bytes() < plan.victims[0].live_bytes {
        plan.victims.clear();
        plan.skipped_lone_victim = true;
    }
    Ok(plan)
}

enum MoverObjective {
    Drain {
        volume_id: String,
    },
    Rebalance,
    /// PR VL7 (§5.7 D1/D2): compaction + locality rewrites; `None` =
    /// every placement-eligible volume.
    Defrag {
        volume_id: Option<String>,
    },
}

enum MoveOutcome {
    Moved,
    Deferred,
    Superseded,
}

/// The suffix (decoration) a mapping string carries past its clean base
/// key (`:rel_off:packed_len` size-carrying forms) — preserved verbatim
/// on the destination mapping so the stored-image geometry survives the
/// move (the §5.4 "same mapping shape" law: a whole-block mapping stays
/// whole-block, a decorated one stays decorated). `pub` for the packed-
/// mapping wire-law contracts (`tests/packed_mapping_wire_tests.rs`).
pub fn decoration_suffix<'a>(mapping: &'a str, base: &str) -> &'a str {
    mapping.strip_prefix(base).unwrap_or("")
}

// The pre-publish refcount ledger (§5.4 step 2): destination base keys
// whose refcounts were raised ahead of publication. Coordinator-visible
// task state — the VL6 fsck C3 checker consults it while a mover job is
// active (offsets here are exempt from refcount findings).
static MOVER_PREPUBLISH_LEDGER: parking_lot::Mutex<Vec<String>> =
    parking_lot::Mutex::new(Vec::new());

fn mover_ledger_insert(key: &str) {
    MOVER_PREPUBLISH_LEDGER.lock().push(key.to_string());
}

fn mover_ledger_remove(key: &str) {
    let mut l = MOVER_PREPUBLISH_LEDGER.lock();
    if let Some(pos) = l.iter().position(|k| k == key) {
        l.swap_remove(pos);
    }
}

/// Snapshot of the pre-publish refcount ledger (§5.4 step 2) — the
/// mover-ledger surface fsck's C3 consultation (PR VL6a) reads.
pub fn mover_prepublish_ledger() -> Vec<String> {
    MOVER_PREPUBLISH_LEDGER.lock().clone()
}

/// Test seam (PR VL6a contracts): park a key in the pre-publish ledger
/// exactly as a live mover task does — the fsck exemption tests need
/// the ledger populated without racing a real mover's timing.
pub fn test_mover_ledger_insert(key: &str) {
    mover_ledger_insert(key);
}

/// Test seam: the task-terminal ledger removal.
pub fn test_mover_ledger_remove(key: &str) {
    mover_ledger_remove(key);
}

// The pack-open ledger (design-small-file-packing §5.3): the base keys of
// the small-file packer's OPEN pack blocks, entered at OPEN (before any
// window in which the block's +1 pin is declared to nobody) and left after
// the seal's pin release lands. fsck's shared C2/C3 arm consults it beside
// the mover pre-publish ledger: the pin is the ONLY discrepancy it ever has
// to excuse — the tenants' transient references are the in-flight
// registry's (one registration per tenant from reserve to commit).
static PACK_OPEN_LEDGER: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

pub(crate) fn pack_ledger_insert(key: &str) {
    PACK_OPEN_LEDGER.lock().push(key.to_string());
}

pub(crate) fn pack_ledger_remove(key: &str) {
    let mut l = PACK_OPEN_LEDGER.lock();
    if let Some(pos) = l.iter().position(|k| k == key) {
        l.swap_remove(pos);
    }
}

/// Snapshot of the pack-open ledger — fsck's shared C2/C3 consultation and
/// the stats census read it.
pub fn pack_open_ledger() -> Vec<String> {
    PACK_OPEN_LEDGER.lock().clone()
}

/// Is `key` an OPEN pack block's base key? The mover's per-candidate probe
/// (§5.11) — one lock, no clone.
fn pack_ledger_contains(key: &str) -> bool {
    PACK_OPEN_LEDGER.lock().iter().any(|k| k == key)
}

/// Test seam — the in-process kill-9 analog's teardown: a real kill-9
/// loses this process-global ledger with the process, but an in-process
/// "crash" (the fixture dropped without a seal) leaves the dead mount's
/// open-pack entries behind, and a fresh fixture in the same process mints
/// the SAME stamped base keys (fresh volume ⇒ same era and sequence), so
/// a stale entry would make the compaction mover defer a live block as
/// "open" (the pack_compaction suite's crash contract).
pub fn test_pack_ledger_clear() {
    PACK_OPEN_LEDGER.lock().clear();
}

// ---------------------------------------------------------------------------
// Rebalance planning (§5.3 step 6 / §5.7 bounded pass)
// ---------------------------------------------------------------------------

struct RebalanceLeg {
    source: String,
    dest: String,
    blocks: u64,
}

/// The bounded-pass plan: bring every volume below `mean − 10 pp` up to
/// that band edge by moving blocks from above-mean volumes (fullest
/// first), never pushing a source below the mean. Empty = balanced.
fn plan_rebalance(router: &crate::routing::DataRouter) -> Vec<RebalanceLeg> {
    let br = &router.backend_router;
    let block_size = router.block_size.load(Ordering::Relaxed).max(1);
    let mut rows: Vec<(String, u64, u64)> = Vec::new(); // (id, used, cap)
    for entry in br.backends.iter() {
        let be_id = entry.key();
        if !br.placement_eligible(be_id) {
            continue;
        }
        let alloc = &entry.value().block_allocator;
        let cap = alloc.capacity_bytes();
        if cap == 0 {
            continue;
        }
        let used = alloc.get_used_blocks().saturating_mul(alloc.chunk_size());
        rows.push((be_id.clone(), used, cap));
    }
    if rows.len() < 2 {
        return Vec::new();
    }
    let total_used: u64 = rows.iter().map(|r| r.1).sum();
    let total_cap: u64 = rows.iter().map(|r| r.2).sum();
    if total_cap == 0 {
        return Vec::new();
    }
    let mean = total_used as f64 / total_cap as f64;

    let fill = |used: u64, cap: u64| used as f64 / cap as f64;
    let mut under: Vec<(String, u64)> = rows
        .iter()
        .filter(|(_, u, c)| fill(*u, *c) < mean - REBALANCE_BAND)
        .map(|(id, u, c)| {
            let deficit = ((mean - REBALANCE_BAND) * *c as f64 - *u as f64).max(0.0) as u64;
            (id.clone(), deficit / block_size)
        })
        .filter(|(_, blocks)| *blocks > 0)
        .collect();
    under.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    let mut over: Vec<(String, u64)> = rows
        .iter()
        .filter(|(_, u, c)| fill(*u, *c) > mean)
        .map(|(id, u, c)| {
            let surplus = (*u as f64 - mean * *c as f64).max(0.0) as u64;
            (id.clone(), surplus / block_size)
        })
        .filter(|(_, blocks)| *blocks > 0)
        .collect();
    over.sort_by_key(|(_, b)| std::cmp::Reverse(*b));

    let mut legs = Vec::new();
    let mut over_iter = over.into_iter();
    let mut cur = over_iter.next();
    for (dest, mut want) in under {
        while want > 0 {
            let Some((src, have)) = cur.as_mut() else {
                break;
            };
            let take = want.min(*have);
            if take > 0 {
                legs.push(RebalanceLeg {
                    source: src.clone(),
                    dest: dest.clone(),
                    blocks: take,
                });
                want -= take;
                *have -= take;
            }
            if *have == 0 {
                cur = over_iter.next();
            }
        }
    }
    legs
}

/// Clip a rebalance census to the plan's per-leg block budgets and tag
/// each task with its planned destination — THE bounded-pass property:
/// the pass moves at most the planned blocks, then completes.
fn bound_rebalance_census(census: &mut Census, plan: &[RebalanceLeg], _block_size: u64) {
    // Rebalance never relocates indirect blobs (they follow their maps
    // on ordinary merges) — drains own that machinery.
    census.blob_relocations.clear();
    let mut budgets: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
    for leg in plan {
        budgets
            .entry(leg.source.as_str())
            .or_default()
            .push((leg.dest.as_str(), leg.blocks));
    }
    let mut bounded = Vec::new();
    for mut task in census.tasks.drain(..) {
        let Some(legs) = budgets.get_mut(task.src_id.as_str()) else {
            continue;
        };
        let Some(slot) = legs.iter_mut().find(|(_, left)| *left > 0) else {
            continue;
        };
        task.dest_hint = Some(slot.0.to_string());
        slot.1 -= 1;
        bounded.push(task);
    }
    census.tasks = bounded;
    census.distinct_blocks = census.tasks.len() as u64;
}

// ---------------------------------------------------------------------------
// PR VL7 — defrag planning (§5.7 D1/D2)
// ---------------------------------------------------------------------------

/// Resolve a parsed backend id to its REGISTERED volume id, honoring the
/// `backend_0`/legacy default-slot aliases (the census walk's matching
/// law, inverted): the record whose registered Arcs ARE the default slot
/// owns bare/aliased keys.
fn canonical_backend_id(br: &crate::routing::BackendRouter, be_id: &str) -> String {
    if be_id != "backend_0" && be_id != "squeezefs" {
        return be_id.to_string();
    }
    for entry in br.backends.iter() {
        if Arc::ptr_eq(&entry.value().device, &br.default_device)
            && Arc::ptr_eq(&entry.value().block_allocator, &br.default_allocator)
        {
            return entry.key().clone();
        }
    }
    be_id.to_string()
}

/// Plan one defrag pass (§5.7): D2 locality rewrites first (refcount-1,
/// quiescent-gated at move time, whole-file logical order onto the
/// plurality-eligible backend, ascending picks), then D1 tail compaction
/// (referenced blocks past the volume's compaction frontier — `used`
/// blocks fit `[0, used)` exactly — into low same-backend gaps). Shared
/// blocks compact fine (the §5.4 move-once machinery); D2 skips them —
/// clones are never de-shared. Indirect map blobs are left to the drain
/// machinery (they relocate on ordinary merges; a defrag never forces
/// them).
async fn plan_defrag(
    meta: &Arc<RoutedMetaBackend>,
    ctx: &MoverCtx,
    volume_id: Option<&str>,
) -> Result<Census, String> {
    let br = &ctx.router.backend_router;

    // Scope: the named volume or every placement-eligible one.
    let mut scope: Vec<String> = Vec::new();
    match volume_id {
        Some(v) => {
            if br.backends.get(v).is_none() {
                return Err(format!(
                    "unknown data volume '{v}' (see `squeezefs volume list`)"
                ));
            }
            if !br.placement_eligible(v) {
                return Err(format!(
                    "volume '{v}' is not placement-eligible \
                     (draining/retired/unhealthy) — defrag needs a writable volume"
                ));
            }
            scope.push(v.to_string());
        }
        None => {
            for entry in br.backends.iter() {
                if br.placement_eligible(entry.key()) {
                    scope.push(entry.key().clone());
                }
            }
            scope.sort();
        }
    }
    let empty = || Census {
        tasks: Vec::new(),
        blob_relocations: Vec::new(),
        distinct_blocks: 0,
    };
    if scope.is_empty() {
        return Ok(empty());
    }

    let mut tasks: Vec<MoveTask> = Vec::new();
    let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();

    // D2: locality rewrites (their low-gap picks feed D1's goal too).
    let files = crate::defrag::walk_striped_files(meta, &ctx.router)
        .await
        .map_err(|e| format!("defrag D2 walk failed: {e}"))?;
    for f in files {
        let (pairs, local) = crate::defrag::locality_pairs(&f.entries);
        if pairs == 0 || local == pairs {
            continue;
        }
        // Scope: the file must touch an in-scope volume.
        if !f
            .entries
            .iter()
            .any(|(_, _, be, _)| scope.contains(&canonical_backend_id(br, be)))
        {
            continue;
        }
        // Target: the plurality backend among ELIGIBLE canonical owners.
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (_, _, be, _) in &f.entries {
            *counts.entry(canonical_backend_id(br, be)).or_default() += 1;
        }
        let mut cands: Vec<(String, u64)> = counts
            .into_iter()
            .filter(|(id, _)| br.placement_eligible(id))
            .collect();
        cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let Some((target, _)) = cands.into_iter().next() else {
            continue; // nowhere eligible to gather the file
        };
        // One ascending floor per file: the rewrite's convergence
        // invariant (each publish bumps it past its destination).
        let floor = Arc::new(AtomicU64::new(0));
        for (idx, mapping, be, off) in &f.entries {
            let clean = crate::routing::clean_block_key(mapping);
            // Refcount-1 only (§5.7: clones are never de-shared by D2).
            if br.block_refcount(&clean) != Some(1) {
                continue;
            }
            if !planned.insert(clean.clone()) {
                continue;
            }
            tasks.push(MoveTask {
                base_key: clean,
                src_id: canonical_backend_id(br, be),
                src_offset: *off,
                refs: vec![MoveRef {
                    ino: f.ino,
                    block_idx: *idx,
                    mapping: mapping.clone(),
                    staged: false,
                }],
                dest_hint: None,
                dest_pick: DestPick::BackendAscending {
                    be_id: target.clone(),
                    floor: floor.clone(),
                },
            });
        }
    }

    // D1: tail compaction over the scope's referenced census.
    let census = census_for(meta, &ctx.router, &scope)
        .await
        .map_err(|e| format!("defrag D1 census failed: {e}"))?;
    let mut frontier: HashMap<String, u64> = HashMap::new();
    for v in &scope {
        if let Some(be) = br.backends.get(v) {
            frontier.insert(v.clone(), be.value().block_allocator.get_used_blocks());
        }
    }
    let mut d1_tasks: Vec<MoveTask> = Vec::new();
    for mut t in census.tasks {
        if planned.contains(&t.base_key) {
            continue;
        }
        let Some(be) = br.backends.get(&t.src_id) else {
            continue;
        };
        let chunk = be.value().block_allocator.chunk_size().max(1);
        drop(be);
        let Some(&fr) = frontier.get(&t.src_id) else {
            continue;
        };
        if t.src_offset / chunk < fr {
            continue; // already below the compaction frontier
        }
        planned.insert(t.base_key.clone());
        t.dest_pick = DestPick::CompactLow;
        d1_tasks.push(t);
    }
    // (ino, logical idx) order: the ascending gap consumption then
    // preserves per-file physical order — compaction must not mint D2
    // work (refs are already ascending-sorted by the census).
    d1_tasks.sort_by_key(|t| {
        t.refs
            .first()
            .map(|r| (r.ino, r.block_idx))
            .unwrap_or((u64::MAX, u32::MAX))
    });
    tasks.extend(d1_tasks);

    let distinct_blocks = tasks.len() as u64;
    Ok(Census {
        tasks,
        blob_relocations: Vec::new(),
        distinct_blocks,
    })
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn hostname_lossy() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// The percentage duty-cycle throttle law (KD-3): after a task that ran
/// for `elapsed`, sleep `elapsed × (100 − pct) / pct` so task-active
/// time ≈ `pct` of wall time. `0` and `≥ 100` mean unthrottled.
pub fn job_throttle_sleep(elapsed: Duration, cpu_limit_pct: u32) -> Option<Duration> {
    if cpu_limit_pct >= 100 || cpu_limit_pct == 0 {
        None
    } else {
        let factor = (100 - cpu_limit_pct) as f64 / cpu_limit_pct as f64;
        let sleep_dur = elapsed.mul_f64(factor);
        if sleep_dur < Duration::from_millis(1) {
            None
        } else {
            Some(sleep_dur)
        }
    }
}
