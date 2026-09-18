//! The **§5.1.6 job-shard execution wire** (PR VL2b,
//! `docs/design-volume-lifecycle.md` §5.1.6 / KD-15).
//!
//! The minimal wire that lets every mounted client execute data-plane
//! job shards online. **Deliberate boundary (KD-15): this is a
//! job-shard execution transport, NOT a general networked DLM** —
//! foreground file-I/O locking stays process-local on the single writer
//! mount; the wire moves shard descriptors, heartbeats, and result
//! *proposals*, never file locks, and carries **no meta-write verb**:
//! the coordinator verifies and commits every result itself.
//!
//! Load-bearing laws implemented here:
//!
//! - **Storage-trust enrollment**: the coordinator writes a random
//!   session secret into the [`JOB_ENROLL_XATTR`] meta-KV record
//!   (behind the VL2 reserved-namespace FUSE screen); a worker proves
//!   storage membership with `HMAC-SHA256(secret, worker_id ‖
//!   server_nonce ‖ endpoint_nonce ‖ "hello")` over a
//!   **coordinator-issued** [`WireFrame::Challenge`] nonce — the wire
//!   proves and grants exactly what shared-storage access already
//!   grants, and only once per challenge (VAL-6, below).
//! - **Transport**: **[`crate::cluster_wire`]** (DLM stage S3) — the ONE
//!   cluster transport. This module no longer owns a codec, a connection
//!   cap, an accept-backoff ladder, a nonce registry, a proof MAC or a TLS
//!   construction: it owns the job-shard VOCABULARY and rides cluster_wire
//!   for all of it. What that changed, concretely: frames are **binary**
//!   (bincode, [`WIRE_SCHEMA`] 3 — `serde_json` decode was 32.6 µs on the
//!   64-checksum shard against §6.5's 10 µs custody budget), every
//!   post-enrollment frame carries a **session MAC** derived from the
//!   `job:enroll` secret (enrollment authenticated the handshake; the MAC
//!   authenticates the session), and a **CA-less TLS config is refused**
//!   rather than admitted as a lesser class — the accept-everything
//!   verifier is deleted from the tree. Network TCP/TLS is the sanctioned
//!   non-uring exception (AGENTS "Not uring" row).
//! - **Shard leases + fencing**: `{holder, lease_expiry (TTL 30 s),
//!   shard_fencing}` per shard; heartbeats every 10 s renew; expiry ⇒
//!   fencing bump + reassignment — **always with freshly allocated
//!   destinations; the expired lease's tuples enter a do-not-publish
//!   quarantine set** (the fresh-destination law: the dangerous zombie
//!   is a *live* one resuming DMA into its old destinations).
//! - **Verification at submission (Issue-30 law)**: a result carries
//!   `{shard_fencing, per-block checksums}`; stale fencing refuses
//!   (`job_remote_refused_stale`); mutating destinations are
//!   verify-read against the submitted checksums BEFORE publish —
//!   sampled under TLS, **mandatory-100 % on plaintext**.
//! - **WERO fence (rung 2)**: the coordinator acquires **Write
//!   Exclusive – Registrants Only** (rtype 3) on the data namespaces at
//!   first remote enrollment, releases at last departure, and preempts
//!   an expired worker host's registration where `RESCAP` supports it
//!   (`job_remote_pr_preempts`; guarantee class in
//!   `job_remote_fence_mode` — `pr` / `deferred-reclaim`).
//!
//! VL2b executes the fabric's [`JobType::Noop`] shards over the wire
//! (the movers land in VL4 through the [`ShardDeviceSeam`] — the
//! destination/verification machinery is exercised today against
//! [`FakeShardDevice`], the harness-surface-in-the-lib precedent).
//!
//! # VAL-6 — closed by the S3 port, not patched in place
//!
//! Execution-plan ruling **D2**: the listener stays **configurable and
//! default-bound `0.0.0.0`** with auto-discovered peers, so the fix was
//! never "close the port" — it is "make the open port safe". VAL-6 did
//! that here as an interim; **S3 moved every one of those bounds into
//! [`crate::cluster_wire`]**, where the next ten stages inherit them
//! instead of re-deriving them. The bounds an attacker-reachable listener
//! still enforces, now once for the whole cluster:
//!
//! - **Bounded framing**: body memory is committed only as bytes
//!   arrive ([`FRAME_CHUNK_BYTES`] per round), so a lying length prefix
//!   costs one chunk instead of [`MAX_FRAME_BYTES`]; pre-enrollment
//!   frames ride the small [`MAX_HELLO_FRAME_BYTES`] class; every
//!   started body carries a deadline (the 10 s timeout used to cover
//!   the HELLO frame alone).
//! - **Bounded connections**: `JobWireConfig::max_connections`
//!   (derived from the core count, `SQUEEZEFS_JOB_WIRE_MAX_CONNS`
//!   overrides absolute), an accept-error **backoff ladder** (the bare
//!   `continue` turned `EMFILE` into a busy loop), and a
//!   **self-draining** per-connection `JoinHandle` registry (RES-5:
//!   `handles` was push-only; the 2026-08-04 quiet-host law then made
//!   the prune trigger the serve task's own completion, never a later
//!   accept — `HandleReaper` on `JobWireHost`).
//! - **Enrollment freshness**: the coordinator speaks first with a
//!   [`WireFrame::Challenge`]; its nonce is **single-use** (replay
//!   registry) inside a **freshness window**
//!   (`JobWireConfig::enroll_freshness`).
//! - **The verification-strength ladder keys on an AUTHENTICATED
//!   channel** — never on the presence of a TLS object. VAL-6 keyed it on
//!   a CA pin ([`channel_authenticated`]) and pinned the CA-less TLS
//!   object to the plaintext class; **S3 refuses that configuration
//!   outright**, and the ladder's predicate is now
//!   `SessionAuthn::verify_sampling_admissible()`: authenticated (storage
//!   proof + session MAC, which plaintext now also is) **and**
//!   confidential (mTLS). Plaintext therefore keeps mandatory-100 %
//!   verify-reads exactly as before.
//! - **A configuration surface that exists**: [`JobWireConfig::from_env`]
//!   (`SQUEEZEFS_JOB_WIRE_*`) — `security` used to be hardwired `None`
//!   with no flag, env var, or config field able to populate it, so the
//!   listener's own warning recommended a configuration the binary
//!   could not express.
//!
//! **Why this module's tasks are NOT on cluster_wire's pinned service
//! pool.** §6.7's venue rule ("owner-side RPC handling runs on pinned
//! service threads, never on the conveyor's task") is about *lock/metadata
//! RPC* — work that would otherwise be dispatched onto a serialized
//! ~0.78 ms server at ρ ≈ 0.92. Job-shard traffic is not that: a shard
//! assignment is a coarse, second-scale unit whose device work rides
//! `RouterShardDevice`, which bridges the sync seam onto the mount runtime
//! with `block_in_place` — an operation that PANICS on a current-thread
//! runtime, i.e. on a pool lane. Moving this module's accept/dispatcher/
//! sweeper tasks onto the pool would therefore trade a real invariant for
//! a venue it does not need, and would rewrite the VAL-6 legs
//! (`retained_task_handles`, the accept-backoff gauge) that pin its
//! bounds. The pool is where **S4's** verbs land, on the same listener.
//!
//! **What S3 closed that VAL-6 deliberately left open**: per-frame
//! authentication after enrollment (the session MAC), a session key
//! derived from the storage secret and bound to the TLS exporter where
//! there is one, deletion of the accept-everything verifier itself, and
//! peer auto-discovery ([`crate::cluster_wire::discover_peers`] — DISC-1).
//! Still open by design: the literal `"localhost"` server name in the
//! dial-side TLS handshake, which is what the `cluster_tls` node certs
//! carry as SANs (a real SAN plan is its own change, and the CA pin plus
//! the storage-trust proof are what actually authenticate the peer).

use crate::cluster_wire::{
    self, session_framers, AuthnConfig, AuthnGate, ChannelClass, ClusterStream, ClusterWriteHalf,
    ConnGate, FrameClass, FrameTx, ProofClaim, SessionAuthn, SessionKey, Verdict,
};
use crate::data_custody::WeroHold;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::jobs::{job_throttle_sleep, JobCtl, JobFabric, JobType};
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use crate::sqz_sync::SqzMutex;
use crate::tiering::cluster_tls::ClusterSecurityConfig;

use serde::{Deserialize, Serialize};
use squeezefs_ipc::{sqz_blocking, sqz_channel, sqz_time};
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::xxh3_64;

/// The wire frame schema this build speaks. A hello carrying any other
/// value refuses loud, naming the field.
///
/// * `1` — the original self-chosen-nonce hello (replayable proof).
/// * `2` — the VAL-6 challenge handshake (coordinator-issued single-use
///   nonce), still JSON-framed.
/// * `3` — the **S3 `cluster_wire` port**: binary framing and a per-frame
///   session MAC. A schema-2 peer's JSON hello does not even decode as a
///   frame here, which is the honest outcome for a protocol whose byte
///   layout changed; the version field makes the refusal legible when the
///   bytes happen to parse.
/// * `4` — **KD-MW-16 fleet read shards** (rung 10c,
///   `docs/design-mw-fleet-jobs.md`): `Enroll` grew the worker
///   capability mask, `ShardDescriptor` the fleet residue pair, and the
///   vocabulary the `ReadShardResult`/`ShardAbandon` frames. Bincode is
///   positional, so the grown structs do not decode across the bump —
///   the schema field is what makes that refusal legible.
pub const WIRE_SCHEMA: u32 = 4;

/// Worker capability bit (`Enroll.caps`, KD-MW-16): this worker executes
/// **fleet READ shards** (fsck census/scrub residues through
/// [`ShardDeviceSeam::run_fleet_shard`]). Capability classing is
/// ROUTING, never security (design-mw-fleet-jobs §2): the coordinator
/// uses it to pick sessions, and a lying capability buys a shard the
/// worker can only abandon — re-leased, never trusted.
pub const CAP_FLEET_READ: u32 = 1 << 0;

/// The per-fabric enrollment-secret record on ino 1 (`job:` prefix ⇒
/// behind the VL2 reserved-namespace FUSE screen; readable only through
/// the meta backend — i.e. by principals that already hold storage).
pub const JOB_ENROLL_XATTR: &str = "job:enroll";

/// Default shard-lease TTL (§5.1.6: 30 s).
pub const LEASE_TTL: Duration = Duration::from_secs(30);
/// Default worker heartbeat cadence (§5.1.6: 10 s).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// The **post-enrollment** frame class: shard descriptors and result
/// proposals (blocks move over shared storage, not the wire). This is
/// cluster_wire's bulk class — one cap for every protocol on the wire.
pub const MAX_FRAME_BYTES: u32 = cluster_wire::BULK_MAX_FRAME_BYTES;

/// The **pre-enrollment** frame class cap: what an unauthenticated peer
/// gets to spend (cluster_wire's handshake class).
pub const MAX_HELLO_FRAME_BYTES: u32 = cluster_wire::HANDSHAKE_MAX_FRAME_BYTES;

/// The frame class a job-shard session speaks after enrollment.
const SESSION_CLASS: FrameClass = FrameClass::Bulk;

pub use crate::cluster_wire::{
    next_accept_backoff, ACCEPT_BACKOFF_MAX, ACCEPT_BACKOFF_START, FRAME_CHUNK_BYTES,
};

/// Worker-side bound on the enrollment exchange (dial → challenge →
/// hello → reply). The coordinator's own gate is
/// `JobWireConfig::handshake_timeout`. Also the floor of the fleet fsck
/// collect loop's progress deadline (`fsck::fleet_collect_progress_floor`):
/// one dial's own bound is the least a proposal may be granted.
pub const ENROLL_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Worker-side DMA/task batch size for per-batch lease re-validation
/// (§5.1.6 rung 1: a woken zombie aborts before its next batch).
const REVALIDATE_BATCH: usize = 16;

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One coordinator-pre-allocated destination tuple `(backend_id,
/// offset)` — allocation is meta-side and never crosses the wire as a
/// capability (the coordinator publishes; workers only fill).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DestTuple {
    pub backend_id: u32,
    pub offset: u64,
}

/// Per-block checksum a result proposal carries: what the worker wrote
/// (or read) at `dest`, for the coordinator's verify-read.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BlockChecksum {
    pub dest: DestTuple,
    pub len: u32,
    pub xxh3: u64,
}

/// The shard descriptor streamed at assignment: task list, source keys,
/// pre-allocated destination tuples, throttle, and the lease law.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ShardDescriptor {
    pub job_id: String,
    pub shard: u32,
    pub shard_fencing: u64,
    /// The task list (VL2b: the fabric's `Noop`; VL4 movers extend the
    /// enum, not the wire).
    pub job_type: JobType,
    /// Source keys for mutating jobs (empty for the Noop vehicle).
    pub source_keys: Vec<String>,
    /// Coordinator-pre-allocated, unpublished destinations.
    pub destinations: Vec<DestTuple>,
    /// Bytes per destination block.
    pub block_len: u32,
    /// Duty-cycle percentage (KD-3) applied per remote worker.
    pub throttle_pct: u32,
    pub lease_ttl_ms: u64,
    /// KD-MW-16: `Some((k, n))` = this is a **fleet READ shard** — the
    /// worker executes the ino-residue `k` of `n` through
    /// [`ShardDeviceSeam::run_fleet_shard`] and proposes the result as a
    /// [`WireFrame::ReadShardResult`]. `None` = the whole-job mutating
    /// shape (destinations + checksums), unchanged.
    pub fleet: Option<(u32, u32)>,
    /// **KD-PV-16**: this fleet shard is an **inode-plane** shard, not a
    /// census residue — the worker evaluates C9/C10 over the volumes IT
    /// owns and reports no census. `#[serde(default)]`: a pre-PR-6
    /// coordinator's frame decodes to `false`, which is the census shape
    /// verbatim, and a pre-PR-6 worker ignores the field and answers a
    /// residue-labelled report the coordinator then treats as a LOST
    /// plane shard (it never silently counts as coverage).
    #[serde(default)]
    pub inode_plane: bool,
}

/// What a fleet shard asks of the worker's seam
/// ([`ShardDeviceSeam::run_fleet_shard`]).
#[derive(Debug, Clone, Copy)]
pub struct FleetShardSpec {
    /// The ino residue this shard covers (`k` of `n`) — meaningless, and
    /// ignored, when [`Self::inode_plane`] is set.
    pub k: u32,
    pub n: u32,
    /// KD-3's duty cycle, applied per worker inside the walk.
    pub throttle_pct: u32,
    /// KD-PV-16: run the INODE PLANE over the volumes this node owns
    /// instead of the census residue.
    pub inode_plane: bool,
}

/// The §5.1.6 job-shard wire vocabulary — the frames, not the transport
/// (that is [`crate::cluster_wire`]).
///
/// Externally tagged **by requirement**: the S3 codec is bincode, which is
/// a non-self-describing format, so serde's internally tagged
/// representation (`#[serde(tag = "frame")]`, what the JSON era used)
/// cannot round-trip through it — it needs `deserialize_any`. The variant
/// index is the discriminant on the wire now.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum WireFrame {
    /// The coordinator speaks first (VAL-6): a single-use nonce inside
    /// a freshness window. The worker cannot choose its own challenge,
    /// so a captured hello is not a credential.
    Challenge {
        wire_schema: u32,
        server_nonce: String,
        /// How long the coordinator will accept this nonce (ms).
        freshness_ms: u64,
    },
    Enroll {
        wire_schema: u32,
        worker_id: String,
        /// The nonce from this connection's [`WireFrame::Challenge`].
        server_nonce: String,
        endpoint_nonce: String,
        /// hex `HMAC-SHA256(secret, worker_id ‖ server_nonce ‖
        /// endpoint_nonce ‖ "hello")`.
        hmac: String,
        /// The PR key this worker's HOST registered on the shared data
        /// namespaces (its own association — the coordinator can only
        /// preempt it, never register it). `None` ⇒ this worker rides
        /// the deferred-reclaim guarantee class.
        pr_key: Option<u64>,
        /// KD-MW-16: capability mask ([`CAP_FLEET_READ`]) — shard
        /// ROUTING, never an authentication claim.
        caps: u32,
    },
    EnrollOk {
        wire_schema: u32,
        heartbeat_ms: u64,
        lease_ttl_ms: u64,
    },
    EnrollRefused {
        reason: String,
    },
    ShardAssign {
        shard: ShardDescriptor,
    },
    Heartbeat {
        worker_id: String,
    },
    HeartbeatAck {
        lease_ttl_ms: u64,
    },
    ResultSubmit {
        job_id: String,
        shard: u32,
        shard_fencing: u64,
        checksums: Vec<BlockChecksum>,
    },
    ResultAck {
        job_id: String,
        shard: u32,
    },
    ResultRefused {
        job_id: String,
        shard: u32,
        reason: String,
    },
    /// KD-MW-16: a fleet READ shard's result proposal — the serialized
    /// shard report. Fencing-checked exactly like [`Self::ResultSubmit`];
    /// it carries no destinations/checksums because a read shard writes
    /// nothing (the coordinator's verification is the merge accounting +
    /// the finalize's own re-verification ladder, design-mw-fleet-jobs
    /// §4).
    ReadShardResult {
        job_id: String,
        shard: u32,
        shard_fencing: u64,
        payload: Vec<u8>,
    },
    /// KD-MW-16: the worker refuses/abandons its shard NOW (R5 Red, an
    /// unsupported capability, a failed local walk) — the PROMPT form of
    /// the lease-expiry law, so the coordinator re-leases without
    /// waiting out the TTL. Fencing-checked; a stale abandon is ignored.
    ShardAbandon {
        job_id: String,
        shard: u32,
        shard_fencing: u64,
        reason: String,
    },
}

/// Write one **unauthenticated** length-prefixed binary frame at the bulk
/// class — the handshake direction, and what the raw-connection tests
/// speak. Post-enrollment frames ride [`FrameTx`] instead, which adds the
/// session MAC.
pub fn write_frame<W: Write>(w: &mut W, frame: &WireFrame) -> std::io::Result<()> {
    cluster_wire::write_plain_frame(w, SESSION_CLASS, frame)
}

/// Read one **unauthenticated** frame at the bulk class
/// ([`MAX_FRAME_BYTES`], no body deadline); `Ok(None)` on clean EOF at a
/// frame boundary.
pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Option<WireFrame>> {
    read_frame_limited(r, MAX_FRAME_BYTES, None)
}

/// Read one frame under an explicit **class cap** and optional **body
/// deadline** — cluster_wire's reader, so the two properties VAL-6 added
/// hold for every protocol on the wire rather than for this one:
///
/// 1. The length prefix is a claim, never an allocation authority — the
///    body Vec grows [`FRAME_CHUNK_BYTES`] at a time *as bytes arrive*,
///    so a peer that declares 16 MiB and sends nothing costs one chunk.
/// 2. Once a body has started, `body_timeout` bounds the WHOLE body (a
///    dribbling peer is an error, not a parked thread). The length-prefix
///    read itself is deliberately unbounded here: an idle enrolled
///    session legitimately waits between frames, and its idle bound is
///    the session-level socket read timeout.
pub fn read_frame_limited<R: Read>(
    r: &mut R,
    max_len: u32,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<WireFrame>> {
    cluster_wire::read_plain_frame(r, max_len, body_timeout)
}

/// The enrollment proof: hex `HMAC-SHA256(secret, worker_id ‖
/// server_nonce ‖ endpoint_nonce ‖ "hello")` — computable only by a
/// principal that can read the meta volume's `job:enroll` record, and
/// bound to the coordinator's single-use challenge, so a captured proof
/// is not a reusable credential.
///
/// This IS [`cluster_wire::proof_mac`]: the S3 port did not change what a
/// worker computes, so a same-commit worker's proof is byte-identical
/// across the port. The name survives because the job-wire vocabulary and
/// its tests speak it.
pub fn enroll_hmac(
    secret: &[u8],
    worker_id: &str,
    server_nonce: &str,
    endpoint_nonce: &str,
) -> String {
    cluster_wire::proof_mac(secret, worker_id, server_nonce, endpoint_nonce)
}

// ---------------------------------------------------------------------------
// The device seam (VL4 plugs the movers in here)
// ---------------------------------------------------------------------------

/// The shard device seam: how the coordinator pre-allocates and
/// verify-reads destinations, and how workers fill them. VL2b's only
/// executable shard type is `Noop` (zero blocks in production); the VL4
/// movers replace [`NoopDeviceSeam`] with the real allocator +
/// `NvmeBlockDev` io_uring paths behind this same seam.
pub trait ShardDeviceSeam: Send + Sync {
    /// Destination blocks a shard of `job` pre-allocates.
    fn plan_blocks(&self, job: &JobType) -> usize;
    /// Bytes per destination block.
    fn block_len(&self) -> usize;
    /// Coordinator-side pre-allocation of `n` fresh, unpublished
    /// destination tuples. MUST never return a tuple it handed out
    /// before within this job fabric's lifetime (the fresh-destination
    /// law's allocator half).
    fn allocate(&self, n: usize) -> std::io::Result<Vec<DestTuple>>;
    /// Worker-side SOURCE read for copy shards (shared storage): the
    /// mover copy step over the wire — a worker fills each destination
    /// with the bytes behind the matching `source_keys` entry instead of
    /// the Noop pattern (VL4; the coordinator-side mover publish is what
    /// still keeps the mover job types off the wire in v1.1).
    fn read_source(&self, key: &str) -> std::io::Result<Vec<u8>>;
    /// Worker-side block fill (shared storage — the data plane).
    fn write_block(&self, dest: &DestTuple, data: &[u8]) -> std::io::Result<()>;
    /// Coordinator-side verify-read before publish.
    fn read_block(&self, dest: &DestTuple) -> std::io::Result<Vec<u8>>;
    /// KD-MW-16 (rung 10c): execute one fleet READ shard — the
    /// ino-residue `spec.k` of `spec.n` of `job`'s detect pass, or (since
    /// KD-PV-16) this owner's INODE PLANE when `spec.inode_plane` is set
    /// — and return the serialized shard report. Runs on the worker's own
    /// blocking lane; the KD-3 duty cycle applies PER WORKER inside the
    /// walk. Default: refused loud — a seam that does not implement the
    /// read class must never fake a report (capability classing is
    /// routing; the refusal re-leases).
    fn run_fleet_shard(&self, _job: &JobType, _spec: FleetShardSpec) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "this seam does not execute fleet read shards (KD-MW-16)",
        ))
    }
    /// DLM **S7** (pre-RC spec §6.7 "Recovery"): admit an expired lease's
    /// destinations to the ALLOCATOR's dead-epoch quarantine, so the
    /// fresh-destination law is enforced by the allocator instead of
    /// asserted after it has already answered — including for the offsets
    /// a later free (seam teardown, fsck repair, recovery) would otherwise
    /// return to the free list while the zombie worker can still DMA into
    /// them. Returns the newly admitted count.
    ///
    /// Default: a no-op. A device-less seam owns no allocator, and the
    /// in-memory fake's monotonic offsets are fresh by construction.
    fn quarantine(&self, _dests: &[DestTuple], _epoch: crate::data_custody::DeadEpoch) -> usize {
        0
    }
    /// DLM **S7**: release a dead epoch's cohort — the caller has a **drain
    /// proof** (the WERO preempt of the victim host landed, so its resumed
    /// DMA is device-rejected). Returns the count released.
    fn release_quarantine(&self, _epoch: crate::data_custody::DeadEpoch) -> usize {
        0
    }
}

/// Device-less seam for fabric-only wiring (unit tests, bare hosts):
/// Noop shards plan zero blocks, so allocation and device access are
/// structurally unreachable — reaching them is a bug, refused loud.
#[derive(Debug)]
pub struct NoopDeviceSeam;

impl ShardDeviceSeam for NoopDeviceSeam {
    fn plan_blocks(&self, _job: &JobType) -> usize {
        0
    }
    fn block_len(&self) -> usize {
        0
    }
    fn allocate(&self, n: usize) -> std::io::Result<Vec<DestTuple>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        Err(std::io::Error::other(
            "NoopDeviceSeam cannot allocate destinations — use RouterShardDevice",
        ))
    }
    fn read_source(&self, _key: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "NoopDeviceSeam has no device — use RouterShardDevice",
        ))
    }
    fn write_block(&self, _dest: &DestTuple, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "NoopDeviceSeam has no device — use RouterShardDevice",
        ))
    }
    fn read_block(&self, _dest: &DestTuple) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "NoopDeviceSeam has no device — use RouterShardDevice",
        ))
    }
}

/// The PRODUCTION seam (PR VL4): `BackendRouter`-backed device access —
/// allocation on placement-eligible volumes' allocators, block fill and
/// verify-reads through the per-volume `NvmeBlockDev` io_uring workers,
/// source reads through the router's key resolution. `DestTuple`'s
/// numeric `backend_id` indexes the ordered volume-id table captured at
/// construction (the wire frame stays schema-stable while volume ids
/// are strings).
///
/// Bridging: the seam trait is sync (workers and the host's verify path
/// call it inline, on the wire's own OS threads); this implementation
/// drives the router's async device paths to completion with the
/// first-party `sqz_blocking::block_on` — no runtime capture, no
/// `block_in_place`.
pub struct RouterShardDevice {
    router: Arc<crate::routing::BackendRouter>,
    backend_ids: Vec<String>,
    block_len: usize,
    /// PR VL6a: coordinator-pre-allocated shard destinations are
    /// unpublished BY DESIGN for the whole shard (and quarantined
    /// destinations until job end) — their live-owner registrations in
    /// the fsck in-flight registry live with the seam (dropped when the
    /// job's seam is torn down, alongside quarantine reclaim).
    inflight: parking_lot::Mutex<Vec<crate::block_allocator::InflightAllocGuard>>,
}

impl RouterShardDevice {
    /// Capture the router and its CURRENT volume-id table.
    pub fn new(router: Arc<crate::routing::BackendRouter>, block_len: usize) -> Arc<Self> {
        let mut backend_ids: Vec<String> =
            router.backends.iter().map(|e| e.key().clone()).collect();
        backend_ids.sort();
        Arc::new(Self {
            router,
            backend_ids,
            block_len,
            inflight: parking_lot::Mutex::new(Vec::new()),
        })
    }

    fn id_of(&self, dest: &DestTuple) -> std::io::Result<&str> {
        self.backend_ids
            .get(dest.backend_id as usize)
            .map(|s| s.as_str())
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "destination backend index {} out of table range {}",
                    dest.backend_id,
                    self.backend_ids.len()
                ))
            })
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        sqz_blocking::block_on(fut)
    }
}

impl ShardDeviceSeam for RouterShardDevice {
    fn plan_blocks(&self, job: &JobType) -> usize {
        // Noop plans no device work; the movers never reach the wire in
        // v1.1 (`JobType::wire_executable`), so a mover job type here is
        // a dispatcher bug — planning 0 keeps it inert.
        let _ = job;
        0
    }
    fn block_len(&self) -> usize {
        self.block_len
    }
    fn allocate(&self, n: usize) -> std::io::Result<Vec<DestTuple>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let (be_id, alloc, _dev) = self
                .router
                .get_active_backend()
                .map_err(std::io::Error::other)?;
            let idx = self
                .backend_ids
                .iter()
                .position(|id| *id == be_id)
                .ok_or_else(|| {
                    std::io::Error::other(format!(
                        "backend '{be_id}' joined after the seam table was captured"
                    ))
                })?;
            let offset = self
                .block_on(alloc.allocate_block())
                .map_err(std::io::Error::other)?;
            // VL6a: unpublished-by-design shard destination — registered
            // in-flight for the seam's lifetime.
            self.inflight.lock().push(alloc.inflight_register(offset));
            out.push(DestTuple {
                backend_id: idx as u32,
                offset,
            });
        }
        Ok(out)
    }
    fn read_source(&self, key: &str) -> std::io::Result<Vec<u8>> {
        self.block_on(self.router.read_block(key, self.block_len))
            .map(|b| b.to_vec())
            .map_err(std::io::Error::other)
    }
    fn write_block(&self, dest: &DestTuple, data: &[u8]) -> std::io::Result<()> {
        let be_id = self.id_of(dest)?.to_string();
        let (_alloc, dev) = self
            .router
            .get_backend(&be_id)
            .map_err(std::io::Error::other)?;
        self.block_on(dev.write_block(dest.offset, bytes::Bytes::copy_from_slice(data)))
            .map_err(std::io::Error::other)
    }
    fn read_block(&self, dest: &DestTuple) -> std::io::Result<Vec<u8>> {
        let be_id = self.id_of(dest)?.to_string();
        let (_alloc, dev) = self
            .router
            .get_backend(&be_id)
            .map_err(std::io::Error::other)?;
        self.block_on(dev.read_block(dest.offset, self.block_len))
            .map(|b| b.to_vec())
            .map_err(std::io::Error::other)
    }
    fn quarantine(&self, dests: &[DestTuple], epoch: crate::data_custody::DeadEpoch) -> usize {
        let mut admitted = 0;
        for dest in dests {
            let Ok(be_id) = self.id_of(dest).map(str::to_string) else {
                continue;
            };
            let Ok((alloc, _dev)) = self.router.get_backend(&be_id) else {
                continue;
            };
            admitted += crate::data_custody::quarantine_offsets(&alloc, [dest.offset], epoch);
        }
        admitted
    }
    fn release_quarantine(&self, epoch: crate::data_custody::DeadEpoch) -> usize {
        let mut released = 0;
        for be_id in &self.backend_ids {
            if let Ok((alloc, _dev)) = self.router.get_backend(be_id) {
                released += alloc.release_quarantine(epoch);
            }
        }
        released
    }
}

/// In-memory fake device seam (the `FakeNvmeNamespace` harness-in-the-
/// lib precedent): a monotonic allocator (fresh tuples by construction)
/// over a shared block store, with the allocation ledger exposed so
/// tests can assert the fresh-destination law.
#[derive(Debug)]
pub struct FakeShardDevice {
    blocks_per_shard: usize,
    block_len: usize,
    next_offset: AtomicU64,
    store: parking_lot::Mutex<HashMap<DestTuple, Vec<u8>>>,
    sources: parking_lot::Mutex<HashMap<String, Vec<u8>>>,
    allocs: parking_lot::Mutex<Vec<Vec<DestTuple>>>,
}

impl FakeShardDevice {
    /// A seam whose every shard "moves" `blocks_per_shard` blocks of
    /// `block_len` bytes (0/0 = the pure-Noop shape).
    pub fn new(blocks_per_shard: usize, block_len: usize) -> Arc<Self> {
        Arc::new(Self {
            blocks_per_shard,
            block_len,
            next_offset: AtomicU64::new(0),
            store: parking_lot::Mutex::new(HashMap::new()),
            sources: parking_lot::Mutex::new(HashMap::new()),
            allocs: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// The allocation ledger: one entry per non-empty `allocate` call,
    /// in call order.
    pub fn allocations(&self) -> Vec<Vec<DestTuple>> {
        self.allocs.lock().clone()
    }

    /// Seed one source key for the copy-step shape (`read_source`).
    pub fn seed_source(&self, key: &str, data: Vec<u8>) {
        self.sources.lock().insert(key.to_string(), data);
    }
}

impl ShardDeviceSeam for FakeShardDevice {
    fn plan_blocks(&self, _job: &JobType) -> usize {
        self.blocks_per_shard
    }
    fn block_len(&self) -> usize {
        self.block_len
    }
    fn allocate(&self, n: usize) -> std::io::Result<Vec<DestTuple>> {
        let out: Vec<DestTuple> = (0..n)
            .map(|_| DestTuple {
                backend_id: 0,
                offset: self
                    .next_offset
                    .fetch_add(self.block_len.max(1) as u64, Ordering::SeqCst),
            })
            .collect();
        if !out.is_empty() {
            self.allocs.lock().push(out.clone());
        }
        Ok(out)
    }
    fn read_source(&self, key: &str) -> std::io::Result<Vec<u8>> {
        self.sources
            .lock()
            .get(key)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unseeded source key"))
    }
    fn write_block(&self, dest: &DestTuple, data: &[u8]) -> std::io::Result<()> {
        self.store.lock().insert(dest.clone(), data.to_vec());
        Ok(())
    }
    fn read_block(&self, dest: &DestTuple) -> std::io::Result<Vec<u8>> {
        self.store.lock().get(dest).cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "unwritten destination")
        })
    }
}

// ---------------------------------------------------------------------------
// Enrollment secret + discovery
// ---------------------------------------------------------------------------

/// Read the fabric's enrollment secret from the `job:enroll` record —
/// the storage-membership credential (workers read it through probe
/// backends, exactly like `clients`/`df`).
pub async fn read_enroll_secret(meta: &Arc<RoutedMetaBackend>) -> Result<Vec<u8>> {
    let Some(raw) = meta.getxattr(1, JOB_ENROLL_XATTR).await? else {
        return Err(SqueezefsError::InvalidOperation(
            "no job:enroll record — is a coordinator mounted on this volume set?".into(),
        ));
    };
    let v: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("job:enroll record undecodable: {e}"))
    })?;
    let hexs = v
        .get("secret")
        .and_then(|s| s.as_str())
        .ok_or_else(|| SqueezefsError::InvalidOperation("job:enroll carries no secret".into()))?;
    cluster_wire::hex_decode(hexs)
        .ok_or_else(|| SqueezefsError::InvalidOperation("job:enroll secret is not hex".into()))
}

/// Discover the live coordinator's job endpoint — **DISC-1**, which is
/// [`cluster_wire::discover_endpoint`]: the shared volume IS the
/// rendezvous, so this is an enumeration of the `client:{uuid}` records
/// the mount heartbeat already writes, not a discovery protocol. Kept as a
/// job-wire spelling because the `job worker` verb reads as one line.
///
/// The projection tightened in the S3 port: cluster_wire dedupes by
/// registration id (records are per-volume, so an N-volume set shows the
/// same peer N times) and orders deterministically, where this used to
/// return whichever xattr enumeration reached a fresh endpoint first.
pub async fn discover_endpoint(meta: &Arc<RoutedMetaBackend>) -> Option<String> {
    cluster_wire::discover_endpoint(meta).await
}

// ---------------------------------------------------------------------------
// Coordinator: config, WERO fence, shard state, host
// ---------------------------------------------------------------------------

/// Wire host configuration.
#[derive(Clone)]
pub struct JobWireConfig {
    /// Listener bind address (`0.0.0.0:0` on mounts — ephemeral port,
    /// published via the mount registration).
    pub bind_addr: SocketAddr,
    /// TLS via sync rustls when set (the `ClusterSecurityConfig`
    /// cert/CA/verifier reuse); plaintext otherwise (OQ-A
    /// default-permissive — ONE loud log line at listener start).
    pub security: Option<ClusterSecurityConfig>,
    /// Shared data namespaces the WERO fence covers (rung 2). Empty ⇒
    /// deferred-reclaim guarantee class.
    pub data_device_paths: Vec<PathBuf>,
    /// Shard-lease TTL (default [`LEASE_TTL`]).
    pub lease_ttl: Duration,
    /// Heartbeat cadence handed to workers (default
    /// [`HEARTBEAT_INTERVAL`]).
    pub heartbeat_interval: Duration,
    /// Verify-read sampling in permille on an **authenticated** channel
    /// (CA-pinned mTLS). **Ignored on every other channel class —
    /// plaintext AND CA-less TLS: the Issue-30 law forces 1000.**
    pub verify_sample_permille: u32,
    /// Start the listener at all (VAL-6 gave the posture a switch;
    /// D2 keeps the default ON at `0.0.0.0`).
    pub enabled: bool,
    /// Concurrent-connection cap (VAL-6). Derived from the core count
    /// by [`default_max_connections`]; `SQUEEZEFS_JOB_WIRE_MAX_CONNS`
    /// overrides absolute.
    pub max_connections: usize,
    /// Deadline covering everything an UNAUTHENTICATED peer does: the
    /// TLS handshake, then the challenge/hello exchange including the
    /// hello body.
    pub handshake_timeout: Duration,
    /// Deadline for a post-enrollment frame body once its length prefix
    /// has arrived (a dribbling peer is an error, not a parked task).
    pub frame_body_timeout: Duration,
    /// Idle bound on an enrolled session — no frame at all within this
    /// window closes it. `None` ⇒ derived from the heartbeat cadence.
    pub session_idle_timeout: Option<Duration>,
    /// How long a coordinator-issued challenge nonce stays acceptable
    /// (VAL-6 freshness window). Single use inside it, refused outside.
    pub enroll_freshness: Duration,
}

/// Hand-written so key material never reaches a log line: the security
/// config is reported as its CHANNEL CLASS, never as bytes.
impl std::fmt::Debug for JobWireConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let channel = match self.security.as_ref() {
            None => "plaintext",
            Some(sec) if channel_authenticated(sec) => "mtls(ca-pinned)",
            // Not a class the listener will start (S3 refuses it) — named
            // so a Debug print of a bad config says WHY it will refuse.
            Some(_) => "incomplete-ca(refused at start)",
        };
        f.debug_struct("JobWireConfig")
            .field("bind_addr", &self.bind_addr)
            .field("enabled", &self.enabled)
            .field("channel", &channel)
            .field("verify_sample_permille", &self.verify_sample_permille)
            .field("max_connections", &self.max_connections)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("frame_body_timeout", &self.frame_body_timeout)
            .field("enroll_freshness", &self.enroll_freshness)
            .finish_non_exhaustive()
    }
}

/// The cluster-wide connection-cap derivation
/// ([`cluster_wire::default_max_connections`]) — one function, so a
/// listener's bound cannot depend on which protocol opened it.
pub use crate::cluster_wire::default_max_connections;

impl Default for JobWireConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
            security: None,
            data_device_paths: Vec::new(),
            lease_ttl: LEASE_TTL,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            verify_sample_permille: 1000,
            enabled: true,
            max_connections: default_max_connections(),
            handshake_timeout: Duration::from_secs(10),
            frame_body_timeout: Duration::from_secs(30),
            session_idle_timeout: None,
            enroll_freshness: Duration::from_secs(30),
        }
    }
}

/// Is this security configuration a **confidential, peer-authenticated**
/// channel — i.e. a complete CA pin?
///
/// CA-pinned mTLS only: the server installs a `WebPkiClientVerifier`
/// rooted at the CA and the client validates against the same root and
/// presents its own CA-signed cert. Anything less used to mean an
/// accept-everything verifier; since S3 it means the listener refuses
/// (`cluster_wire::tls_acceptor`), so this predicate now separates the two
/// postures that EXIST rather than ranking three.
///
/// The key half is load-bearing, not decorative: the node cert the
/// cluster machinery presents is signed by the CA key, so a CA cert
/// without its key cannot produce an authenticated channel at all — which
/// is why `ClusterSecurityConfig::ca_pair` is the check.
pub fn channel_authenticated(security: &ClusterSecurityConfig) -> bool {
    security.ca_pair().is_some()
}

impl JobWireConfig {
    /// The operator surface (VAL-6): `security` used to be hardwired
    /// `None` with nothing able to populate it, so the listener's own
    /// warning recommended a configuration the binary could not express.
    ///
    /// | Variable | Effect |
    /// |---|---|
    /// | `SQUEEZEFS_JOB_WIRE_BIND` | `addr:port`, or `off`/`none`/`disabled` to not listen. Default `0.0.0.0:0` (ruling D2 — configurable, default open, ephemeral port published via the mount registration) |
    /// | `SQUEEZEFS_JOB_WIRE_CA_CERT` | Cluster CA certificate (PEM or DER). Derived from the key when unset |
    /// | `SQUEEZEFS_JOB_WIRE_CA_KEY` | Cluster CA private key (PEM or DER). **Required** for a CA-pinned (authenticated) channel |
    /// | `SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE` | Verify-read sampling ‰ (1..=1000) — honored only on an authenticated channel |
    /// | `SQUEEZEFS_JOB_WIRE_MAX_CONNS` | Absolute concurrent-connection cap (absolute > derived) |
    /// | `SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS` | Challenge freshness window |
    ///
    /// Unset ⇒ today's behavior verbatim (open listener, plaintext,
    /// mandatory-100 % verify-reads). Every malformed value refuses
    /// loud naming its own knob — a security knob must never fail open.
    pub fn from_env(data_device_paths: Vec<PathBuf>) -> Result<Self> {
        let mut cfg = Self {
            // D2: the mount posture is the wide bind on an ephemeral
            // port; `JobWireConfig::default()`'s loopback is the library
            // default (tests, offline coordinators).
            bind_addr: "0.0.0.0:0".parse().expect("literal addr"),
            data_device_paths,
            ..Default::default()
        };

        if let Some(raw) = env_str("SQUEEZEFS_JOB_WIRE_BIND") {
            match raw.to_ascii_lowercase().as_str() {
                "off" | "none" | "disabled" | "0" => cfg.enabled = false,
                _ => {
                    cfg.bind_addr = raw.parse().map_err(|e| {
                        SqueezefsError::InvalidOperation(format!(
                            "SQUEEZEFS_JOB_WIRE_BIND='{raw}' is not an addr:port ({e}) — \
                             refusing rather than falling back to the open default"
                        ))
                    })?
                }
            }
        }

        let ca_cert_path = env_str("SQUEEZEFS_JOB_WIRE_CA_CERT");
        let ca_key_path = env_str("SQUEEZEFS_JOB_WIRE_CA_KEY");
        match (ca_cert_path, ca_key_path) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(SqueezefsError::InvalidOperation(
                    "SQUEEZEFS_JOB_WIRE_CA_CERT is set without SQUEEZEFS_JOB_WIRE_CA_KEY — \
                     the cluster machinery signs this node's certificate with the CA key, \
                     so a cert alone cannot make an authenticated channel"
                        .into(),
                ))
            }
            (cert, Some(key_path)) => {
                let ca_key = load_der(&key_path, "SQUEEZEFS_JOB_WIRE_CA_KEY")?;
                let ca_cert = match cert {
                    Some(p) => load_der(&p, "SQUEEZEFS_JOB_WIRE_CA_CERT")?,
                    // Derived from the key by the same construction the
                    // cluster machinery uses, so every node holding the
                    // key pins the identical root.
                    None => derive_ca_cert(&ca_key)?,
                };
                cfg.security = Some(ClusterSecurityConfig {
                    ca_cert: Some(ca_cert),
                    ca_key: Some(ca_key),
                });
            }
        }

        if let Some(raw) = env_str("SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE") {
            let v: u32 = raw.parse().map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE='{raw}' is not a number ({e})"
                ))
            })?;
            if v == 0 || v > 1000 {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE={v} out of range 1..=1000"
                )));
            }
            cfg.verify_sample_permille = v;
        }
        if let Some(raw) = env_str("SQUEEZEFS_JOB_WIRE_MAX_CONNS") {
            let v: usize = raw.parse().map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "SQUEEZEFS_JOB_WIRE_MAX_CONNS='{raw}' is not a number ({e})"
                ))
            })?;
            if v == 0 {
                return Err(SqueezefsError::InvalidOperation(
                    "SQUEEZEFS_JOB_WIRE_MAX_CONNS=0 would refuse every worker — use \
                     SQUEEZEFS_JOB_WIRE_BIND=off to disable the listener"
                        .into(),
                ));
            }
            cfg.max_connections = v;
        }
        if let Some(raw) = env_str("SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS") {
            let v: u64 = raw.parse().map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS='{raw}' is not a number ({e})"
                ))
            })?;
            if v == 0 {
                return Err(SqueezefsError::InvalidOperation(
                    "SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS=0 would expire every challenge \
                     before it could be answered"
                        .into(),
                ));
            }
            cfg.enroll_freshness = Duration::from_millis(v);
        }
        Ok(cfg)
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Load a PEM or DER file as DER bytes (`ClusterSecurityConfig` holds
/// DER). Operators have both shapes; refusing one of them for no reason
/// is how a security knob ends up unused.
fn load_der(path: &str, knob: &str) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("{knob}='{path}' is unreadable: {e}"))
    })?;
    if raw.starts_with(b"-----BEGIN") {
        return pem_to_der(&raw).ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!("{knob}='{path}' is malformed PEM"))
        });
    }
    if raw.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "{knob}='{path}' is empty"
        )));
    }
    Ok(raw)
}

/// Minimal PEM body decoder (first block only — a CA file carries one).
fn pem_to_der(raw: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            inside = true;
        } else if line.starts_with("-----END") {
            break;
        } else if inside {
            body.push_str(line);
        }
    }
    if body.is_empty() {
        return None;
    }
    base64_decode(&body)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let pad = quad.iter().filter(|b| **b == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut acc = 0u32;
        for b in quad {
            acc = (acc << 6) | if *b == b'=' { 0 } else { val(*b)? };
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Reconstruct the cluster CA certificate from its key, by the same
/// construction `tiering::dht` uses when it signs this node's cert —
/// so a key-only configuration pins exactly the root the handshake
/// will present.
fn derive_ca_cert(ca_key_der: &[u8]) -> Result<Vec<u8>> {
    let key = rcgen::KeyPair::try_from(ca_key_der.to_vec()).map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "SQUEEZEFS_JOB_WIRE_CA_KEY is not a usable private key: {e}"
        ))
    })?;
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "SqueezeFS Cluster CA");
    let cert = params.self_signed(&key).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("deriving the cluster CA certificate failed: {e}"))
    })?;
    Ok(cert.der().to_vec())
}

/// One enrolled worker session. Its writer is **authenticated**: every
/// frame the coordinator sends after enrollment carries the session MAC
/// (S3), so the shard descriptors and acks a worker acts on cannot be
/// forged or reordered by anything on the path.
struct Session {
    id: u64,
    worker_id: String,
    pr_key: Option<u64>,
    /// KD-MW-16 capability mask from the Enroll frame (routing only).
    caps: u32,
    writer: Arc<parking_lot::Mutex<AuthedWriter>>,
    /// Holds an assigned shard right now.
    busy: AtomicBool,
    /// Lease expired while holding a shard — never assignable again
    /// (the connection stays open so a late ResultSubmit can be
    /// REFUSED, not ignored).
    expired: AtomicBool,
}

/// The current lease holder of a shard.
struct ShardHolder {
    session_id: u64,
    worker_id: String,
    lease_expiry: Instant,
}

/// Coordinator-side shard state (persisted as `job:{id}:shard:{k}` on
/// the KD-2 record plane; this is the live truth between checkpoints).
struct ShardState {
    job_id: String,
    shard: u32,
    /// The fabric control block — `Some` for whole-job shards (the
    /// requeue path needs it); `None` for fleet read shards, whose
    /// re-lease belongs to the dispatching executor via `outcome`.
    ctl: Option<Arc<JobCtl>>,
    fencing: AtomicU64,
    holder: parking_lot::Mutex<Option<ShardHolder>>,
    destinations: parking_lot::Mutex<Vec<DestTuple>>,
    done: AtomicBool,
    /// KD-MW-16: `Some((k, n))` = fleet READ shard (no destinations, no
    /// quarantine, no PR preempt on expiry — design-mw-fleet-jobs §5).
    fleet: Option<(u32, u32)>,
    /// The dispatching executor's outcome channel (fleet shards only).
    outcome: parking_lot::Mutex<Option<crate::jobs::FleetOutcomeTx>>,
}

/// The authenticated write half of a session: the stream half plus its
/// [`FrameTx`] sequence. One object, so no call site can accidentally
/// write a session frame WITHOUT its MAC.
struct AuthedWriter {
    half: ClusterWriteHalf,
    tx: FrameTx,
}

impl AuthedWriter {
    fn send(&mut self, frame: &WireFrame) -> std::io::Result<()> {
        self.tx.send(&mut self.half, SESSION_CLASS, frame)
    }
}

/// The §5.1.6 wire host: TCP(/TLS) listener + dispatcher + lease
/// sweeper on the job-fabric coordinator.
pub struct JobWireHost {
    fabric: Arc<JobFabric>,
    seam: Arc<dyn ShardDeviceSeam>,
    cfg: JobWireConfig,
    endpoint: SocketAddr,
    /// The ONE authn ladder ([`AuthnGate`]): the storage secret, the
    /// server-issued single-use challenge registry, its freshness window,
    /// and the session-key derivation — all of it cluster_wire's, so this
    /// module cannot drift from the RPC surface S4 adds beside it.
    gate: Arc<AuthnGate>,
    transport: &'static str,
    /// CA-pinned mTLS — the predicate the verification-strength ladder
    /// keys on (VAL-6), never `transport == "tls"`.
    authenticated: bool,
    verify_permille: u32,
    tls: Option<Arc<rustls::ServerConfig>>,
    /// `false` when the posture disabled the listener entirely.
    listening: bool,
    /// Resolved idle bound for an enrolled session.
    session_idle: Duration,
    /// Connection accounting: cluster_wire's [`ConnGate`] (the cap gauge),
    /// its refusals, and the accept-error backoff engagement counter
    /// (must stay 0 on a healthy host).
    conns: ConnGate,
    conns_refused: AtomicU64,
    accept_backoffs: AtomicU64,
    next_session: AtomicU64,
    sessions: parking_lot::Mutex<HashMap<u64, Arc<Session>>>,
    /// Live shard state, keyed `(job_id, shard_no)` — whole jobs ride
    /// shard 0; fleet read shards ride `1..n` (KD-MW-16).
    shards: parking_lot::Mutex<HashMap<(String, u32), Arc<ShardState>>>,
    quarantine: parking_lot::Mutex<BTreeSet<DestTuple>>,
    /// WERO fence, held first-enrollment → last-departure. The async
    /// mutex serializes acquire/release transitions.
    fence: SqzMutex<Option<WeroHold>>,
    /// Guarantee class: true = `pr` (WERO held on EVERY configured data
    /// namespace).
    pr_mode: AtomicBool,
    shutdown: AtomicBool,
    /// Thread registry: the accept/dispatcher/sweeper cores plus one
    /// entry per live connection. **Self-draining** (the 2026-08-04
    /// quiet-host law): a serve thread's own completion removes its entry
    /// ([`HandleReaper`]), so finished handles are released the moment
    /// their thread ends — never parked until a later accept happens to
    /// run a prune (the RES-5 retain-on-accept left a quiet host's last
    /// arrivals retained until shutdown, which is exactly the schedule
    /// the `task check` flake kept losing to).
    handles: parking_lot::Mutex<HashMap<u64, std::thread::JoinHandle<()>>>,
    /// Dup'd per-connection sockets: shutdown's nudge — `shutdown(Both)`
    /// wakes a serve thread parked in a socket read (threads cannot be
    /// aborted). Same id space and reaper as `handles`.
    conn_socks: parking_lot::Mutex<HashMap<u64, std::net::TcpStream>>,
    next_handle_id: AtomicU64,
}

/// Removes one serve thread's registry entries when the thread ENDS — on
/// every exit: normal return, panic unwind, or the shutdown nudge (where
/// [`JobWireHost::shutdown`] already took the maps, making the removes
/// no-ops). Held as the serve thread's first local, so the drop runs
/// unconditionally. This is what makes
/// [`JobWireHost::retained_task_handles`] converge on a quiet host: the
/// completion itself is the prune trigger.
struct HandleReaper {
    host: Arc<JobWireHost>,
    id: u64,
}

impl Drop for HandleReaper {
    fn drop(&mut self) {
        self.host.handles.lock().remove(&self.id);
        self.host.conn_socks.lock().remove(&self.id);
    }
}

impl std::fmt::Debug for JobWireHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobWireHost")
            .field("endpoint", &self.endpoint)
            .field("listening", &self.listening)
            .field("transport", &self.transport)
            .field("authenticated", &self.authenticated)
            .field("verify_permille", &self.verify_permille)
            .field("live_connections", &self.live_connections())
            .field("connections_refused", &self.connections_refused())
            .finish_non_exhaustive()
    }
}

impl JobWireHost {
    /// Start the wire: write the `job:enroll` secret, bind the
    /// listener (TLS when configured, plaintext otherwise — loud), and
    /// spawn the accept loop, dispatcher, and lease sweeper.
    pub async fn start(
        fabric: Arc<JobFabric>,
        cfg: JobWireConfig,
        seam: Arc<dyn ShardDeviceSeam>,
    ) -> Result<Arc<Self>> {
        // The session secret: 32 random bytes, durable on ino 1 behind
        // the reserved-namespace screen. The record is the SET's root of
        // trust (ruling D2: possession of volume access IS membership) —
        // it is minted ONCE and REUSED by every later coordinator: a
        // member outlives a coordinator's incarnation (symmetric PR 12b's
        // joined writers, an S9 co-writer, an S6 member reader), and every
        // session it dials the successor with proves the secret it read
        // at its own arm. Rotating it per mount made every reclaim after a
        // manager failover `mac invalid` for the member's life (the
        // sym-crash fleet leg: the joiners parked at `T_self` against a
        // successor whose grace window would have admitted them).
        //
        // Three outcomes, never conflated (PR 12b review round 1, Issue
        // 3): ABSENT ⇒ mint (the first coordinator of the set); a record
        // PRESENT but of another length or undecodable ⇒ REFUSE loud
        // naming it (a reserved-xattr corruption is never overwritten
        // silently — the operator's rotation lever is REMOVING the record,
        // `docs/operations.md`); an I/O error ⇒ PROPAGATE — re-minting on
        // a transient read failure would bring the rotation, and the `mac
        // invalid` class it produced, back on exactly the bad day.
        let secret = match fabric.meta_handle().getxattr(1, JOB_ENROLL_XATTR).await? {
            None => {
                let mut fresh = vec![0u8; 32];
                rand::Rng::fill(&mut rand::thread_rng(), &mut fresh[..]);
                let record =
                    serde_json::json!({ "schema": 1, "secret": cluster_wire::hex_encode(&fresh) });
                fabric
                    .meta_handle()
                    .setxattr(1, JOB_ENROLL_XATTR, record.to_string().as_bytes())
                    .await?;
                fresh
            }
            Some(_) => {
                let existing = read_enroll_secret(fabric.meta_handle()).await?;
                if existing.len() != 32 {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "the set's `{JOB_ENROLL_XATTR}` record carries a {}-byte secret (32 \
                         expected) — a corrupt reserved xattr is never overwritten silently; \
                         remove the record on a quiesced set to mint a fresh secret (every \
                         member re-enrolls at its next arm)",
                        existing.len()
                    )));
                }
                existing
            }
        };

        // The channel class. S3: a TLS configuration that is not a
        // complete CA pair is REFUSED here (`cluster_wire::tls_acceptor`),
        // naming the missing half — it used to install an
        // accept-everything verifier client-side and take no client auth
        // server-side, which VAL-6 then had to special-case out of the
        // ladder. There is no "tls-unauthenticated" class any more.
        let (tls, channel) = match cfg.security.as_ref() {
            Some(sec) => (
                Some(cluster_wire::tls_acceptor(sec)?),
                ChannelClass::MutualTls,
            ),
            None => (None, ChannelClass::Plaintext),
        };
        // Every admitted session on this wire is authenticated (storage
        // proof + per-frame MAC). The Issue-30 verify-read ladder needs
        // more than authentication — it needs CONFIDENTIALITY, because
        // sampling trades reads for trust in what crossed the wire — so
        // its predicate is `verify_sampling_admissible()`, i.e. mTLS.
        let authn = SessionAuthn {
            channel,
            proof_verified: true,
            mac_engaged: true,
        };
        let transport = channel.name();
        let authenticated = authn.verify_sampling_admissible();
        let verify_permille = if authenticated {
            cfg.verify_sample_permille.clamp(1, 1000)
        } else {
            1000
        };

        let (listener, endpoint) = if cfg.enabled {
            let l = std::net::TcpListener::bind(cfg.bind_addr)?;
            // Non-blocking + the accept poll tick is how the accept
            // thread observes the shutdown latch (threads cannot be
            // aborted; keep it simple and loud).
            l.set_nonblocking(true)?;
            let ep = l.local_addr()?;
            (Some(l), ep)
        } else {
            log::info!(
                "job wire: listener DISABLED by configuration — this mount serves no remote \
                 job shards (local pool only)"
            );
            (None, cfg.bind_addr)
        };
        if listener.is_some() {
            match transport {
                // OQ-A default-permissive: the ONE loud line. Since S3
                // the session MAC means a plaintext session cannot be
                // hijacked or forged either — what plaintext still lacks
                // is confidentiality, and the Issue-30 law keeps every
                // mutating publish 100 % verify-read there.
                "plaintext" => log::warn!(
                    "job wire: listener {endpoint} is PLAINTEXT TCP (no ClusterSecurityConfig) \
                     — sessions are AUTHENTICATED (storage-trust challenge proof + per-frame \
                     session MAC, S3 cluster_wire) but not private, so mutating publishes pay \
                     mandatory-100 % verify-reads (Issue-30; ≈2× device reads on \
                     remote-mutated bytes). Set SQUEEZEFS_JOB_WIRE_CA_KEY (+ optional \
                     SQUEEZEFS_JOB_WIRE_CA_CERT) for CA-pinned mTLS + sampled verification, \
                     or SQUEEZEFS_JOB_WIRE_BIND to narrow/disable the listener."
                ),
                _ => {
                    log::info!(
                        "job wire: listener {endpoint} (CA-pinned mTLS via ClusterSecurityConfig)"
                    );
                    if verify_permille < 1000 {
                        log::info!(
                            "job wire: verify-read sampling {verify_permille}‰ (sanctioned on \
                             an authenticated channel)"
                        );
                    }
                }
            }
            log::info!(
                "job wire: bounds — max {} concurrent connections, {} pre-enrollment frame \
                 cap, handshake deadline {:?}, challenge freshness {:?}",
                cfg.max_connections,
                MAX_HELLO_FRAME_BYTES,
                cfg.handshake_timeout,
                cfg.enroll_freshness
            );
        }

        let session_idle = cfg
            .session_idle_timeout
            .unwrap_or_else(|| (cfg.heartbeat_interval * 4).max(Duration::from_secs(60)));
        // One outstanding challenge per admitted connection, plus slack
        // for reconnect churn inside one freshness window.
        let nonce_cap = cfg.max_connections.saturating_mul(4).max(64);

        let host = Arc::new(Self {
            fabric,
            seam,
            endpoint,
            gate: Arc::new(AuthnGate::new(
                secret,
                AuthnConfig {
                    freshness: cfg.enroll_freshness,
                    // One outstanding challenge per admitted connection,
                    // plus slack for reconnect churn inside one window.
                    nonce_cap,
                    schema: WIRE_SCHEMA,
                    ..AuthnConfig::default()
                },
            )),
            transport,
            authenticated,
            verify_permille,
            tls,
            listening: listener.is_some(),
            session_idle,
            conns: ConnGate::new(cfg.max_connections),
            conns_refused: AtomicU64::new(0),
            accept_backoffs: AtomicU64::new(0),
            next_session: AtomicU64::new(1),
            sessions: parking_lot::Mutex::new(HashMap::new()),
            shards: parking_lot::Mutex::new(HashMap::new()),
            quarantine: parking_lot::Mutex::new(BTreeSet::new()),
            fence: SqzMutex::new(None),
            pr_mode: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            handles: parking_lot::Mutex::new(HashMap::new()),
            conn_socks: parking_lot::Mutex::new(HashMap::new()),
            next_handle_id: AtomicU64::new(0),
            cfg,
        });

        // The core loops run on named OS threads (pidstat/perf
        // attribution — the fuse3-tpcN lesson). The dispatcher and
        // sweeper bodies stay async (fabric/meta awaits) and are driven
        // by the first-party block_on on their own threads.
        let spawn_core =
            |name: &str, f: Box<dyn FnOnce() + Send>| -> Result<std::thread::JoinHandle<()>> {
                std::thread::Builder::new()
                    .name(name.to_string())
                    .spawn(f)
                    .map_err(|e| {
                        SqueezefsError::InvalidOperation(format!(
                            "job wire: {name} thread refused: {e}"
                        ))
                    })
            };
        let mut core = Vec::with_capacity(3);
        if let Some(listener) = listener {
            let h = Arc::clone(&host);
            core.push(spawn_core(
                "sqz-jw-accept",
                Box::new(move || h.accept_loop(listener)),
            )?);
        }
        let h = Arc::clone(&host);
        core.push(spawn_core(
            "sqz-jw-dispatch",
            Box::new(move || sqz_blocking::block_on(h.dispatcher_loop())),
        )?);
        let h = Arc::clone(&host);
        core.push(spawn_core(
            "sqz-jw-sweep",
            Box::new(move || sqz_blocking::block_on(h.sweeper_loop())),
        )?);
        {
            let mut handles = host.handles.lock();
            for h in core {
                let id = host.next_handle_id.fetch_add(1, Ordering::Relaxed);
                handles.insert(id, h);
            }
        }
        // KD-MW-16: register the fleet read-shard dispatch seam on the
        // fabric (Weak — the host holds the fabric, so a strong edge
        // would be a leak-grade Arc cycle). Registered even when the
        // listener posture is off: capacity is simply 0 there, and the
        // fan-out's zero-capacity arm IS the local run.
        host.fabric.set_fleet_dispatch(
            Arc::downgrade(&host) as std::sync::Weak<dyn crate::jobs::FleetDispatch>
        );
        Ok(host)
    }

    /// The bound listener address.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// The endpoint this wire PUBLISHES (the mount registration's
    /// `job_wire_endpoint`): THE endpoint law — an explicit
    /// `SQUEEZEFS_JOB_WIRE_BIND` IP verbatim, the route-derived IP for the
    /// unspecified address, the bound port either way. Before it the
    /// registration named `local_advertise_ip()` whatever the bind said
    /// (the class PR 12's review found on the S9 arms).
    pub fn advertised_endpoint(&self) -> String {
        cluster_wire::advertised_endpoint(self.cfg.bind_addr, self.endpoint.port())
    }

    /// `"plaintext"` or `"mtls"` — the only two classes that exist since
    /// S3. `"tls-unauthenticated"` (a TLS object with no CA pin, which
    /// VAL-6 had to carve out of the ladder) is gone: that configuration
    /// refuses the listener, because the verifier it depended on no longer
    /// exists in the tree.
    pub fn transport_mode(&self) -> &'static str {
        self.transport
    }

    /// The verification-strength ladder's predicate: an authenticated
    /// **and confidential** channel, i.e. CA-pinned mTLS — never the mere
    /// presence of a TLS object (VAL-6), and never `true` for plaintext.
    ///
    /// S3 nuance worth keeping straight: since the session MAC landed,
    /// EVERY admitted session is authenticated (`SessionAuthn`), plaintext
    /// included. What sampling needs on top of that is confidentiality,
    /// which is what this reports.
    pub fn channel_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Whether the listener was started at all.
    pub fn listening(&self) -> bool {
        self.listening
    }

    /// Live accepted connections (the [`JobWireConfig::max_connections`]
    /// gauge).
    pub fn live_connections(&self) -> u64 {
        self.conns.live()
    }

    /// Connections closed unserved because the cap was full.
    pub fn connections_refused(&self) -> u64 {
        self.conns_refused.load(Ordering::SeqCst)
    }

    /// Accept-error backoff engagement — **0 on a healthy host**;
    /// sustained growth means fd exhaustion or a listener fault.
    pub fn accept_backoffs(&self) -> u64 {
        self.accept_backoffs.load(Ordering::SeqCst)
    }

    /// Retained task handles (RES-5 gauge: this must NOT grow with the
    /// number of connections ever accepted — and, since the 2026-08-04
    /// quiet-host law, it converges back to the core-task base as serve
    /// tasks finish, with no further accept required: the registry is
    /// self-draining via `HandleReaper`).
    pub fn retained_task_handles(&self) -> usize {
        self.handles.lock().len()
    }

    /// Outstanding (issued, unanswered, unexpired) enrollment challenges.
    pub fn outstanding_challenges(&self) -> usize {
        self.gate.outstanding_challenges()
    }

    /// Effective verify-read sampling (‰). **1000 on every channel that
    /// is not CA-pinned mTLS, by the Issue-30 law, whatever was
    /// configured.**
    pub fn verify_permille(&self) -> u32 {
        self.verify_permille
    }

    /// Guarantee class: `"pr"` while the WERO fence is held on every
    /// configured data namespace, else `"deferred-reclaim"`.
    pub fn fence_mode(&self) -> &'static str {
        if self.pr_mode.load(Ordering::SeqCst) {
            "pr"
        } else {
            "deferred-reclaim"
        }
    }

    /// Snapshot of the do-not-publish quarantine set (test/ops
    /// observation; the gauge is `job_remote_quarantined_destinations`).
    pub fn quarantined_destinations(&self) -> Vec<DestTuple> {
        self.quarantine.lock().iter().cloned().collect()
    }

    /// Stop the wire: latch the shutdown flag, nudge every live
    /// connection's socket (threads cannot be aborted — `shutdown(Both)`
    /// wakes any parked read; the accept/dispatcher/sweeper cores exit on
    /// their poll ticks), join the threads, drop every session, and
    /// release the WERO fence.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Take both maps FIRST: every woken serve thread's reaper then
        // removes against the fresh (empty) maps — a no-op — while this
        // call owns nudging the sockets and joining the taken handles.
        let socks = std::mem::take(&mut *self.conn_socks.lock());
        for sock in socks.into_values() {
            let _ = sock.shutdown(Shutdown::Both);
        }
        let handles = std::mem::take(&mut *self.handles.lock());
        // Joining OS threads blocks; hop through the blocking pool so an
        // async caller's executor thread is never parked on it.
        sqz_blocking::run_blocking(move || {
            for h in handles.into_values() {
                let _ = h.join();
            }
        })
        .await;
        // The socket nudge killed both halves at the transport; drop the
        // session map's write halves too so nothing retains the fds and
        // remote workers observe EOF.
        self.sessions.lock().clear();
        self.release_fence().await;
    }

    // -- transport plumbing --------------------------------------------------

    fn accept_loop(self: Arc<Self>, listener: std::net::TcpListener) {
        let mut backoff: Option<Duration> = None;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let (tcp, peer) = match listener.accept() {
                Ok(x) => {
                    backoff = None;
                    x
                }
                Err(e) if cluster_wire::io_timed_out(&e) => {
                    // Non-blocking listener: the poll tick is the
                    // shutdown-latch observation cadence.
                    std::thread::sleep(cluster_wire::ACCEPT_POLL_TICK);
                    continue;
                }
                Err(e) => {
                    // VAL-6: the old arm `continue`d, so a persistent
                    // EMFILE/ENFILE condition spun the accept loop at
                    // 100 % of a core. Back off, capped, and say so.
                    let d = next_accept_backoff(backoff);
                    backoff = Some(d);
                    self.accept_backoffs.fetch_add(1, Ordering::SeqCst);
                    log::warn!("job wire: accept failed: {e} — backing off {d:?}");
                    std::thread::sleep(d);
                    continue;
                }
            };

            // The concurrent-connection cap (VAL-6). Claim the slot
            // BEFORE spawning anything: over-cap peers cost one accept
            // and one close, never a thread or a buffer.
            let permit = match self.conns.try_admit() {
                Some(p) => p,
                None => {
                    self.conns_refused.fetch_add(1, Ordering::SeqCst);
                    log::warn!(
                        "job wire: refusing {peer} — {} concurrent connections is the cap \
                         (SQUEEZEFS_JOB_WIRE_MAX_CONNS)",
                        self.cfg.max_connections
                    );
                    drop(tcp);
                    continue;
                }
            };
            if let Err(e) = tcp.set_nonblocking(false) {
                log::warn!("job wire: dropping {peer} — set_nonblocking(false) failed: {e}");
                continue;
            }

            let host = Arc::clone(&self);
            let handshake = self.cfg.handshake_timeout;
            // Self-draining registry (RES-5 + the 2026-08-04 quiet-host
            // law): the serve thread's own completion removes its entry,
            // so `retained_task_handles` converges without a further
            // accept. The lock is held across spawn+insert, so the
            // reaper's remove can never run before the insert it undoes.
            let id = self.next_handle_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(nudge) = tcp.try_clone() {
                self.conn_socks.lock().insert(id, nudge);
            }
            let mut handles = self.handles.lock();
            let spawned = std::thread::Builder::new()
                .name("sqz-jw-conn".to_string())
                .spawn(move || {
                    let _reaper = HandleReaper {
                        host: Arc::clone(&host),
                        id,
                    };
                    let _permit = permit;
                    // The handshake itself is attacker-paced: bound every
                    // pre-enrollment syscall at the socket. The TLS
                    // **exporter** output is captured here and mixed into
                    // the session key, so an mTLS session's per-frame MAC
                    // is channel-bound (a key cannot be lifted onto
                    // another connection).
                    if tcp.set_read_timeout(Some(handshake)).is_err()
                        || tcp.set_write_timeout(Some(handshake)).is_err()
                    {
                        log::warn!("job wire: {peer}: socket timeout setup failed — dropped");
                        return;
                    }
                    let (stream, binding): (ClusterStream, Option<[u8; 32]>) =
                        match host.tls.clone() {
                            Some(cfg) => match cluster_wire::tls_server_handshake(cfg, tcp) {
                                Ok(s) => {
                                    let binding = cluster_wire::server_exporter(&s.conn);
                                    (ClusterStream::tls_server(s), binding)
                                }
                                Err(e) => {
                                    log::warn!("job wire: TLS handshake with {peer} failed: {e}");
                                    return;
                                }
                            },
                            None => (ClusterStream::tcp(tcp), None),
                        };
                    // The session body stays async (fabric/meta awaits);
                    // this connection's own thread drives it.
                    sqz_blocking::block_on(host.serve_conn(stream, peer, binding));
                });
            match spawned {
                Ok(h) => {
                    handles.insert(id, h);
                }
                Err(e) => {
                    self.conn_socks.lock().remove(&id);
                    log::warn!("job wire: dropping {peer} — serve thread refused: {e}");
                }
            }
            drop(handles);
        }
    }

    /// One connection: the coordinator-issued challenge, the enrollment
    /// gate, then the **authenticated** session frame loop. Everything
    /// before enrollment runs under `handshake_timeout` at the
    /// [`MAX_HELLO_FRAME_BYTES`] class; everything after it carries the
    /// session MAC.
    async fn serve_conn(
        self: &Arc<Self>,
        stream: ClusterStream,
        peer: SocketAddr,
        binding: Option<[u8; 32]>,
    ) {
        let mut stream = stream;
        let deadline = self.cfg.handshake_timeout;

        // The coordinator speaks first: a single-use, freshness-windowed
        // nonce the worker cannot choose. A captured hello is therefore
        // not a credential.
        let issued = self.gate.issue_challenge();
        let challenge = WireFrame::Challenge {
            wire_schema: issued.schema,
            server_nonce: issued.server_nonce.clone(),
            freshness_ms: issued.freshness_ms,
        };
        // The socket read/write timeouts were set to the handshake
        // deadline before the TLS handshake; the challenge write and the
        // hello read below ride the same bound.
        if let Err(e) = write_frame(&mut stream, &challenge) {
            log::warn!("job wire: {peer}: challenge write failed: {e}");
            return;
        }

        let hello = match read_frame_limited(&mut stream, MAX_HELLO_FRAME_BYTES, Some(deadline)) {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) if cluster_wire::io_timed_out(&e) => {
                log::warn!("job wire: {peer}: no hello within {deadline:?} — dropped");
                return;
            }
            Err(e) => {
                log::warn!("job wire: {peer}: undecodable hello: {e}");
                METRICS
                    .job_remote_enroll_refused
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let WireFrame::Enroll {
            wire_schema,
            worker_id,
            server_nonce: presented_nonce,
            endpoint_nonce,
            hmac,
            pr_key,
            caps,
        } = hello
        else {
            METRICS
                .job_remote_enroll_refused
                .fetch_add(1, Ordering::Relaxed);
            let _ = write_frame(
                &mut stream,
                &WireFrame::EnrollRefused {
                    reason: "expected Enroll".into(),
                },
            );
            return;
        };
        // ONE ladder, cluster_wire's: schema, then the identity bound,
        // then the constant-time proof compare, and only then the
        // single-use/freshness nonce consume — so a peer that cannot
        // produce a valid proof can never spend the challenge of the
        // honest connection it raced. On success both ends hold the same
        // session key (derived from the storage secret, both nonces, the
        // worker id and — under mTLS — the channel binding).
        let key = match self.gate.verify(
            ProofClaim {
                schema: wire_schema,
                peer_id: &worker_id,
                server_nonce: &presented_nonce,
                peer_nonce: &endpoint_nonce,
                mac: &hmac,
            },
            binding.as_ref().map(|b| &b[..]),
        ) {
            Verdict::Admit(key) => key,
            Verdict::Refuse(reason) => {
                METRICS
                    .job_remote_enroll_refused
                    .fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "job wire: {peer}: worker {worker_id} refused: {reason} \
                     (job_remote_enroll_refused)"
                );
                let _ = write_frame(&mut stream, &WireFrame::EnrollRefused { reason });
                return;
            }
        };

        // Admitted.
        METRICS
            .job_remote_enrollments
            .fetch_add(1, Ordering::Relaxed);
        METRICS.job_remote_workers.fetch_add(1, Ordering::Relaxed);
        self.on_first_enrollment().await;

        if let Err(e) = write_frame(
            &mut stream,
            &WireFrame::EnrollOk {
                wire_schema: WIRE_SCHEMA,
                heartbeat_ms: self.cfg.heartbeat_interval.as_millis() as u64,
                lease_ttl_ms: self.cfg.lease_ttl.as_millis() as u64,
            },
        ) {
            log::warn!("job wire: {peer}: EnrollOk write failed: {e}");
            self.on_worker_departed(None).await;
            return;
        }

        // Session posture: the idle bound rides the socket read timeout
        // (the prefix wait), writes are bounded by the body deadline.
        let _ = stream.set_read_timeout(Some(self.session_idle));
        let _ = stream.set_write_timeout(Some(self.cfg.frame_body_timeout));

        // EnrollOk was the last UNAUTHENTICATED frame on this
        // connection: from here both directions carry the session MAC.
        let (mut rd, wr) = match stream.split() {
            Ok(halves) => halves,
            Err(e) => {
                log::warn!("job wire: {peer}: session split failed: {e}");
                self.on_worker_departed(None).await;
                return;
            }
        };
        // Re-assert the idle bound on the READ half: a split TLS reader
        // enforces its bound across mutex-sliced polls, not at the
        // socket.
        let _ = rd.set_read_timeout(Some(self.session_idle));
        let (tx, mut rx) = session_framers(&key, cluster_wire::Role::Coordinator);
        let session = Arc::new(Session {
            id: self.next_session.fetch_add(1, Ordering::SeqCst),
            worker_id: worker_id.clone(),
            pr_key,
            caps,
            writer: Arc::new(parking_lot::Mutex::new(AuthedWriter { half: wr, tx })),
            busy: AtomicBool::new(false),
            expired: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .insert(session.id, Arc::clone(&session));
        log::info!(
            "job wire: worker {worker_id} enrolled from {peer} (session {}, pr_key {:?}, \
             fence {}, channel {}{})",
            session.id,
            pr_key,
            self.fence_mode(),
            self.transport,
            if binding.is_some() {
                ", exporter-bound session MAC"
            } else {
                ", session MAC"
            }
        );

        // Session frame loop. Post-enrollment classes: the full frame
        // cap, a body deadline once a length prefix lands, an idle bound
        // (a session that stops speaking entirely is closed — its slot is
        // a capped resource), and the **session MAC** on every frame: a
        // shard result is custody-bearing, so an unauthenticated frame in
        // this position ends the session rather than being interpreted.
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let frame = match rx.recv::<_, WireFrame>(
                &mut rd,
                MAX_FRAME_BYTES,
                Some(self.cfg.frame_body_timeout),
            ) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) if cluster_wire::io_timed_out(&e) => {
                    log::warn!(
                        "job wire: session {} sent no frame within {:?} (heartbeat cadence \
                         {:?}) — closing the idle session",
                        session.id,
                        self.session_idle,
                        self.cfg.heartbeat_interval
                    );
                    break;
                }
                Err(e) => {
                    log::warn!("job wire: session {} read error: {e}", session.id);
                    break;
                }
            };
            match frame {
                WireFrame::Heartbeat { .. } => {
                    self.renew_lease(&session);
                    let mut w = session.writer.lock();
                    if w.send(&WireFrame::HeartbeatAck {
                        lease_ttl_ms: self.cfg.lease_ttl.as_millis() as u64,
                    })
                    .is_err()
                    {
                        break;
                    }
                }
                WireFrame::ResultSubmit {
                    job_id,
                    shard,
                    shard_fencing,
                    checksums,
                } => {
                    self.handle_submit(&session, job_id, shard, shard_fencing, checksums)
                        .await;
                }
                WireFrame::ReadShardResult {
                    job_id,
                    shard,
                    shard_fencing,
                    payload,
                } => {
                    self.handle_read_result(&session, job_id, shard, shard_fencing, payload)
                        .await;
                }
                WireFrame::ShardAbandon {
                    job_id,
                    shard,
                    shard_fencing,
                    reason,
                } => {
                    self.handle_abandon(&session, job_id, shard, shard_fencing, &reason)
                        .await;
                }
                other => {
                    log::warn!(
                        "job wire: session {} sent an unexpected frame {other:?} — ignored",
                        session.id
                    );
                }
            }
        }

        // Shutdown short-circuit (the abort analog): a session woken by
        // the teardown nudge must not run the departure ceremony —
        // shutdown owns the fence release, and an expire/requeue against
        // a stopping fabric would park the join on metadata commits.
        if self.shutdown.load(Ordering::SeqCst) {
            self.sessions.lock().remove(&session.id);
            return;
        }

        // Departure: a session holding an unexpired shard is treated as
        // expired NOW (its dial is gone — same reclaim law, no wait).
        self.sessions.lock().remove(&session.id);
        let held: Vec<Arc<ShardState>> = {
            let shards = self.shards.lock();
            shards
                .values()
                .filter(|s| {
                    !s.done.load(Ordering::SeqCst)
                        && s.holder
                            .lock()
                            .as_ref()
                            .is_some_and(|h| h.session_id == session.id)
                })
                .cloned()
                .collect()
        };
        for shard in held {
            self.expire_shard(&shard, "holder departed").await;
        }
        self.on_worker_departed(Some(&session)).await;
        log::info!(
            "job wire: worker {} departed (session {})",
            session.worker_id,
            session.id
        );
    }

    // -- WERO fence (rung 2) --------------------------------------------------

    /// First remote enrollment ⇒ acquire WERO on every configured data
    /// namespace. `pr` requires ALL of them PR-capable; anything less is
    /// the documented deferred-reclaim class (loud, no bypass knob).
    async fn on_first_enrollment(self: &Arc<Self>) {
        if self.cfg.data_device_paths.is_empty() {
            return;
        }
        let mut fence = self.fence.lock().await;
        if fence.is_some() {
            return;
        }
        let paths = self.cfg.data_device_paths.clone();
        // Direct call: this runs on a wire-owned OS thread (conn/serve),
        // so the blocking ioctl fan-out no longer needs a spawn_blocking
        // hop (its JoinError arm is dead and deleted with it).
        let acquired = crate::data_custody::acquire_wero(&paths);
        match acquired {
            Some(f) => {
                log::info!(
                    "job wire: WERO (rtype 3) held on the data namespaces (key {:#x}) — \
                     guarantee class pr (expired worker hosts will be PR-preempted). The \
                     hold is the process's ONE data-plane reservation (DLM S7, \
                     data_custody::acquire_wero): an S7-armed mount and this fence share \
                     it rather than conflicting at the device",
                    f.key()
                );
                *fence = Some(f);
                self.pr_mode.store(true, Ordering::SeqCst);
                METRICS.job_remote_fence_mode.store(1, Ordering::Relaxed);
            }
            None => {
                log::warn!(
                    "job wire: data namespaces are not (all) PR-capable — guarantee class \
                     deferred-reclaim: quarantine reclaim defers to job end; the \
                     unbounded-pause zombie window is the documented residual class \
                     (design-volume-lifecycle §5.1.6 rung 3)"
                );
            }
        }
    }

    /// Last departure ⇒ release the WERO hold (zero residue).
    async fn on_worker_departed(self: &Arc<Self>, _session: Option<&Session>) {
        let workers = METRICS.job_remote_workers.load(Ordering::Relaxed);
        METRICS
            .job_remote_workers
            .store(workers.saturating_sub(1), Ordering::Relaxed);
        // Snapshot the emptiness BEFORE awaiting: an `if` condition's
        // temporary guard would otherwise be held across the await.
        let last = self.sessions.lock().is_empty();
        if last {
            self.release_fence().await;
        }
    }

    async fn release_fence(&self) {
        let mut fence = self.fence.lock().await;
        if let Some(f) = fence.take() {
            // Dropping the last hold releases the reservation AND its
            // registration (zero residue) — off the runtime, because the
            // release is one ioctl per namespace.
            crate::data_custody::release_hold(f).await;
            self.pr_mode.store(false, Ordering::SeqCst);
            METRICS.job_remote_fence_mode.store(0, Ordering::Relaxed);
            log::info!("job wire: WERO released (last remote worker departed)");
        }
    }

    // -- dispatcher ------------------------------------------------------------

    /// Claim queued jobs through the same gate as the local pool and
    /// stream them to idle enrolled workers as shards.
    async fn dispatcher_loop(self: Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let Some(session) = self.pick_idle_session() else {
                sqz_time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            // `true`: the wire claims only wire-executable job types —
            // the VL4 movers stay on the local pool (their per-ino meta
            // publish is coordinator-local; see `JobType::wire_executable`).
            let Some((job_id, ctl)) = self.fabric.claim_next(true) else {
                let _ = sqz_time::timeout(Duration::from_millis(250), self.fabric.work_notified())
                    .await;
                continue;
            };
            if let Err(e) = self.assign(&job_id, &ctl, &session).await {
                log::warn!(
                    "job wire: assigning job {job_id} to session {} failed ({e}) — requeued",
                    session.id
                );
                session.expired.store(true, Ordering::SeqCst);
                session.busy.store(false, Ordering::SeqCst);
                self.fabric.requeue_remote(&job_id, &ctl).await;
            }
        }
    }

    fn pick_idle_session(&self) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .values()
            .find(|s| !s.busy.load(Ordering::SeqCst) && !s.expired.load(Ordering::SeqCst))
            .cloned()
    }

    /// KD-MW-16: an idle, non-expired session advertising `cap` —
    /// optionally the one
    /// belonging to a NAMED worker (KD-PV-16: an inode-plane shard has
    /// exactly one legitimate venue, the owner of the volumes it asks
    /// about, so "any idle member" is not a substitute).
    fn pick_session(&self, cap: u32, worker_id: Option<&str>) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .values()
            .find(|s| {
                s.caps & cap == cap
                    && !s.busy.load(Ordering::SeqCst)
                    && !s.expired.load(Ordering::SeqCst)
                    && worker_id.is_none_or(|id| s.worker_id == id)
            })
            .cloned()
    }

    /// KD-MW-16: idle read-capable sessions right now — the population a
    /// fleet fan-out may shard across (the [`crate::jobs::FleetDispatch`]
    /// face, exposed for the contracts and the stats surface).
    pub fn fleet_read_capacity(&self) -> usize {
        self.sessions
            .lock()
            .values()
            .filter(|s| {
                s.caps & CAP_FLEET_READ == CAP_FLEET_READ
                    && !s.busy.load(Ordering::SeqCst)
                    && !s.expired.load(Ordering::SeqCst)
            })
            .count()
    }

    // -- fleet read shards (KD-MW-16, design-mw-fleet-jobs §4) -----------------

    /// Handle a fleet READ shard's result proposal: fencing-checked
    /// exactly like a mutating submission; no verify-read (nothing was
    /// written — the merge accounting and the coordinator's finalize
    /// ladder are this class's verification); the payload lands on the
    /// dispatching executor's outcome channel.
    async fn handle_read_result(
        self: &Arc<Self>,
        session: &Arc<Session>,
        job_id: String,
        shard_no: u32,
        shard_fencing: u64,
        payload: Vec<u8>,
    ) {
        let shard = self
            .shards
            .lock()
            .get(&(job_id.clone(), shard_no))
            .cloned()
            .filter(|s| s.fleet.is_some());
        let Some(shard) = shard else {
            METRICS
                .job_remote_refused_stale
                .fetch_add(1, Ordering::Relaxed);
            refuse_submit(
                session,
                &job_id,
                shard_no,
                "unknown fleet shard (reclaimed or never assigned)".into(),
            );
            return;
        };
        let current = shard.fencing.load(Ordering::SeqCst);
        if shard_fencing != current || shard.done.load(Ordering::SeqCst) {
            METRICS
                .job_remote_refused_stale
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: session {} proposed fleet shard {job_id}:{shard_no} with stale \
                 fencing {shard_fencing} (current {current}) — refused \
                 (job_remote_refused_stale); the residue was re-leased",
                session.id
            );
            refuse_submit(
                session,
                &job_id,
                shard_no,
                format!("stale shard_fencing {shard_fencing} (current {current})"),
            );
            return;
        }

        shard.done.store(true, Ordering::SeqCst);
        // §5.8.2 clause 2: the outcome carries the LEASE HOLDER's id from
        // this table, captured before the lease is released — the
        // coordinator's inode-plane admission reads it, never the
        // payload's own account of who sent it.
        let holder_id = shard.holder.lock().take().map(|h| h.worker_id);
        session.busy.store(false, Ordering::SeqCst);
        METRICS
            .job_remote_submissions
            .fetch_add(1, Ordering::Relaxed);
        METRICS
            .job_fleet_shards_completed
            .fetch_add(1, Ordering::Relaxed);
        self.persist_shard_record(&shard, "completed").await;
        if let Some(tx) = shard.outcome.lock().as_ref() {
            let _ = tx.send(crate::jobs::FleetOutcome {
                shard: shard_no,
                payload: Some(payload),
                worker_id: holder_id,
            });
        }
        let mut w = session.writer.lock();
        let _ = w.send(&WireFrame::ResultAck {
            job_id: job_id.clone(),
            shard: shard_no,
        });
    }

    /// Handle a worker's voluntary shard abandon (R5 Red, unsupported
    /// capability): fencing-checked, then the expiry law runs NOW — the
    /// prompt form of the TTL (design-mw-fleet-jobs §5).
    async fn handle_abandon(
        self: &Arc<Self>,
        session: &Arc<Session>,
        job_id: String,
        shard_no: u32,
        shard_fencing: u64,
        reason: &str,
    ) {
        let shard = self.shards.lock().get(&(job_id.clone(), shard_no)).cloned();
        let Some(shard) = shard else {
            return; // already reclaimed — nothing to abandon
        };
        let current = shard.fencing.load(Ordering::SeqCst);
        if shard_fencing != current || shard.done.load(Ordering::SeqCst) {
            return; // stale abandon — the expiry law already ran
        }
        let held_here = shard
            .holder
            .lock()
            .as_ref()
            .is_some_and(|h| h.session_id == session.id);
        if !held_here {
            return;
        }
        log::warn!(
            "job wire: worker {} ABANDONED shard {job_id}:{shard_no}: {reason} — \
             re-leasing now (the prompt form of the TTL law)",
            session.worker_id
        );
        self.expire_shard(&shard, &format!("worker abandoned: {reason}"))
            .await;
    }

    /// Assign one claimed job to `session` as shard 0: pre-allocate
    /// FRESH destinations, arm the lease, persist the shard record,
    /// stream the descriptor.
    async fn assign(
        self: &Arc<Self>,
        job_id: &str,
        ctl: &Arc<JobCtl>,
        session: &Arc<Session>,
    ) -> Result<()> {
        // Reassignments reuse the shard state (and its BUMPED fencing);
        // first assignments start at fencing 0.
        let shard = {
            let mut shards = self.shards.lock();
            Arc::clone(shards.entry((job_id.to_string(), 0)).or_insert_with(|| {
                Arc::new(ShardState {
                    job_id: job_id.to_string(),
                    shard: 0,
                    ctl: Some(Arc::clone(ctl)),
                    fencing: AtomicU64::new(0),
                    holder: parking_lot::Mutex::new(None),
                    destinations: parking_lot::Mutex::new(Vec::new()),
                    done: AtomicBool::new(false),
                    fleet: None,
                    outcome: parking_lot::Mutex::new(None),
                })
            }))
        };

        // The fresh-destination law: EVERY assignment allocates fresh
        // tuples; the allocator must never resurrect a quarantined one.
        let blocks = self.seam.plan_blocks(&ctl.job_type);
        let dests = self.seam.allocate(blocks)?;
        {
            let quarantine = self.quarantine.lock();
            if let Some(dup) = dests.iter().find(|d| quarantine.contains(*d)) {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "allocator returned quarantined destination {dup:?} — fresh-destination \
                     law violated (allocator contract breach)"
                )));
            }
        }
        *shard.destinations.lock() = dests.clone();
        *shard.holder.lock() = Some(ShardHolder {
            session_id: session.id,
            worker_id: session.worker_id.clone(),
            lease_expiry: Instant::now() + self.cfg.lease_ttl,
        });
        let fencing = shard.fencing.load(Ordering::SeqCst);

        session.busy.store(true, Ordering::SeqCst);
        METRICS.job_remote_shards.fetch_add(1, Ordering::Relaxed);
        self.persist_shard_record(&shard, "assigned").await;
        self.fabric.remote_running(job_id, ctl).await;

        let descriptor = ShardDescriptor {
            job_id: job_id.to_string(),
            shard: shard.shard,
            shard_fencing: fencing,
            job_type: ctl.job_type.clone(),
            source_keys: Vec::new(),
            destinations: dests,
            block_len: self.seam.block_len() as u32,
            throttle_pct: ctl.throttle.load(Ordering::Relaxed),
            lease_ttl_ms: self.cfg.lease_ttl.as_millis() as u64,
            fleet: None,
            inode_plane: false,
        };
        let mut w = session.writer.lock();
        w.send(&WireFrame::ShardAssign { shard: descriptor })
            .map_err(SqueezefsError::from)
    }

    // -- leases ------------------------------------------------------------

    /// Heartbeat renewal: extend the lease of the shard this session
    /// holds (an expired session renews nothing — its shard is gone).
    fn renew_lease(&self, session: &Arc<Session>) {
        if session.expired.load(Ordering::SeqCst) {
            return;
        }
        let shards = self.shards.lock();
        for shard in shards.values() {
            let mut holder = shard.holder.lock();
            if let Some(h) = holder.as_mut() {
                if h.session_id == session.id {
                    h.lease_expiry = Instant::now() + self.cfg.lease_ttl;
                }
            }
        }
    }

    /// The lease sweeper: expiry ⇒ fencing bump + quarantine + PR
    /// preempt + requeue.
    async fn sweeper_loop(self: Arc<Self>) {
        let tick = (self.cfg.lease_ttl / 4).max(Duration::from_millis(25));
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            // Tick in bounded slices so a long lease TTL never parks the
            // sweeper past the shutdown latch.
            let mut remaining = tick;
            while remaining > Duration::ZERO {
                if self.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let slice = remaining.min(Duration::from_millis(250));
                sqz_time::sleep(slice).await;
                remaining = remaining.saturating_sub(slice);
            }
            let now = Instant::now();
            let expired: Vec<Arc<ShardState>> = {
                let shards = self.shards.lock();
                shards
                    .values()
                    .filter(|s| {
                        !s.done.load(Ordering::SeqCst)
                            && s.holder
                                .lock()
                                .as_ref()
                                .is_some_and(|h| now >= h.lease_expiry)
                    })
                    .cloned()
                    .collect()
            };
            for shard in expired {
                self.expire_shard(&shard, "lease TTL expired (missed heartbeats)")
                    .await;
            }
        }
    }

    /// Expire a shard's lease: bump `shard_fencing` (the old holder's
    /// late submission refuses), quarantine its destinations
    /// (do-not-publish — the live-zombie half of the law), PR-preempt
    /// the expired worker HOST's registration where the fence is held,
    /// and requeue the job for any population.
    async fn expire_shard(self: &Arc<Self>, shard: &Arc<ShardState>, why: &str) {
        let Some(holder) = shard.holder.lock().take() else {
            return;
        };
        shard.fencing.fetch_add(1, Ordering::SeqCst);
        METRICS
            .job_remote_lease_expiries
            .fetch_add(1, Ordering::Relaxed);
        // Counted with the expiry (the requeue below is unconditional):
        // an expired shard IS a reassignment — whoever claims it next.
        METRICS
            .job_remote_reassignments
            .fetch_add(1, Ordering::Relaxed);

        // KD-MW-16 (design-mw-fleet-jobs §5): a fleet READ shard's expiry
        // takes the fencing/notify arm ONLY. There are no destinations to
        // quarantine (nothing was pre-allocated) and NO PR preempt — a
        // read worker DMAs nothing, and preempting a live host's
        // registrant key over a lost READ shard would fence its DATA
        // plane (a co-located co-writer's custody rides that key). The
        // holder session stays open-but-unassignable so the late
        // proposal is REFUSED, not dropped; the dispatching executor is
        // told to re-lease the residue.
        if shard.fleet.is_some() {
            log::warn!(
                "job wire: fleet read shard {}:{} lease of worker {} expired ({why}) — \
                 fencing bumped to {}, residue handed back for re-lease",
                shard.job_id,
                shard.shard,
                holder.worker_id,
                shard.fencing.load(Ordering::SeqCst)
            );
            if let Some(s) = self.sessions.lock().get(&holder.session_id) {
                s.expired.store(true, Ordering::SeqCst);
                s.busy.store(false, Ordering::SeqCst);
            }
            self.persist_shard_record(shard, "reclaimed").await;
            if let Some(tx) = shard.outcome.lock().as_ref() {
                let _ = tx.send(crate::jobs::FleetOutcome {
                    shard: shard.shard,
                    payload: None,
                    worker_id: Some(holder.worker_id.clone()),
                });
            }
            return;
        }

        log::warn!(
            "job wire: shard {}:{} lease of worker {} expired ({why}) — fencing bumped to \
             {}, destinations quarantined, shard requeued",
            shard.job_id,
            shard.shard,
            holder.worker_id,
            shard.fencing.load(Ordering::SeqCst)
        );

        // Quarantine the expired lease's destinations (never reused
        // within the job; reclaimed at job end / next mount tree-walk).
        // DLM S7: the same law, pushed down into the ALLOCATOR — the
        // do-not-publish set above stops the coordinator from publishing
        // them, the allocator quarantine stops ANY path (a later free,
        // recovery, an fsck repair) from handing them to a new owner while
        // this dead worker can still DMA into them (spec §6.7 "Recovery").
        let old_dests = std::mem::take(&mut *shard.destinations.lock());
        let dead_epoch = if old_dests.is_empty() {
            None
        } else {
            let epoch = crate::data_custody::declare_dead_epoch(&format!(
                "job wire: worker {} lease expired ({why})",
                holder.worker_id
            ));
            self.seam.quarantine(&old_dests, epoch);
            let mut q = self.quarantine.lock();
            for d in old_dests {
                if q.insert(d) {
                    METRICS
                        .job_remote_quarantined_destinations
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(epoch)
        };

        // The holder session is never assignable again; its connection
        // stays open so the late ResultSubmit is REFUSED, not dropped.
        let victim_pr_key = {
            let sessions = self.sessions.lock();
            sessions.get(&holder.session_id).map(|s| {
                s.expired.store(true, Ordering::SeqCst);
                s.busy.store(false, Ordering::SeqCst);
                s.pr_key
            })
        }
        .flatten();

        // Rung 2: PR-preempt the expired host's data-namespace
        // registration under the standing WERO — its resumed DMA is
        // device-rejected while every other registrant keeps writing.
        if let Some(victim) = victim_pr_key {
            // RES-16: CLONE the hold and RELEASE the mutex before the
            // blocking ioctl fan-out — the guard used to be held across
            // it, so one slow namespace stalled every other fence
            // transition (acquire at first enrollment, release at last
            // departure). The preempt law itself lives in `data_custody`
            // (DLM S7 — one place issues rung 2).
            let hold = {
                let fence = self.fence.lock().await;
                fence.clone()
            };
            if let Some(hold) = hold {
                // Direct call: expire_shard runs on a wire-owned OS
                // thread (sweeper/conn), so the blocking ioctl fan-out
                // needs no spawn_blocking hop (its JoinError fallback arm
                // is dead and deleted with it).
                let preempted = hold.preempt(victim);
                if preempted > 0 {
                    METRICS
                        .job_remote_pr_preempts
                        .fetch_add(preempted, Ordering::Relaxed);
                    log::warn!(
                        "job wire: preempted expired worker host's PR registration \
                         {victim:#x} on {preempted} namespace(s) — its resumed DMA is \
                         device-rejected"
                    );
                    // DLM S7: the landed preempt IS the drain proof — the
                    // victim host cannot submit another command, so the
                    // allocator quarantine may release this epoch's
                    // cohort. On a detection-grade substrate no proof
                    // exists and the cohort stays quarantined (the
                    // documented deferred-reclaim class: the space
                    // returns at the next mount's recovery walk).
                    if let Some(epoch) = dead_epoch {
                        self.seam.release_quarantine(epoch);
                    }
                }
            }
        }

        self.persist_shard_record(shard, "reclaimed").await;
        if let Some(ctl) = &shard.ctl {
            self.fabric.requeue_remote(&shard.job_id, ctl).await;
        }
    }

    // -- result submission ------------------------------------------------------

    /// Fencing-checked, verify-read result handling (Issue-30 law).
    async fn handle_submit(
        self: &Arc<Self>,
        session: &Arc<Session>,
        job_id: String,
        shard_no: u32,
        shard_fencing: u64,
        checksums: Vec<BlockChecksum>,
    ) {
        let shard = self
            .shards
            .lock()
            .get(&(job_id.clone(), shard_no))
            .cloned()
            .filter(|s| s.fleet.is_none());
        let Some(shard) = shard else {
            METRICS
                .job_remote_refused_stale
                .fetch_add(1, Ordering::Relaxed);
            refuse_submit(
                session,
                &job_id,
                shard_no,
                "unknown shard (reclaimed or never assigned)".into(),
            );
            return;
        };
        let current = shard.fencing.load(Ordering::SeqCst);
        if shard_fencing != current || shard.done.load(Ordering::SeqCst) {
            METRICS
                .job_remote_refused_stale
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: session {} submitted shard {job_id}:{shard_no} with stale \
                 fencing {shard_fencing} (current {current}) — refused \
                 (job_remote_refused_stale); its destinations are quarantined garbage",
                session.id
            );
            refuse_submit(
                session,
                &job_id,
                shard_no,
                format!("stale shard_fencing {shard_fencing} (current {current})"),
            );
            return;
        }

        // Verification at submission: verify-read destinations against
        // the submitted checksums BEFORE publish — 100 % on plaintext,
        // sampled under TLS.
        let dests = shard.destinations.lock().clone();
        match self.verify_destinations(&dests, &checksums) {
            Ok(()) => {}
            Err(e) => {
                log::error!(
                    "job wire: shard {job_id}:{shard_no} verification FAILED ({e}) — \
                     refused; fencing bumped and destinations quarantined (unpublished \
                     garbage, reclaimed by the §5.4 law)"
                );
                refuse_submit(
                    session,
                    &job_id,
                    shard_no,
                    format!("verification failed: {e}"),
                );
                // A failed proposal is treated like an expired lease:
                // never publish, never reuse those destinations.
                *shard.holder.lock() = Some(ShardHolder {
                    session_id: session.id,
                    worker_id: session.worker_id.clone(),
                    lease_expiry: Instant::now(),
                });
                self.expire_shard(&shard, "verification failed").await;
                return;
            }
        }

        // Publish: for the Noop vehicle this is the job's completion
        // commit through the fabric (the movers' merge_block_mappings
        // publish lands in VL4 behind the same gate).
        shard.done.store(true, Ordering::SeqCst);
        *shard.holder.lock() = None;
        session.busy.store(false, Ordering::SeqCst);
        let bytes: u64 = dests.len() as u64 * self.seam.block_len() as u64;
        METRICS
            .job_remote_submissions
            .fetch_add(1, Ordering::Relaxed);
        METRICS
            .job_remote_bytes_moved
            .fetch_add(bytes, Ordering::Relaxed);
        self.persist_shard_record(&shard, "completed").await;
        if let Some(ctl) = &shard.ctl {
            self.fabric.remote_complete(&job_id, ctl).await;
        }

        let mut w = session.writer.lock();
        let _ = w.send(&WireFrame::ResultAck {
            job_id: job_id.clone(),
            shard: shard_no,
        });
    }

    /// Verify-read `dests` against the submitted checksums. Plaintext ⇒
    /// every destination; TLS ⇒ the configured sample (Issue-30).
    fn verify_destinations(
        &self,
        dests: &[DestTuple],
        checksums: &[BlockChecksum],
    ) -> std::io::Result<()> {
        if checksums.len() != dests.len() {
            return Err(std::io::Error::other(format!(
                "checksum count {} != destination count {}",
                checksums.len(),
                dests.len()
            )));
        }
        let by_dest: HashMap<&DestTuple, &BlockChecksum> =
            checksums.iter().map(|c| (&c.dest, c)).collect();
        for dest in dests {
            let Some(cs) = by_dest.get(dest) else {
                return Err(std::io::Error::other(format!(
                    "no checksum submitted for destination {dest:?}"
                )));
            };
            if self.verify_permille < 1000 && fastrand::u32(0..1000) >= self.verify_permille {
                continue; // sampled out (TLS only — plaintext is pinned to 1000)
            }
            let data = self.seam.read_block(dest)?;
            METRICS
                .job_remote_verify_read_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            if data.len() != cs.len as usize || xxh3_64(&data) != cs.xxh3 {
                return Err(std::io::Error::other(format!(
                    "checksum mismatch at {dest:?}"
                )));
            }
        }
        Ok(())
    }

    /// Persist the durable `job:{id}:shard:{k}` record (KD-2 plane —
    /// offline-probe visible, schema-versioned). Best-effort like every
    /// advisory checkpoint.
    async fn persist_shard_record(&self, shard: &Arc<ShardState>, state: &str) {
        let holder = shard.holder.lock().as_ref().map(|h| h.worker_id.clone());
        let record = serde_json::json!({
            "schema": 1,
            "job_id": shard.job_id,
            "shard": shard.shard,
            "state": state,
            "holder": holder,
            "shard_fencing": shard.fencing.load(Ordering::SeqCst),
            "lease_ttl_ms": self.cfg.lease_ttl.as_millis() as u64,
            "destinations": *shard.destinations.lock(),
        });
        let name = format!("job:{}:shard:{}", shard.job_id, shard.shard);
        if let Err(e) = self
            .fabric
            .meta_handle()
            .setxattr(1, &name, record.to_string().as_bytes())
            .await
        {
            log::warn!("job wire: shard record {name} persist failed: {e}");
        }
    }
}

/// KD-MW-16 (design-mw-fleet-jobs §4): the fleet read-shard dispatch
/// seam the fabric's executors consume. Assignment mirrors the whole-job
/// `assign` — same lease law, same fencing identity across re-dispatches
/// — minus everything a read shard structurally lacks: destinations,
/// checksums, verify-reads, quarantine, PR preemption.
impl crate::jobs::FleetDispatch for JobWireHost {
    fn read_capacity(&self) -> usize {
        self.fleet_read_capacity()
    }

    fn shard_lease_ttl(&self) -> std::time::Duration {
        self.cfg.lease_ttl
    }

    fn dispatch_read_shard(
        &self,
        job_id: &str,
        shard_no: u32,
        shard_count: u32,
        job_type: &JobType,
        throttle_pct: u32,
        tx: &crate::jobs::FleetOutcomeTx,
    ) -> bool {
        self.dispatch_fleet_shard(
            job_id,
            shard_no,
            shard_count,
            job_type,
            throttle_pct,
            tx,
            None,
        )
    }

    fn dispatch_inode_plane_shard(
        &self,
        job_id: &str,
        shard_no: u32,
        worker_id: &str,
        job_type: &JobType,
        throttle_pct: u32,
        tx: &crate::jobs::FleetOutcomeTx,
    ) -> bool {
        // `shard_count` is carried unchanged for the record/frame shape;
        // a plane shard covers volumes, not a residue of `n`.
        self.dispatch_fleet_shard(
            job_id,
            shard_no,
            1,
            job_type,
            throttle_pct,
            tx,
            Some(worker_id),
        )
    }

    fn retire_fleet_shards(&self, job_id: &str) {
        self.shards
            .lock()
            .retain(|(jid, _), s| jid != job_id || s.fleet.is_none());
    }
}

impl JobWireHost {
    /// The shared body of both fleet dispatch verbs: `plane_owner`
    /// `Some(worker_id)` makes this an inode-plane shard bound to that
    /// owner's session (KD-PV-16); `None` is the census residue's
    /// any-idle-member shape, unchanged.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_fleet_shard(
        &self,
        job_id: &str,
        shard_no: u32,
        shard_count: u32,
        job_type: &JobType,
        throttle_pct: u32,
        tx: &crate::jobs::FleetOutcomeTx,
        plane_owner: Option<&str>,
    ) -> bool {
        if self.shutdown.load(Ordering::SeqCst) || !self.listening {
            return false;
        }
        let Some(session) = self.pick_session(CAP_FLEET_READ, plane_owner) else {
            return false;
        };
        // Re-dispatches of a lost residue REUSE the shard state (and its
        // BUMPED fencing) — the stale-refusal law's identity.
        let shard = {
            let mut shards = self.shards.lock();
            Arc::clone(
                shards
                    .entry((job_id.to_string(), shard_no))
                    .or_insert_with(|| {
                        Arc::new(ShardState {
                            job_id: job_id.to_string(),
                            shard: shard_no,
                            ctl: None,
                            fencing: AtomicU64::new(0),
                            holder: parking_lot::Mutex::new(None),
                            destinations: parking_lot::Mutex::new(Vec::new()),
                            done: AtomicBool::new(false),
                            fleet: Some((shard_no, shard_count)),
                            outcome: parking_lot::Mutex::new(None),
                        })
                    }),
            )
        };
        if shard.done.load(Ordering::SeqCst) {
            return false; // already completed (a late re-lease retry)
        }
        *shard.outcome.lock() = Some(tx.clone());
        *shard.holder.lock() = Some(ShardHolder {
            session_id: session.id,
            worker_id: session.worker_id.clone(),
            lease_expiry: Instant::now() + self.cfg.lease_ttl,
        });
        let fencing = shard.fencing.load(Ordering::SeqCst);
        session.busy.store(true, Ordering::SeqCst);

        let descriptor = ShardDescriptor {
            job_id: job_id.to_string(),
            shard: shard_no,
            shard_fencing: fencing,
            job_type: job_type.clone(),
            source_keys: Vec::new(),
            destinations: Vec::new(),
            block_len: 0,
            throttle_pct,
            lease_ttl_ms: self.cfg.lease_ttl.as_millis() as u64,
            fleet: Some((shard_no, shard_count)),
            inode_plane: plane_owner.is_some(),
        };
        let sent = {
            let mut w = session.writer.lock();
            w.send(&WireFrame::ShardAssign { shard: descriptor })
                .is_ok()
        };
        if !sent {
            // The session vanished mid-dispatch: undo, tell the caller to
            // pick another venue (never a lost residue — the caller still
            // owns it).
            *shard.holder.lock() = None;
            *shard.outcome.lock() = None;
            session.expired.store(true, Ordering::SeqCst);
            session.busy.store(false, Ordering::SeqCst);
            return false;
        }
        METRICS.job_remote_shards.fetch_add(1, Ordering::Relaxed);
        METRICS
            .job_fleet_shards_dispatched
            .fetch_add(1, Ordering::Relaxed);

        // Best-effort durable shard record (KD-2 plane) — spawned: this
        // seam is sync (the executor's fan-out calls it inline) and the
        // record is advisory, like every checkpoint.
        let meta = Arc::clone(self.fabric.meta_handle());
        let record = serde_json::json!({
            "schema": 1,
            "job_id": job_id,
            "shard": shard_no,
            "state": "assigned",
            "holder": session.worker_id.clone(),
            "shard_fencing": fencing,
            "lease_ttl_ms": self.cfg.lease_ttl.as_millis() as u64,
            "fleet": [shard_no, shard_count],
            "inode_plane": plane_owner.is_some(),
        })
        .to_string();
        let name = format!("job:{job_id}:shard:{shard_no}");
        crate::meta_exec::spawn_meta("fleet_shard_record", async move {
            if let Err(e) = meta.setxattr(1, &name, record.as_bytes()).await {
                log::warn!("job wire: fleet shard record {name} persist failed: {e}");
            }
        });
        true
    }
}

/// Send one ResultRefused reply (best-effort — a vanished session's
/// refusal has nowhere to land, which is fine: fencing already holds).
fn refuse_submit(session: &Arc<Session>, job_id: &str, shard: u32, reason: String) {
    let mut w = session.writer.lock();
    let _ = w.send(&WireFrame::ResultRefused {
        job_id: job_id.to_string(),
        shard,
        reason,
    });
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Worker knobs + test hooks (the zombie models the G-VL-7 legs need:
/// a partitioned/paused worker stops heartbeating and can park its
/// submission past the TTL).
#[derive(Clone)]
pub struct WorkerOptions {
    pub worker_id: String,
    /// Heartbeating enabled (clear = the partition/pause model; also
    /// disables the per-batch wire round-trip, as a real partition
    /// would).
    pub heartbeats: Arc<AtomicBool>,
    /// Park the next ResultSubmit until cleared (the pause-before-
    /// submit zombie window).
    pub hold_submission: Arc<AtomicBool>,
    /// The PR key this worker's HOST registered on the shared data
    /// namespaces (reported at enrollment so the coordinator can
    /// preempt it on lease expiry).
    pub pr_key: Option<u64>,
    /// TLS client config source (must match the coordinator's).
    pub security: Option<ClusterSecurityConfig>,
    /// KD-MW-16: capability mask advertised at enrollment
    /// ([`CAP_FLEET_READ`]) — shard routing, never authentication.
    pub caps: u32,
    /// Test observable (the `hold_submission` precedent): bumps when a
    /// submission ACCEPT ack lands at this worker. A coordinator
    /// `shutdown()` hard-kills sockets by design, so a worker's local
    /// `shards_completed` legally races an in-flight ack — the
    /// 2026-08-18 consolidation gate caught a suite asserting the
    /// report through that race. Polling this before shutdown makes the
    /// assertion deterministic without weakening it. Never read by
    /// product code.
    pub acks_received: Arc<std::sync::atomic::AtomicU64>,
}

impl WorkerOptions {
    pub fn new(worker_id: &str) -> Self {
        Self {
            worker_id: worker_id.to_string(),
            heartbeats: Arc::new(AtomicBool::new(true)),
            hold_submission: Arc::new(AtomicBool::new(false)),
            pr_key: None,
            security: None,
            caps: 0,
            acks_received: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

/// What a worker run did (returned when the coordinator connection
/// closes).
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerReport {
    pub shards_completed: u64,
    pub submissions_refused: u64,
    /// Shards abandoned by the per-batch lease re-validation (a woken
    /// zombie aborting before its next batch).
    pub shards_aborted: u64,
}

/// An enrolled remote worker (client side of the wire). Carries the
/// derived session key: enrollment is the LAST unauthenticated exchange,
/// and every frame after it is MAC'd in both directions.
pub struct JobWireWorker {
    stream: ClusterStream,
    opts: WorkerOptions,
    heartbeat: Duration,
    key: SessionKey,
}

impl std::fmt::Debug for JobWireWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobWireWorker")
            .field("worker_id", &self.opts.worker_id)
            .field("heartbeat", &self.heartbeat)
            .finish_non_exhaustive()
    }
}

impl JobWireWorker {
    /// Dial, enroll (HMAC over the storage-membership secret), and
    /// return the enrolled worker. Refusals surface loud with the
    /// coordinator's reason. Async API preserved; the socket work runs on
    /// the blocking pool.
    pub async fn connect(endpoint: &str, secret: &[u8], opts: WorkerOptions) -> Result<Self> {
        let endpoint = endpoint.to_string();
        let secret = secret.to_vec();
        sqz_blocking::run_blocking(move || Self::connect_sync(&endpoint, &secret, opts)).await
    }

    fn connect_sync(endpoint: &str, secret: &[u8], opts: WorkerOptions) -> Result<Self> {
        let tcp = cluster_wire::dial_tcp(endpoint, ENROLL_DIAL_TIMEOUT)?;
        // The whole enrollment exchange is bounded per syscall at the
        // socket (the tokio::time::timeout wrappers this replaces bounded
        // the same exchanges).
        tcp.set_read_timeout(Some(ENROLL_DIAL_TIMEOUT))?;
        tcp.set_write_timeout(Some(ENROLL_DIAL_TIMEOUT))?;
        // S3: a CA-less TLS config REFUSES here (`tls_connector`) instead
        // of warning and presenting the storage secret's proof into a pipe
        // whose far end was never validated.
        let (mut stream, binding): (ClusterStream, Option<[u8; 32]>) = match opts.security.as_ref()
        {
            Some(sec) => {
                // The ClusterSecurityConfig node certs carry
                // localhost/127.0.0.1 SANs (cluster_tls.rs construction).
                let tls =
                    cluster_wire::tls_client_handshake(cluster_wire::tls_connector(sec)?, tcp)?;
                let binding = cluster_wire::client_exporter(&tls.conn);
                (ClusterStream::tls_client(tls), binding)
            }
            None => (ClusterStream::tcp(tcp), None),
        };
        // The coordinator speaks first: its challenge nonce is what the
        // proof is bound to. Bounded read at the hello class — a hostile
        // "coordinator" gets no allocation authority either.
        let server_nonce = match read_frame_limited(
            &mut stream,
            MAX_HELLO_FRAME_BYTES,
            Some(ENROLL_DIAL_TIMEOUT),
        )
        .map_err(|e| {
            if cluster_wire::io_timed_out(&e) {
                SqueezefsError::InvalidOperation(format!(
                    "no enrollment challenge from the coordinator within {ENROLL_DIAL_TIMEOUT:?}"
                ))
            } else {
                SqueezefsError::from(e)
            }
        })? {
            Some(WireFrame::Challenge {
                wire_schema,
                server_nonce,
                ..
            }) => {
                if wire_schema != WIRE_SCHEMA {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "coordinator speaks wire_schema {wire_schema}, this worker speaks \
                         {WIRE_SCHEMA}"
                    )));
                }
                server_nonce
            }
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "expected an enrollment Challenge first, got {other:?}"
                )))
            }
        };
        let nonce = uuid::Uuid::new_v4().to_string();
        write_frame(
            &mut stream,
            &WireFrame::Enroll {
                wire_schema: WIRE_SCHEMA,
                worker_id: opts.worker_id.clone(),
                server_nonce: server_nonce.clone(),
                endpoint_nonce: nonce.clone(),
                hmac: enroll_hmac(secret, &opts.worker_id, &server_nonce, &nonce),
                pr_key: opts.pr_key,
                caps: opts.caps,
            },
        )?;
        match read_frame_limited(
            &mut stream,
            MAX_HELLO_FRAME_BYTES,
            Some(ENROLL_DIAL_TIMEOUT),
        )? {
            Some(WireFrame::EnrollOk {
                wire_schema: _,
                heartbeat_ms,
                lease_ttl_ms: _,
            }) => {
                // Derived, never transmitted: the same inputs the
                // coordinator used (secret, both nonces, worker id, and
                // the channel binding under mTLS).
                let key = cluster_wire::session_key(
                    secret,
                    &opts.worker_id,
                    &server_nonce,
                    &nonce,
                    binding.as_ref().map(|b| &b[..]),
                );
                Ok(Self {
                    stream,
                    opts,
                    heartbeat: Duration::from_millis(heartbeat_ms.max(1)),
                    key,
                })
            }
            Some(WireFrame::EnrollRefused { reason }) => Err(SqueezefsError::InvalidOperation(
                format!("enrollment refused by the coordinator: {reason}"),
            )),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "unexpected enrollment reply: {other:?}"
            ))),
        }
    }

    /// A dup of the connection socket for teardown: `shutdown(Both)`
    /// wakes the serve loop's parked frame wait (the mount-side fleet
    /// worker's disarm path — KD-MW-16). Best-effort like the internal
    /// nudge; `None` on transports without a raw handle.
    pub fn teardown_nudge(&self) -> Option<std::net::TcpStream> {
        self.stream.nudge_handle().ok()
    }

    /// Serve shards until the coordinator connection closes. Executes
    /// the task list with the duty-cycle throttle, re-validates the
    /// lease per batch (§5.1.6 rung 1), fills destinations through
    /// `seam`, and submits fencing-stamped result proposals.
    pub async fn run(self, seam: Arc<dyn ShardDeviceSeam>) -> Result<WorkerReport> {
        let JobWireWorker {
            stream,
            opts,
            heartbeat,
            key,
        } = self;
        // The enrollment dial installed a 10 s socket read timeout; an
        // enrolled worker legitimately waits UNBOUNDED between frames
        // (assignments arrive on job cadence), so clear it — teardown is
        // the nudge below, and the coordinator's EOF still ends the read.
        stream
            .set_read_timeout(None)
            .map_err(SqueezefsError::from)?;
        // Teardown nudge: `shutdown(Both)` on the dup wakes the read
        // thread out of its (unbounded) frame wait.
        let nudge = stream.nudge_handle().ok();
        let (mut rd, wr) = stream.split().map_err(SqueezefsError::from)?;
        // Both directions authenticated from here (the peer half of the
        // coordinator's framers).
        let (tx, mut rx) = session_framers(&key, cluster_wire::Role::Peer);
        let writer = Arc::new(parking_lot::Mutex::new(AuthedWriter { half: wr, tx }));

        let (assign_tx, mut assign_rx) = sqz_channel::mpsc::channel::<ShardDescriptor>(4);
        let (resp_tx, mut resp_rx) =
            sqz_channel::mpsc::channel::<std::result::Result<(), String>>(4);
        // The worker-side lease clock: latest deadline estimate,
        // extended by every HeartbeatAck.
        let (lease_tx, lease_rx) = sqz_channel::watch::channel(Instant::now());

        let stop = Arc::new(AtomicBool::new(false));

        // Read loop: route inbound frames, on its own named OS thread
        // (the frame reads are blocking socket I/O). The teardown guard
        // ties it to THIS future's completion: the stop latch plus the
        // socket nudge replace the old JoinHandle::abort, and both stream
        // halves drop with the threads, so the coordinator observes the
        // departure EOF.
        let read_thread = std::thread::Builder::new()
            .name("sqz-jw-rd".to_string())
            .spawn(move || loop {
                match rx.recv::<_, WireFrame>(&mut rd, MAX_FRAME_BYTES, None) {
                    Ok(Some(WireFrame::ShardAssign { shard })) => {
                        if sqz_blocking::block_on(assign_tx.send(shard)).is_err() {
                            return;
                        }
                    }
                    Ok(Some(WireFrame::HeartbeatAck { lease_ttl_ms })) => {
                        lease_tx.send(Instant::now() + Duration::from_millis(lease_ttl_ms));
                    }
                    Ok(Some(WireFrame::ResultAck { .. })) => {
                        if sqz_blocking::block_on(resp_tx.send(Ok(()))).is_err() {
                            return;
                        }
                    }
                    Ok(Some(WireFrame::ResultRefused { reason, .. })) => {
                        if sqz_blocking::block_on(resp_tx.send(Err(reason))).is_err() {
                            return;
                        }
                    }
                    Ok(Some(other)) => {
                        log::warn!("job worker: unexpected frame {other:?} — ignored");
                    }
                    Ok(None) | Err(_) => return,
                }
            })
            .map_err(SqueezefsError::from)?;

        // Heartbeat loop (10 s cadence by default; the coordinator's
        // EnrollOk sets it). Sleeps in bounded slices so the stop latch
        // is observed promptly. The test hook models a partition: no
        // heartbeats at all.
        let hb_writer = Arc::clone(&writer);
        let hb_opts = opts.clone();
        let hb_stop = Arc::clone(&stop);
        let hb_thread = std::thread::Builder::new()
            .name("sqz-jw-hb".to_string())
            .spawn(move || loop {
                let mut waited = Duration::ZERO;
                while waited < heartbeat {
                    if hb_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let slice = (heartbeat - waited).min(Duration::from_millis(100));
                    std::thread::sleep(slice);
                    waited += slice;
                }
                if hb_stop.load(Ordering::SeqCst) {
                    return;
                }
                if !hb_opts.heartbeats.load(Ordering::SeqCst) {
                    continue;
                }
                let mut w = hb_writer.lock();
                if w.send(&WireFrame::Heartbeat {
                    worker_id: hb_opts.worker_id.clone(),
                })
                .is_err()
                {
                    return;
                }
            })
            .map_err(SqueezefsError::from)?;

        let teardown = WorkerTeardown {
            stop,
            nudge,
            threads: vec![read_thread, hb_thread],
        };

        let mut report = WorkerReport::default();
        'shards: while let Some(shard) = assign_rx.recv().await {
            let ttl = Duration::from_millis(shard.lease_ttl_ms.max(1));
            // Local lease clock (rung 1): the assignment starts a full
            // TTL; HeartbeatAcks extend the watch.
            let assigned_deadline = Instant::now() + ttl;

            let mut lease_rx = lease_rx.clone();

            // KD-MW-16: fleet READ shards — no destinations, no Noop
            // task list. The seam executes the residue on the blocking
            // lane (heartbeats keep the lease renewed underneath) and
            // the result is proposed fencing-stamped like every
            // proposal; a seam refusal ABANDONS the shard loudly so the
            // coordinator re-leases now instead of at the TTL.
            if let Some((k, n)) = shard.fleet {
                if !revalidate_lease(&mut lease_rx, assigned_deadline, &writer, &opts, ttl).await {
                    report.shards_aborted += 1;
                    continue 'shards;
                }
                let seam2 = Arc::clone(&seam);
                let jt = shard.job_type.clone();
                let spec = FleetShardSpec {
                    k,
                    n,
                    throttle_pct: shard.throttle_pct,
                    inode_plane: shard.inode_plane,
                };
                let outcome =
                    sqz_blocking::run_blocking(move || seam2.run_fleet_shard(&jt, spec)).await;
                match outcome {
                    Ok(payload) => {
                        // The pause-before-submit test hook (the zombie
                        // window the fencing check exists for).
                        while opts.hold_submission.load(Ordering::SeqCst) {
                            sqz_time::sleep(Duration::from_millis(20)).await;
                        }
                        {
                            let mut w = writer.lock();
                            if w.send(&WireFrame::ReadShardResult {
                                job_id: shard.job_id.clone(),
                                shard: shard.shard,
                                shard_fencing: shard.shard_fencing,
                                payload,
                            })
                            .is_err()
                            {
                                break 'shards;
                            }
                        }
                        match resp_rx.recv().await {
                            Some(Ok(())) => {
                                report.shards_completed += 1;
                                METRICS
                                    .job_fleet_worker_shards
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            Some(Err(reason)) => {
                                report.submissions_refused += 1;
                                log::warn!(
                                    "job worker: fleet shard {}:{} proposal refused: {reason}",
                                    shard.job_id,
                                    shard.shard
                                );
                            }
                            None => break 'shards,
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "job worker: fleet shard {}:{} refused locally ({e}) — \
                             abandoning (the coordinator re-leases now)",
                            shard.job_id,
                            shard.shard
                        );
                        report.shards_aborted += 1;
                        let mut w = writer.lock();
                        if w.send(&WireFrame::ShardAbandon {
                            job_id: shard.job_id.clone(),
                            shard: shard.shard,
                            shard_fencing: shard.shard_fencing,
                            reason: e.to_string(),
                        })
                        .is_err()
                        {
                            break 'shards;
                        }
                    }
                }
                continue 'shards;
            }

            let mut aborted = false;

            // 1. Fill the pre-allocated destinations in re-validated
            //    batches (the DMA phase of the mover shape).
            let mut checksums = Vec::with_capacity(shard.destinations.len());
            'fill: for (chunk_no, chunk) in shard.destinations.chunks(REVALIDATE_BATCH).enumerate()
            {
                if !revalidate_lease(&mut lease_rx, assigned_deadline, &writer, &opts, ttl).await {
                    aborted = true;
                    break 'fill;
                }
                for (i, dest) in chunk.iter().enumerate() {
                    let dest_idx = chunk_no * REVALIDATE_BATCH + i;
                    // The copy step (VL4): a shard carrying source keys
                    // is a mover-shaped copy — fill each destination
                    // with the matching source's bytes read off shared
                    // storage. Keyless shards keep the Noop pattern.
                    let data = match shard.source_keys.get(dest_idx) {
                        Some(key) => match seam.read_source(key) {
                            Ok(d) => d,
                            Err(e) => {
                                log::warn!(
                                    "job worker: source read of '{key}' failed: {e} — \
                                     aborting shard"
                                );
                                aborted = true;
                                break 'fill;
                            }
                        },
                        None => block_pattern(shard.shard_fencing, dest, shard.block_len as usize),
                    };
                    if let Err(e) = seam.write_block(dest, &data) {
                        log::warn!("job worker: destination write failed: {e} — aborting shard");
                        aborted = true;
                        break 'fill;
                    }
                    checksums.push(BlockChecksum {
                        dest: dest.clone(),
                        len: data.len() as u32,
                        xxh3: xxh3_64(&data),
                    });
                }
            }

            // 2. The task list, throttled (KD-3), re-validated per batch.
            if !aborted {
                match &shard.job_type {
                    JobType::Noop { tasks, task_ms } => {
                        let mut remaining = *tasks;
                        while remaining > 0 {
                            if !revalidate_lease(
                                &mut lease_rx,
                                assigned_deadline,
                                &writer,
                                &opts,
                                ttl,
                            )
                            .await
                            {
                                aborted = true;
                                break;
                            }
                            let batch = remaining.min(REVALIDATE_BATCH as u64);
                            for _ in 0..batch {
                                let start = Instant::now();
                                sqz_time::sleep(Duration::from_millis(*task_ms)).await;
                                if let Some(delay) =
                                    job_throttle_sleep(start.elapsed(), shard.throttle_pct)
                                {
                                    sqz_time::sleep(delay).await;
                                }
                            }
                            remaining -= batch;
                        }
                    }
                    // The movers (and fsck, VL6a) never reach the wire in
                    // v1.1 (`JobType::wire_executable` gates the
                    // dispatcher); an assigned shard of these is a
                    // coordinator bug — abort loudly, never
                    // fake-complete it.
                    JobType::EvacuateVolume { .. }
                    | JobType::Rebalance
                    | JobType::MigrateMetaSlot { .. }
                    | JobType::Fsck { .. }
                    | JobType::DefragData { .. }
                    | JobType::DefragPack { .. }
                    | JobType::DefragMeta
                    | JobType::DefragFold
                    | JobType::KvmapSweep { .. } => {
                        log::error!(
                            "job worker: mover shard {}:{} reached the wire — the \
                             dispatcher must not assign mover job types (v1.1); aborting",
                            shard.job_id,
                            shard.shard
                        );
                        aborted = true;
                    }
                }
            }

            if aborted {
                report.shards_aborted += 1;
                log::warn!(
                    "job worker: shard {}:{} abandoned by lease re-validation (the lease \
                     is gone — no submission)",
                    shard.job_id,
                    shard.shard
                );
                continue 'shards;
            }

            // The pause-before-submit test hook (the zombie window the
            // fencing check exists for).
            while opts.hold_submission.load(Ordering::SeqCst) {
                sqz_time::sleep(Duration::from_millis(20)).await;
            }

            {
                let mut w = writer.lock();
                if w.send(&WireFrame::ResultSubmit {
                    job_id: shard.job_id.clone(),
                    shard: shard.shard,
                    shard_fencing: shard.shard_fencing,
                    checksums,
                })
                .is_err()
                {
                    break 'shards;
                }
            }
            match resp_rx.recv().await {
                Some(Ok(())) => {
                    report.shards_completed += 1;
                    opts.acks_received
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Some(Err(reason)) => {
                    report.submissions_refused += 1;
                    log::warn!(
                        "job worker: submission of {}:{} refused: {reason}",
                        shard.job_id,
                        shard.shard
                    );
                }
                None => break 'shards,
            }
        }

        // Drop the routing channels FIRST (a read thread parked in a
        // full channel's send must observe the closed receiver, never
        // the teardown join), then the teardown guard stops, nudges and
        // joins the threads — dropping both stream halves with them
        // closes the connection, so the coordinator sees the departure.
        drop(assign_rx);
        drop(resp_rx);
        drop(teardown);
        Ok(report)
    }
}

/// Ties the worker's spawned threads to `run()`'s lifetime (no
/// fire-and-forget leaks — AGENTS structured-concurrency posture).
/// Dropping the guard latches the stop flag, nudges the socket
/// (`shutdown(Both)` wakes the read thread's parked frame wait — the
/// `JoinHandle::abort` replacement), and joins both threads, which
/// releases the stream halves they own.
struct WorkerTeardown {
    stop: Arc<AtomicBool>,
    nudge: Option<std::net::TcpStream>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for WorkerTeardown {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(n) = &self.nudge {
            let _ = n.shutdown(Shutdown::Both);
        }
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

/// Per-batch lease re-validation (§5.1.6 rung 1): a worker checks its
/// local expiry clock before every batch — expired ⇒ abort (`false`);
/// past half-TTL with heartbeating alive ⇒ one wire round-trip before
/// proceeding (bounded wait for the ack).
async fn revalidate_lease(
    lease_rx: &mut sqz_channel::watch::Receiver<Instant>,
    assigned_deadline: Instant,
    writer: &Arc<parking_lot::Mutex<AuthedWriter>>,
    opts: &WorkerOptions,
    ttl: Duration,
) -> bool {
    let deadline =
        |rx: &sqz_channel::watch::Receiver<Instant>| (*rx.borrow()).max(assigned_deadline);
    let now = Instant::now();
    let d = deadline(lease_rx);
    if now >= d {
        return false;
    }
    if opts.heartbeats.load(Ordering::SeqCst) && now + ttl / 2 >= d {
        // Wire round-trip: heartbeat now and wait (bounded) for the ack
        // to move the deadline before committing the next batch.
        {
            let mut w = writer.lock();
            if w.send(&WireFrame::Heartbeat {
                worker_id: opts.worker_id.clone(),
            })
            .is_err()
            {
                return false;
            }
        }
        let _ = sqz_time::timeout(ttl / 4, lease_rx.changed()).await;
        return Instant::now() < deadline(lease_rx);
    }
    true
}

/// Deterministic destination fill for the Noop vehicle (VL4 movers copy
/// real blocks; the wire only needs bytes whose checksums round-trip).
fn block_pattern(fencing: u64, dest: &DestTuple, len: usize) -> Vec<u8> {
    let seed = fencing ^ dest.offset ^ u64::from(dest.backend_id);
    (0..len)
        .map(|i| (seed.wrapping_add(i as u64) & 0xff) as u8)
        .collect()
}
