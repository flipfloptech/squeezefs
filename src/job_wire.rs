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
//! - **Transport**: length-prefixed schema-versioned frames
//!   ([`WIRE_SCHEMA`]) over tokio TCP; TLS via **tokio-rustls** reusing
//!   `ClusterSecurityConfig`'s cert/CA/verifier construction
//!   (`tiering::cluster_tls`). Without a security config the listener runs
//!   **plaintext** (OQ-A default-permissive) with ONE loud log line at
//!   start. Network TCP/TLS is the sanctioned non-uring exception
//!   (AGENTS "Not uring" row).
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
//!   Exclusive – Registrants Only** (rtype 2) on the data namespaces at
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
//! # VAL-6 — the INTERIM hardening of an open port (pre-RC, P0)
//!
//! Execution-plan ruling **D2**: the listener stays **configurable and
//! default-bound `0.0.0.0`** with auto-discovered peers, so the fix is
//! not "close the port" — it is "make the open port safe" until stage
//! **S3 `cluster_wire`** lands the zero-config mutual-authn redesign.
//! What this module now enforces on attacker-reachable input:
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
//!   `continue` turned `EMFILE` into a busy loop), and per-connection
//!   `JoinHandle` **pruning** (RES-5: `handles` was push-only).
//! - **Enrollment freshness**: the coordinator speaks first with a
//!   [`WireFrame::Challenge`]; its nonce is **single-use** (replay
//!   registry) inside a **freshness window**
//!   (`JobWireConfig::enroll_freshness`).
//! - **The verification-strength ladder keys on an AUTHENTICATED
//!   channel** — CA-pinned mTLS ([`channel_authenticated`]), never on
//!   the presence of a TLS object. A `ClusterSecurityConfig` with no CA
//!   installs an accept-everything verifier client-side and
//!   `with_no_client_auth()` server-side: it is refused for the ladder
//!   and pinned to the plaintext class (mandatory-100 % verify-reads).
//! - **A configuration surface that exists**: [`JobWireConfig::from_env`]
//!   (`SQUEEZEFS_JOB_WIRE_*`) — `security` used to be hardwired `None`
//!   with no flag, env var, or config field able to populate it, so the
//!   listener's own warning recommended a configuration the binary
//!   could not express.
//!
//! **Deliberately left to S3 `cluster_wire`** (not fixable inside this
//! transport's shape): per-frame authentication after enrollment, a
//! session key derived from the storage secret (TLS-PSK or an
//! exporter-bound per-frame MAC), deletion of the accept-everything
//! verifier itself (it lives in `tiering::dht`, shared with the DHT),
//! the literal `"localhost"` server name, and peer auto-discovery
//! (DISC-1).

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::jobs::{job_throttle_sleep, JobCtl, JobFabric, JobType};
use crate::meta_backend::reservation::{register_ladder, resolve_for_mount, ReservationClient};
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use crate::tiering::cluster_tls::{
    rustls_client_config, rustls_server_config, ClusterSecurityConfig,
};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use xxhash_rust::xxh3::xxh3_64;

/// The wire frame schema this build speaks. A hello carrying any other
/// value refuses loud, naming the field.
///
/// `2` = the VAL-6 challenge handshake (coordinator-issued single-use
/// nonce). A schema-1 worker's self-chosen-nonce hello is refused loud
/// rather than admitted on a replayable proof.
pub const WIRE_SCHEMA: u32 = 2;

/// The per-fabric enrollment-secret record on ino 1 (`job:` prefix ⇒
/// behind the VL2 reserved-namespace FUSE screen; readable only through
/// the meta backend — i.e. by principals that already hold storage).
pub const JOB_ENROLL_XATTR: &str = "job:enroll";

/// Default shard-lease TTL (§5.1.6: 30 s).
pub const LEASE_TTL: Duration = Duration::from_secs(30);
/// Default worker heartbeat cadence (§5.1.6: 10 s).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Frame size cap — shard descriptors and heartbeats are KB-scale
/// (blocks move over shared storage, not the wire); anything larger is
/// a protocol violation, refused loud. This is the **post-enrollment**
/// class; see [`MAX_HELLO_FRAME_BYTES`] for what an unauthenticated
/// peer gets to spend.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// The **pre-enrollment** frame class cap (VAL-6). A `Challenge`/
/// `Enroll` pair is a few hundred bytes; nothing an unauthenticated
/// peer sends legitimately approaches this, and the body is streamed
/// in [`FRAME_CHUNK_BYTES`] rounds anyway.
pub const MAX_HELLO_FRAME_BYTES: u32 = 8 * 1024;

/// Frame bodies are committed to memory this much at a time (VAL-6).
/// The length prefix is a *claim*, not an allocation authority: a peer
/// that declares [`MAX_FRAME_BYTES`] and sends nothing costs one chunk.
pub const FRAME_CHUNK_BYTES: usize = 64 * 1024;

/// First rung of the accept-error backoff ladder (VAL-6: the old arm
/// `continue`d, so a persistent `EMFILE`/`ENFILE` spun a core).
pub const ACCEPT_BACKOFF_START: Duration = Duration::from_millis(5);

/// Backoff ceiling — small enough that a transient fd exhaustion
/// recovers promptly once the pressure lifts.
pub const ACCEPT_BACKOFF_MAX: Duration = Duration::from_millis(1000);

/// The accept-error backoff ladder: doubling from [`ACCEPT_BACKOFF_START`],
/// saturating at [`ACCEPT_BACKOFF_MAX`]. `None` = the first error after a
/// successful accept.
pub fn next_accept_backoff(prev: Option<Duration>) -> Duration {
    match prev {
        None => ACCEPT_BACKOFF_START,
        Some(d) => (d.saturating_mul(2)).min(ACCEPT_BACKOFF_MAX),
    }
}

/// Worker-side bound on the enrollment exchange (dial → challenge →
/// hello → reply). The coordinator's own gate is
/// `JobWireConfig::handshake_timeout`.
const ENROLL_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on an enrolling peer's self-declared identity — it lands in log
/// lines and the durable shard records, so it is bounded like every
/// other attacker-chosen field.
const MAX_WORKER_ID_BYTES: usize = 256;

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
}

/// The §5.1.6 wire frames (`wire_schema: 1`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "frame", rename_all = "snake_case")]
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
}

/// Write one length-prefixed JSON frame (u32-BE length prefix).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &WireFrame,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    if body.len() as u32 > MAX_FRAME_BYTES {
        return Err(std::io::Error::other("frame exceeds MAX_FRAME_BYTES"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Read one frame at the post-enrollment class ([`MAX_FRAME_BYTES`], no
/// body deadline); `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<WireFrame>> {
    read_frame_limited(r, MAX_FRAME_BYTES, None).await
}

/// Read one frame under an explicit **class cap** and optional **body
/// deadline** (VAL-6).
///
/// Two properties the caller gets that the old `read_frame` did not:
///
/// 1. The length prefix is a claim, never an allocation authority — the
///    body Vec grows [`FRAME_CHUNK_BYTES`] at a time *as bytes arrive*,
///    so a peer that declares 16 MiB and sends nothing costs one chunk.
/// 2. Once a body has started, `body_timeout` bounds the WHOLE body (a
///    dribbling peer is an error, not a parked task). The length-prefix
///    read itself is deliberately unbounded here: an idle enrolled
///    session legitimately waits between frames, and its idle bound is
///    the session-level deadline.
pub async fn read_frame_limited<R: AsyncRead + Unpin>(
    r: &mut R,
    max_len: u32,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<WireFrame>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > max_len {
        return Err(std::io::Error::other(format!(
            "frame length {len} exceeds the {max_len} B cap"
        )));
    }
    let body = match body_timeout {
        Some(d) => tokio::time::timeout(d, read_body_chunked(r, len as usize))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("frame body of {len} B did not arrive within {d:?}"),
                )
            })??,
        None => read_body_chunked(r, len as usize).await?,
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::other(format!("undecodable frame: {e}")))
}

/// Commit body memory only as it arrives: at most one
/// [`FRAME_CHUNK_BYTES`] round is outstanding ahead of the peer.
async fn read_body_chunked<R: AsyncRead + Unpin>(
    r: &mut R,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::with_capacity(len.min(FRAME_CHUNK_BYTES));
    while body.len() < len {
        let want = (len - body.len()).min(FRAME_CHUNK_BYTES);
        let start = body.len();
        body.resize(start + want, 0);
        r.read_exact(&mut body[start..]).await?;
    }
    Ok(body)
}

/// The enrollment proof: hex `HMAC-SHA256(secret, worker_id ‖
/// server_nonce ‖ endpoint_nonce ‖ "hello")` — computable only by a
/// principal that can read the meta volume's `job:enroll` record, and
/// (VAL-6) bound to the coordinator's single-use challenge, so a
/// captured proof is not a reusable credential.
pub fn enroll_hmac(
    secret: &[u8],
    worker_id: &str,
    server_nonce: &str,
    endpoint_nonce: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(worker_id.as_bytes());
    mac.update(server_nonce.as_bytes());
    mac.update(endpoint_nonce.as_bytes());
    mac.update(b"hello");
    hex_encode(&mac.finalize().into_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Constant-time-ish comparison for the enrollment proof (both sides
/// are fixed-length hex MACs; XOR-accumulate, never short-circuit).
fn mac_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
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
/// call it inline); this implementation hops onto the captured runtime
/// via `block_in_place` — it therefore requires the multi-thread
/// runtime, which every mount and offline coordinator runs.
pub struct RouterShardDevice {
    router: Arc<crate::routing::BackendRouter>,
    backend_ids: Vec<String>,
    block_len: usize,
    rt: tokio::runtime::Handle,
    /// PR VL6a: coordinator-pre-allocated shard destinations are
    /// unpublished BY DESIGN for the whole shard (and quarantined
    /// destinations until job end) — their live-owner registrations in
    /// the fsck in-flight registry live with the seam (dropped when the
    /// job's seam is torn down, alongside quarantine reclaim).
    inflight: parking_lot::Mutex<Vec<crate::block_allocator::InflightAllocGuard>>,
}

impl RouterShardDevice {
    /// Capture the router and its CURRENT volume-id table (call from
    /// async context — mount wiring is).
    pub fn new(router: Arc<crate::routing::BackendRouter>, block_len: usize) -> Arc<Self> {
        let mut backend_ids: Vec<String> =
            router.backends.iter().map(|e| e.key().clone()).collect();
        backend_ids.sort();
        Arc::new(Self {
            router,
            backend_ids,
            block_len,
            rt: tokio::runtime::Handle::current(),
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
        tokio::task::block_in_place(|| self.rt.block_on(fut))
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
    hex_decode(hexs)
        .ok_or_else(|| SqueezefsError::InvalidOperation("job:enroll secret is not hex".into()))
}

/// The address a coordinator advertises in its mount registration: the
/// interface the default route would use (the UDP-connect trick — no
/// packet is sent), falling back to loopback on isolated boxes (where
/// same-host workers still reach it).
pub fn local_advertise_ip() -> std::net::IpAddr {
    std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|s| {
            s.connect(("192.0.2.1", 9))?; // TEST-NET-1: route lookup only
            Ok(s.local_addr()?.ip())
        })
        .unwrap_or_else(|_| std::net::IpAddr::from([127, 0, 0, 1]))
}

/// Discover the live coordinator's job endpoint from the mount
/// registrations (the `job_endpoint` additive field on the coordinator's
/// `client:{id}` heartbeat record). Read-only; safe on probe backends.
pub async fn discover_endpoint(meta: &Arc<RoutedMetaBackend>) -> Option<String> {
    for vol in &meta.volumes {
        for reg in vol.mount_registrations().await {
            if reg.kind == "client" && reg.heartbeat_fresh {
                if let Some(ep) = reg.job_endpoint {
                    return Some(ep);
                }
            }
        }
    }
    None
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
    /// TLS via tokio-rustls when set (the `ClusterSecurityConfig`
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
            Some(_) => "tls-unauthenticated",
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

/// Default concurrent-connection cap: derived from the core count (the
/// AGENTS "resource caps derive from system resources" law), floored so
/// a small box still admits a real worker population and ceilinged so a
/// large one still has a bound.
pub fn default_max_connections() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .saturating_mul(16)
        .clamp(64, 1024)
}

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

/// Is this security configuration an **authenticated** channel?
///
/// CA-pinned mTLS only: the server installs a `WebPkiClientVerifier`
/// rooted at the CA and the client validates against the same root.
/// Without a CA the client installs an accept-everything verifier via
/// `.dangerous()` and the server takes `with_no_client_auth()` — a TLS
/// object, not authentication. The verification-strength ladder keys on
/// THIS, never on the presence of a TLS object (VAL-6).
///
/// The key half is load-bearing, not decorative: the node cert the
/// cluster machinery presents is signed by the CA key, so a CA cert
/// without its key cannot produce an authenticated channel at all.
pub fn channel_authenticated(security: &ClusterSecurityConfig) -> bool {
    security.ca_cert.is_some() && security.ca_key.is_some()
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

/// A type-erased duplex stream (plaintext TCP or TLS).
trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}
type BoxedStream = Box<dyn Duplex>;

/// One enrolled worker session.
struct Session {
    id: u64,
    worker_id: String,
    pr_key: Option<u64>,
    writer: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<BoxedStream>>>,
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
    lease_expiry: tokio::time::Instant,
}

/// Coordinator-side shard state (persisted as `job:{id}:shard:{k}` on
/// the KD-2 record plane; this is the live truth between checkpoints).
struct ShardState {
    job_id: String,
    shard: u32,
    ctl: Arc<JobCtl>,
    fencing: AtomicU64,
    holder: parking_lot::Mutex<Option<ShardHolder>>,
    destinations: parking_lot::Mutex<Vec<DestTuple>>,
    done: AtomicBool,
}

/// The coordinator's WERO hold on the shared data namespaces.
struct WeroFence {
    key: u64,
    holds: Vec<Arc<dyn ReservationClient>>,
}

/// Outcome of consuming a challenge nonce (VAL-6 freshness).
enum NonceOutcome {
    /// Issued by this coordinator, inside its window, first use.
    Fresh,
    /// Issued, but the freshness window has closed.
    Expired,
    /// Never issued, or already used — a replay.
    UnknownOrReplayed,
}

/// The challenge registry: single-use nonces inside a freshness window,
/// bounded in memory (an unauthenticated peer can only make the
/// coordinator hold `cap` of these, and the connection cap bounds the
/// rate at which it can try).
struct NonceRegistry {
    issued: HashMap<String, tokio::time::Instant>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}

impl NonceRegistry {
    fn new(cap: usize) -> Self {
        Self {
            issued: HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn issue(&mut self, freshness: Duration) -> String {
        let now = tokio::time::Instant::now();
        self.prune(now, freshness);
        let nonce = uuid::Uuid::new_v4().to_string();
        self.issued.insert(nonce.clone(), now);
        self.order.push_back(nonce.clone());
        nonce
    }

    /// Single use: a fresh nonce is REMOVED as it is accepted, so the
    /// second presentation of the same hello is a replay.
    fn consume(&mut self, nonce: &str, freshness: Duration) -> NonceOutcome {
        let now = tokio::time::Instant::now();
        match self.issued.remove(nonce) {
            Some(issued) if now.duration_since(issued) <= freshness => NonceOutcome::Fresh,
            Some(_) => NonceOutcome::Expired,
            None => NonceOutcome::UnknownOrReplayed,
        }
    }

    fn outstanding(&self) -> usize {
        self.issued.len()
    }

    fn prune(&mut self, now: tokio::time::Instant, freshness: Duration) {
        while let Some(front) = self.order.front() {
            let stale = self
                .issued
                .get(front)
                .is_none_or(|t| now.duration_since(*t) > freshness);
            let over_cap = self.issued.len() > self.cap;
            if stale || over_cap {
                if let Some(n) = self.order.pop_front() {
                    self.issued.remove(&n);
                }
            } else {
                break;
            }
        }
    }
}

/// One admitted connection's slot in the [`JobWireConfig::max_connections`]
/// budget: RAII, so every exit path (TLS failure, hello timeout, clean
/// departure, panic) returns it.
struct ConnPermit {
    live: Arc<AtomicU64>,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The §5.1.6 wire host: TCP(/TLS) listener + dispatcher + lease
/// sweeper on the job-fabric coordinator.
pub struct JobWireHost {
    fabric: Arc<JobFabric>,
    seam: Arc<dyn ShardDeviceSeam>,
    cfg: JobWireConfig,
    endpoint: SocketAddr,
    secret: Vec<u8>,
    transport: &'static str,
    /// CA-pinned mTLS — the predicate the verification-strength ladder
    /// keys on (VAL-6), never `transport == "tls"`.
    authenticated: bool,
    verify_permille: u32,
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// `false` when the posture disabled the listener entirely.
    listening: bool,
    /// Resolved idle bound for an enrolled session.
    session_idle: Duration,
    /// VAL-6 challenge registry (single-use nonces, freshness-windowed).
    nonces: parking_lot::Mutex<NonceRegistry>,
    /// Connection accounting: the cap gauge, its refusals, and the
    /// accept-error backoff engagement counter (must stay 0 on a
    /// healthy host).
    live_conns: Arc<AtomicU64>,
    conns_refused: AtomicU64,
    accept_backoffs: AtomicU64,
    next_session: AtomicU64,
    sessions: parking_lot::Mutex<HashMap<u64, Arc<Session>>>,
    shards: parking_lot::Mutex<HashMap<String, Arc<ShardState>>>,
    quarantine: parking_lot::Mutex<BTreeSet<DestTuple>>,
    /// WERO fence, held first-enrollment → last-departure. The tokio
    /// mutex serializes acquire/release transitions.
    fence: tokio::sync::Mutex<Option<WeroFence>>,
    /// Guarantee class: true = `pr` (WERO held on EVERY configured data
    /// namespace).
    pr_mode: AtomicBool,
    shutdown: AtomicBool,
    handles: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
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
        // the reserved-namespace screen.
        let mut secret = vec![0u8; 32];
        rand::Rng::fill(&mut rand::thread_rng(), &mut secret[..]);
        let record = serde_json::json!({ "schema": 1, "secret": hex_encode(&secret) });
        fabric
            .meta_handle()
            .setxattr(1, JOB_ENROLL_XATTR, record.to_string().as_bytes())
            .await?;

        // The verification-strength ladder keys on an AUTHENTICATED
        // channel (VAL-6), never on the presence of a TLS object.
        let (tls, transport, authenticated) = match cfg.security.as_ref() {
            Some(sec) => {
                if sec.ca_cert.is_some() && sec.ca_key.is_none() {
                    // `rustls_{server,client}_config` unwrap the CA key
                    // whenever a cert is present — refuse here, loud,
                    // instead of panicking on the first connection.
                    return Err(SqueezefsError::InvalidOperation(
                        "job wire: ClusterSecurityConfig carries a CA cert with no ca_key — \
                         the node certificate is signed by the CA key, so this configuration \
                         can never produce an authenticated channel"
                            .into(),
                    ));
                }
                let server_cfg = rustls_server_config(sec)?;
                let authed = channel_authenticated(sec);
                (
                    Some(tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg))),
                    if authed {
                        "mtls"
                    } else {
                        "tls-unauthenticated"
                    },
                    authed,
                )
            }
            None => (None, "plaintext", false),
        };
        // The Issue-30 law, restated on the authenticated-channel
        // predicate: anything but CA-pinned mTLS is plaintext-class.
        let verify_permille = if authenticated {
            cfg.verify_sample_permille.clamp(1, 1000)
        } else {
            1000
        };

        let (listener, endpoint) = if cfg.enabled {
            let l = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
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
                // OQ-A default-permissive: the ONE loud line. HMAC gates
                // enrollment (integrity of enrollment, not of frames);
                // the Issue-30 law makes every mutating publish 100 %
                // verify-read, so a hijacked session cannot publish bytes
                // the coordinator has not itself read and checksummed.
                "plaintext" => log::warn!(
                    "job wire: listener {endpoint} is PLAINTEXT TCP (no ClusterSecurityConfig) \
                     — enrollment is challenge-HMAC-gated only; mutating publishes pay \
                     mandatory-100 % verify-reads (Issue-30; ≈2× device reads on \
                     remote-mutated bytes). Set SQUEEZEFS_JOB_WIRE_CA_KEY (+ optional \
                     SQUEEZEFS_JOB_WIRE_CA_CERT) for CA-pinned mTLS + sampled verification, \
                     or SQUEEZEFS_JOB_WIRE_BIND to narrow/disable the listener."
                ),
                "tls-unauthenticated" => log::warn!(
                    "job wire: listener {endpoint} runs TLS with NO CA pin — the client side \
                     installs an accept-everything certificate verifier and the server takes \
                     no client auth, so this is an UNAUTHENTICATED channel: it is refused for \
                     the verification-strength ladder and pinned to mandatory-100 % \
                     verify-reads. Set SQUEEZEFS_JOB_WIRE_CA_KEY for CA-pinned mTLS."
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
            secret,
            transport,
            authenticated,
            verify_permille,
            tls,
            listening: listener.is_some(),
            session_idle,
            nonces: parking_lot::Mutex::new(NonceRegistry::new(nonce_cap)),
            live_conns: Arc::new(AtomicU64::new(0)),
            conns_refused: AtomicU64::new(0),
            accept_backoffs: AtomicU64::new(0),
            next_session: AtomicU64::new(1),
            sessions: parking_lot::Mutex::new(HashMap::new()),
            shards: parking_lot::Mutex::new(HashMap::new()),
            quarantine: parking_lot::Mutex::new(BTreeSet::new()),
            fence: tokio::sync::Mutex::new(None),
            pr_mode: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            handles: parking_lot::Mutex::new(Vec::new()),
            cfg,
        });

        let mut core = Vec::with_capacity(3);
        if let Some(listener) = listener {
            core.push(tokio::spawn(Self::accept_loop(Arc::clone(&host), listener)));
        }
        core.push(tokio::spawn(Self::dispatcher_loop(Arc::clone(&host))));
        core.push(tokio::spawn(Self::sweeper_loop(Arc::clone(&host))));
        host.handles.lock().extend(core);
        Ok(host)
    }

    /// The bound listener address.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// `"plaintext"`, `"tls-unauthenticated"` (a TLS object with no CA
    /// pin — accept-everything verifier, no client auth), or `"mtls"`
    /// (CA-pinned, the only authenticated class).
    pub fn transport_mode(&self) -> &'static str {
        self.transport
    }

    /// Is the channel AUTHENTICATED (CA-pinned mTLS)? The
    /// verification-strength ladder keys on this — never on the
    /// presence of a TLS object (VAL-6).
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
        self.live_conns.load(Ordering::SeqCst)
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
    /// number of connections ever accepted).
    pub fn retained_task_handles(&self) -> usize {
        self.handles.lock().len()
    }

    /// Outstanding (issued, unanswered, unexpired) enrollment challenges.
    pub fn outstanding_challenges(&self) -> usize {
        self.nonces.lock().outstanding()
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

    /// Stop the wire: abort every task, drop every session (closing the
    /// worker connections — their `run()` futures resolve on the EOF),
    /// and release the WERO fence.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let handles: Vec<_> = std::mem::take(&mut *self.handles.lock());
        for h in handles {
            h.abort();
            let _ = h.await;
        }
        // Aborting a serve_conn task drops only its READ half; the
        // session map still owns the write half, which keeps the TCP
        // connection open and a remote worker's read loop parked. Drop
        // the sessions so workers observe EOF.
        self.sessions.lock().clear();
        self.release_fence().await;
    }

    // -- transport plumbing --------------------------------------------------

    async fn accept_loop(self: Arc<Self>, listener: tokio::net::TcpListener) {
        let mut backoff: Option<Duration> = None;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => {
                    backoff = None;
                    x
                }
                Err(e) => {
                    // VAL-6: the old arm `continue`d, so a persistent
                    // EMFILE/ENFILE condition span the accept loop at
                    // 100 % of a core. Back off, capped, and say so.
                    let d = next_accept_backoff(backoff);
                    backoff = Some(d);
                    self.accept_backoffs.fetch_add(1, Ordering::SeqCst);
                    log::warn!("job wire: accept failed: {e} — backing off {d:?}");
                    tokio::time::sleep(d).await;
                    continue;
                }
            };

            // The concurrent-connection cap (VAL-6). Claim the slot
            // BEFORE spawning anything: over-cap peers cost one accept
            // and one close, never a task or a buffer.
            let permit = match self.try_admit() {
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

            let host = Arc::clone(&self);
            let handshake = self.cfg.handshake_timeout;
            let h = tokio::spawn(async move {
                let _permit = permit;
                let stream: BoxedStream = match host.tls.clone() {
                    // The handshake itself is attacker-paced: bound it.
                    Some(acceptor) => {
                        match tokio::time::timeout(handshake, acceptor.accept(tcp)).await {
                            Ok(Ok(s)) => Box::new(s),
                            Ok(Err(e)) => {
                                log::warn!("job wire: TLS handshake with {peer} failed: {e}");
                                return;
                            }
                            Err(_) => {
                                log::warn!(
                                    "job wire: TLS handshake with {peer} exceeded {handshake:?} — \
                                 dropped"
                                );
                                return;
                            }
                        }
                    }
                    None => Box::new(tcp),
                };
                host.serve_conn(stream, peer).await;
            });
            // RES-5: `handles` was push-only — one JoinHandle retained
            // per connection ever accepted. Prune the finished ones on
            // every accept (the `admin_conns` pattern).
            {
                let mut handles = self.handles.lock();
                handles.retain(|h| !h.is_finished());
                handles.push(h);
            }
        }
    }

    /// Claim a connection slot, or `None` at the cap.
    fn try_admit(&self) -> Option<ConnPermit> {
        let cap = self.cfg.max_connections as u64;
        let mut live = self.live_conns.load(Ordering::SeqCst);
        loop {
            if live >= cap {
                return None;
            }
            match self.live_conns.compare_exchange_weak(
                live,
                live + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ConnPermit {
                        live: Arc::clone(&self.live_conns),
                    })
                }
                Err(observed) => live = observed,
            }
        }
    }

    /// One connection: the VAL-6 challenge, the enrollment gate, then
    /// the session frame loop. Everything before enrollment runs under
    /// `handshake_timeout` at the [`MAX_HELLO_FRAME_BYTES`] class.
    async fn serve_conn(self: &Arc<Self>, stream: BoxedStream, peer: SocketAddr) {
        let mut stream = stream;
        let deadline = self.cfg.handshake_timeout;

        // The coordinator speaks first: a single-use, freshness-windowed
        // nonce the worker cannot choose (VAL-6). A captured hello is
        // therefore not a credential.
        let server_nonce = self.nonces.lock().issue(self.cfg.enroll_freshness);
        let challenge = WireFrame::Challenge {
            wire_schema: WIRE_SCHEMA,
            server_nonce: server_nonce.clone(),
            freshness_ms: self.cfg.enroll_freshness.as_millis() as u64,
        };
        match tokio::time::timeout(deadline, write_frame(&mut stream, &challenge)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("job wire: {peer}: challenge write failed: {e}");
                return;
            }
            Err(_) => {
                log::warn!("job wire: {peer}: challenge write stalled past {deadline:?}");
                return;
            }
        }

        let hello = match tokio::time::timeout(
            deadline,
            read_frame_limited(&mut stream, MAX_HELLO_FRAME_BYTES, Some(deadline)),
        )
        .await
        {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                log::warn!("job wire: {peer}: undecodable hello: {e}");
                METRICS
                    .job_remote_enroll_refused
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(_) => {
                log::warn!("job wire: {peer}: no hello within {deadline:?} — dropped");
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
            )
            .await;
            return;
        };
        if wire_schema != WIRE_SCHEMA {
            METRICS
                .job_remote_enroll_refused
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: {peer}: worker {worker_id} speaks wire_schema {wire_schema}, \
                 this coordinator speaks {WIRE_SCHEMA} — refused"
            );
            let _ = write_frame(
                &mut stream,
                &WireFrame::EnrollRefused {
                    reason: format!(
                        "wire_schema {wire_schema} not supported (coordinator speaks {WIRE_SCHEMA})"
                    ),
                },
            )
            .await;
            return;
        }
        if worker_id.len() > MAX_WORKER_ID_BYTES {
            METRICS
                .job_remote_enroll_refused
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: {peer}: worker id of {} B exceeds the {MAX_WORKER_ID_BYTES} B cap \
                 — refused",
                worker_id.len()
            );
            let _ = write_frame(
                &mut stream,
                &WireFrame::EnrollRefused {
                    reason: format!("worker_id exceeds {MAX_WORKER_ID_BYTES} B"),
                },
            )
            .await;
            return;
        }
        let expected = enroll_hmac(&self.secret, &worker_id, &presented_nonce, &endpoint_nonce);
        if !mac_eq(&expected, &hmac) {
            METRICS
                .job_remote_enroll_refused
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: {peer}: worker {worker_id} failed the storage-membership HMAC — \
                 refused (job_remote_enroll_refused)"
            );
            let _ = write_frame(
                &mut stream,
                &WireFrame::EnrollRefused {
                    reason: "hmac invalid (storage-membership proof failed)".into(),
                },
            )
            .await;
            return;
        }
        // Freshness (VAL-6): the proof must answer a challenge THIS
        // coordinator issued, inside its window, and only once. A valid
        // MAC over a stale or already-spent nonce is a replay.
        let outcome = self
            .nonces
            .lock()
            .consume(&presented_nonce, self.cfg.enroll_freshness);
        let refusal = match outcome {
            NonceOutcome::Fresh => None,
            NonceOutcome::Expired => Some(format!(
                "enrollment challenge expired (freshness window {:?}) — reconnect",
                self.cfg.enroll_freshness
            )),
            NonceOutcome::UnknownOrReplayed => Some(
                "enrollment nonce is unknown or already spent (replayed hello) — challenges \
                 are single-use"
                    .to_string(),
            ),
        };
        if let Some(reason) = refusal {
            METRICS
                .job_remote_enroll_refused
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "job wire: {peer}: worker {worker_id} presented a bad challenge nonce: \
                 {reason} (job_remote_enroll_refused)"
            );
            let _ = write_frame(&mut stream, &WireFrame::EnrollRefused { reason }).await;
            return;
        }

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
        )
        .await
        {
            log::warn!("job wire: {peer}: EnrollOk write failed: {e}");
            self.on_worker_departed(None).await;
            return;
        }

        let (mut rd, wr) = tokio::io::split(stream);
        let session = Arc::new(Session {
            id: self.next_session.fetch_add(1, Ordering::SeqCst),
            worker_id: worker_id.clone(),
            pr_key,
            writer: Arc::new(tokio::sync::Mutex::new(wr)),
            busy: AtomicBool::new(false),
            expired: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .insert(session.id, Arc::clone(&session));
        log::info!(
            "job wire: worker {worker_id} enrolled from {peer} (session {}, pr_key {:?}, \
             fence {})",
            session.id,
            pr_key,
            self.fence_mode()
        );

        // Session frame loop. Post-enrollment classes: the full frame
        // cap, a body deadline once a length prefix lands, and an idle
        // bound (a session that stops speaking entirely is closed —
        // its slot is a capped resource).
        loop {
            let frame = match tokio::time::timeout(
                self.session_idle,
                read_frame_limited(&mut rd, MAX_FRAME_BYTES, Some(self.cfg.frame_body_timeout)),
            )
            .await
            {
                Ok(Ok(Some(f))) => f,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    log::warn!("job wire: session {} read error: {e}", session.id);
                    break;
                }
                Err(_) => {
                    log::warn!(
                        "job wire: session {} sent no frame within {:?} (heartbeat cadence \
                         {:?}) — closing the idle session",
                        session.id,
                        self.session_idle,
                        self.cfg.heartbeat_interval
                    );
                    break;
                }
            };
            match frame {
                WireFrame::Heartbeat { .. } => {
                    self.renew_lease(&session);
                    let mut w = session.writer.lock().await;
                    if write_frame(
                        &mut *w,
                        &WireFrame::HeartbeatAck {
                            lease_ttl_ms: self.cfg.lease_ttl.as_millis() as u64,
                        },
                    )
                    .await
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
                other => {
                    log::warn!(
                        "job wire: session {} sent an unexpected frame {other:?} — ignored",
                        session.id
                    );
                }
            }
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
        let acquired = tokio::task::spawn_blocking(move || acquire_wero(&paths)).await;
        match acquired {
            Ok(Some(f)) => {
                log::info!(
                    "job wire: WERO (rtype 2) acquired on {} data namespace(s) — guarantee \
                     class pr (expired worker hosts will be PR-preempted)",
                    f.holds.len()
                );
                *fence = Some(f);
                self.pr_mode.store(true, Ordering::SeqCst);
                METRICS.job_remote_fence_mode.store(1, Ordering::Relaxed);
            }
            Ok(None) => {
                log::warn!(
                    "job wire: data namespaces are not (all) PR-capable — guarantee class \
                     deferred-reclaim: quarantine reclaim defers to job end; the \
                     unbounded-pause zombie window is the documented residual class \
                     (design-volume-lifecycle §5.1.6 rung 3)"
                );
            }
            Err(e) => {
                log::warn!("job wire: WERO acquire task failed: {e} — deferred-reclaim");
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
            let _ = tokio::task::spawn_blocking(move || {
                for client in &f.holds {
                    if let Err(e) = client.release_registrants_only(f.key) {
                        log::warn!("job wire: WERO release failed: {e}");
                    }
                }
            })
            .await;
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
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            // `true`: the wire claims only wire-executable job types —
            // the VL4 movers stay on the local pool (their per-ino meta
            // publish is coordinator-local; see `JobType::wire_executable`).
            let Some((job_id, ctl)) = self.fabric.claim_next(true) else {
                let _ =
                    tokio::time::timeout(Duration::from_millis(250), self.fabric.work_notified())
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
            Arc::clone(shards.entry(job_id.to_string()).or_insert_with(|| {
                Arc::new(ShardState {
                    job_id: job_id.to_string(),
                    shard: 0,
                    ctl: Arc::clone(ctl),
                    fencing: AtomicU64::new(0),
                    holder: parking_lot::Mutex::new(None),
                    destinations: parking_lot::Mutex::new(Vec::new()),
                    done: AtomicBool::new(false),
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
            lease_expiry: tokio::time::Instant::now() + self.cfg.lease_ttl,
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
        };
        let mut w = session.writer.lock().await;
        write_frame(&mut *w, &WireFrame::ShardAssign { shard: descriptor })
            .await
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
                    h.lease_expiry = tokio::time::Instant::now() + self.cfg.lease_ttl;
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
            tokio::time::sleep(tick).await;
            let now = tokio::time::Instant::now();
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
        let old_dests = std::mem::take(&mut *shard.destinations.lock());
        if !old_dests.is_empty() {
            let mut q = self.quarantine.lock();
            for d in old_dests {
                if q.insert(d) {
                    METRICS
                        .job_remote_quarantined_destinations
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }

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
            // RES-16: take the fence's {key, holds} snapshot and RELEASE
            // the mutex before the blocking ioctl fan-out — the guard
            // used to be held across it, so one slow namespace stalled
            // every other fence transition (acquire at first enrollment,
            // release at last departure).
            let snapshot = {
                let fence = self.fence.lock().await;
                fence.as_ref().map(|f| (f.key, f.holds.clone()))
            };
            if let Some((key, holds)) = snapshot {
                let preempted = tokio::task::spawn_blocking(move || {
                    let mut n = 0u64;
                    for client in &holds {
                        match client.preempt_registrants_only(key, victim) {
                            Ok(()) => n += 1,
                            Err(e) => {
                                log::warn!("job wire: WERO preempt of key {victim:#x} failed: {e}")
                            }
                        }
                    }
                    n
                })
                .await
                .unwrap_or(0);
                if preempted > 0 {
                    METRICS
                        .job_remote_pr_preempts
                        .fetch_add(preempted, Ordering::Relaxed);
                    log::warn!(
                        "job wire: preempted expired worker host's PR registration \
                         {victim:#x} on {preempted} namespace(s) — its resumed DMA is \
                         device-rejected"
                    );
                }
            }
        }

        self.persist_shard_record(shard, "reclaimed").await;
        self.fabric.requeue_remote(&shard.job_id, &shard.ctl).await;
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
        let shard = self.shards.lock().get(&job_id).cloned();
        let Some(shard) = shard else {
            METRICS
                .job_remote_refused_stale
                .fetch_add(1, Ordering::Relaxed);
            refuse_submit(
                session,
                &job_id,
                shard_no,
                "unknown shard (reclaimed or never assigned)".into(),
            )
            .await;
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
            )
            .await;
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
                )
                .await;
                // A failed proposal is treated like an expired lease:
                // never publish, never reuse those destinations.
                *shard.holder.lock() = Some(ShardHolder {
                    session_id: session.id,
                    worker_id: session.worker_id.clone(),
                    lease_expiry: tokio::time::Instant::now(),
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
        self.fabric.remote_complete(&job_id, &shard.ctl).await;

        let mut w = session.writer.lock().await;
        let _ = write_frame(
            &mut *w,
            &WireFrame::ResultAck {
                job_id: job_id.clone(),
                shard: shard_no,
            },
        )
        .await;
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

/// Send one ResultRefused reply (best-effort — a vanished session's
/// refusal has nowhere to land, which is fine: fencing already holds).
async fn refuse_submit(session: &Arc<Session>, job_id: &str, shard: u32, reason: String) {
    let mut w = session.writer.lock().await;
    let _ = write_frame(
        &mut *w,
        &WireFrame::ResultRefused {
            job_id: job_id.to_string(),
            shard,
            reason,
        },
    )
    .await;
}

/// Acquire WERO (rtype 2) on every path. `Some` only when EVERY
/// namespace is PR-capable and every acquire succeeded (`pr` class);
/// anything partial releases what it took and returns `None`
/// (deferred-reclaim). Blocking (one-shot ioctls) — call via
/// `spawn_blocking`.
fn acquire_wero(paths: &[PathBuf]) -> Option<WeroFence> {
    let key = loop {
        let k = rand::Rng::gen::<u64>(&mut rand::thread_rng());
        if k != 0 {
            break k;
        }
    };
    let mut holds: Vec<Arc<dyn ReservationClient>> = Vec::new();
    for path in paths {
        let Some(client) = resolve_for_mount(path) else {
            log::warn!(
                "job wire: data namespace {} advertises no reservation support — \
                 WERO fence unavailable",
                path.display()
            );
            release_partial(&holds, key);
            return None;
        };
        let step = register_ladder(client.as_ref(), key)
            .and_then(|_| client.acquire_write_exclusive_registrants_only(key));
        match step {
            Ok(()) => holds.push(client),
            Err(e) => {
                log::warn!(
                    "job wire: WERO acquire on {} failed: {e} — fence unavailable",
                    path.display()
                );
                release_partial(&holds, key);
                return None;
            }
        }
    }
    if holds.is_empty() {
        return None;
    }
    Some(WeroFence { key, holds })
}

fn release_partial(holds: &[Arc<dyn ReservationClient>], key: u64) {
    for client in holds {
        if let Err(e) = client.release_registrants_only(key) {
            log::warn!("job wire: partial WERO release failed: {e}");
        }
    }
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
}

impl WorkerOptions {
    pub fn new(worker_id: &str) -> Self {
        Self {
            worker_id: worker_id.to_string(),
            heartbeats: Arc::new(AtomicBool::new(true)),
            hold_submission: Arc::new(AtomicBool::new(false)),
            pr_key: None,
            security: None,
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

/// An enrolled remote worker (client side of the wire).
pub struct JobWireWorker {
    stream: BoxedStream,
    opts: WorkerOptions,
    heartbeat: Duration,
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
    /// coordinator's reason.
    pub async fn connect(endpoint: &str, secret: &[u8], opts: WorkerOptions) -> Result<Self> {
        let tcp = tokio::net::TcpStream::connect(endpoint).await?;
        let mut stream: BoxedStream = match opts.security.as_ref() {
            Some(sec) => {
                if !channel_authenticated(sec) {
                    // The dial side of the VAL-6 ladder: without a CA
                    // pin `rustls_client_config` installs an
                    // accept-everything verifier — say so out loud
                    // rather than presenting the storage secret's proof
                    // into an unauthenticated pipe silently.
                    log::warn!(
                        "job worker: TLS with no CA pin — the server certificate is NOT \
                         validated (accept-everything verifier). Configure the cluster CA \
                         for an authenticated channel."
                    );
                }
                let cfg = rustls_client_config(sec)?;
                let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
                // The ClusterSecurityConfig node certs carry
                // localhost/127.0.0.1 SANs (cluster_tls.rs construction).
                let name = rustls::pki_types::ServerName::try_from("localhost")
                    .expect("literal server name")
                    .to_owned();
                Box::new(connector.connect(name, tcp).await?)
            }
            None => Box::new(tcp),
        };
        // The coordinator speaks first (VAL-6 schema 2): its challenge
        // nonce is what the proof is bound to. Bounded read at the
        // hello class — a hostile "coordinator" gets no allocation
        // authority either.
        let server_nonce = match tokio::time::timeout(
            ENROLL_DIAL_TIMEOUT,
            read_frame_limited(
                &mut stream,
                MAX_HELLO_FRAME_BYTES,
                Some(ENROLL_DIAL_TIMEOUT),
            ),
        )
        .await
        .map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "no enrollment challenge from the coordinator within {ENROLL_DIAL_TIMEOUT:?}"
            ))
        })?? {
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
            },
        )
        .await?;
        match read_frame_limited(
            &mut stream,
            MAX_HELLO_FRAME_BYTES,
            Some(ENROLL_DIAL_TIMEOUT),
        )
        .await?
        {
            Some(WireFrame::EnrollOk {
                wire_schema: _,
                heartbeat_ms,
                lease_ttl_ms: _,
            }) => Ok(Self {
                stream,
                opts,
                heartbeat: Duration::from_millis(heartbeat_ms.max(1)),
            }),
            Some(WireFrame::EnrollRefused { reason }) => Err(SqueezefsError::InvalidOperation(
                format!("enrollment refused by the coordinator: {reason}"),
            )),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "unexpected enrollment reply: {other:?}"
            ))),
        }
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
        } = self;
        let (mut rd, wr) = tokio::io::split(stream);
        let writer = Arc::new(tokio::sync::Mutex::new(wr));

        let (assign_tx, mut assign_rx) = tokio::sync::mpsc::channel::<ShardDescriptor>(4);
        let (resp_tx, mut resp_rx) =
            tokio::sync::mpsc::channel::<std::result::Result<(), String>>(4);
        // The worker-side lease clock: latest deadline estimate,
        // extended by every HeartbeatAck.
        let (lease_tx, lease_rx) = tokio::sync::watch::channel(tokio::time::Instant::now());

        // Read loop: route inbound frames. Abort-on-drop guards tie the
        // spawned halves to THIS future's lifetime (no leaked tasks —
        // and an aborted `run()` drops both stream halves, so the
        // coordinator observes the departure EOF).
        let read_task = AbortOnDrop(tokio::spawn(async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(Some(WireFrame::ShardAssign { shard })) => {
                        if assign_tx.send(shard).await.is_err() {
                            return;
                        }
                    }
                    Ok(Some(WireFrame::HeartbeatAck { lease_ttl_ms })) => {
                        let _ = lease_tx.send(
                            tokio::time::Instant::now() + Duration::from_millis(lease_ttl_ms),
                        );
                    }
                    Ok(Some(WireFrame::ResultAck { .. })) => {
                        if resp_tx.send(Ok(())).await.is_err() {
                            return;
                        }
                    }
                    Ok(Some(WireFrame::ResultRefused { reason, .. })) => {
                        if resp_tx.send(Err(reason)).await.is_err() {
                            return;
                        }
                    }
                    Ok(Some(other)) => {
                        log::warn!("job worker: unexpected frame {other:?} — ignored");
                    }
                    Ok(None) | Err(_) => return,
                }
            }
        }));

        // Heartbeat loop (10 s cadence by default; the coordinator's
        // EnrollOk sets it). The test hook models a partition: no
        // heartbeats at all.
        let hb_writer = Arc::clone(&writer);
        let hb_opts = opts.clone();
        let hb_task = AbortOnDrop(tokio::spawn(async move {
            loop {
                tokio::time::sleep(heartbeat).await;
                if !hb_opts.heartbeats.load(Ordering::SeqCst) {
                    continue;
                }
                let mut w = hb_writer.lock().await;
                if write_frame(
                    &mut *w,
                    &WireFrame::Heartbeat {
                        worker_id: hb_opts.worker_id.clone(),
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
            }
        }));

        let mut report = WorkerReport::default();
        'shards: while let Some(shard) = assign_rx.recv().await {
            let ttl = Duration::from_millis(shard.lease_ttl_ms.max(1));
            // Local lease clock (rung 1): the assignment starts a full
            // TTL; HeartbeatAcks extend the watch.
            let assigned_deadline = tokio::time::Instant::now() + ttl;

            let mut lease_rx = lease_rx.clone();
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
                                let start = tokio::time::Instant::now();
                                tokio::time::sleep(Duration::from_millis(*task_ms)).await;
                                if let Some(delay) =
                                    job_throttle_sleep(start.elapsed(), shard.throttle_pct)
                                {
                                    tokio::time::sleep(delay).await;
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
                    | JobType::DefragMeta
                    | JobType::DefragFold => {
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
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            {
                let mut w = writer.lock().await;
                if write_frame(
                    &mut *w,
                    &WireFrame::ResultSubmit {
                        job_id: shard.job_id.clone(),
                        shard: shard.shard,
                        shard_fencing: shard.shard_fencing,
                        checksums,
                    },
                )
                .await
                .is_err()
                {
                    break 'shards;
                }
            }
            match resp_rx.recv().await {
                Some(Ok(())) => report.shards_completed += 1,
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

        // The guards abort + detach the halves (dropping both stream
        // halves closes the connection — the coordinator sees the
        // departure).
        drop(hb_task);
        drop(read_task);
        Ok(report)
    }
}

/// Ties a spawned task to its owner's lifetime: dropping the guard
/// aborts the task (no fire-and-forget leaks — AGENTS structured-
/// concurrency posture; the abort also releases the stream half the
/// task owns).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Per-batch lease re-validation (§5.1.6 rung 1): a worker checks its
/// local expiry clock before every batch — expired ⇒ abort (`false`);
/// past half-TTL with heartbeating alive ⇒ one wire round-trip before
/// proceeding (bounded wait for the ack).
async fn revalidate_lease(
    lease_rx: &mut tokio::sync::watch::Receiver<tokio::time::Instant>,
    assigned_deadline: tokio::time::Instant,
    writer: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<BoxedStream>>>,
    opts: &WorkerOptions,
    ttl: Duration,
) -> bool {
    let deadline = |rx: &tokio::sync::watch::Receiver<tokio::time::Instant>| {
        (*rx.borrow()).max(assigned_deadline)
    };
    let now = tokio::time::Instant::now();
    let d = deadline(lease_rx);
    if now >= d {
        return false;
    }
    if opts.heartbeats.load(Ordering::SeqCst) && now + ttl / 2 >= d {
        // Wire round-trip: heartbeat now and wait (bounded) for the ack
        // to move the deadline before committing the next batch.
        {
            let mut w = writer.lock().await;
            if write_frame(
                &mut *w,
                &WireFrame::Heartbeat {
                    worker_id: opts.worker_id.clone(),
                },
            )
            .await
            .is_err()
            {
                return false;
            }
        }
        let _ = tokio::time::timeout(ttl / 4, lease_rx.changed()).await;
        return tokio::time::Instant::now() < deadline(lease_rx);
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
