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
//!   endpoint_nonce ‖ "hello")` — the wire proves and grants exactly
//!   what shared-storage access already grants.
//! - **Transport**: length-prefixed schema-versioned frames
//!   ([`WIRE_SCHEMA`]) over tokio TCP; TLS via **tokio-rustls** reusing
//!   `ClusterSecurityConfig`'s cert/CA/verifier construction (never the
//!   quinn wrap). Without a security config the listener runs
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

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::jobs::{job_throttle_sleep, JobCtl, JobFabric, JobType};
use crate::meta_backend::reservation::{register_ladder, resolve_for_mount, ReservationClient};
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use crate::tiering::dht::{rustls_client_config, rustls_server_config, ClusterSecurityConfig};

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
pub const WIRE_SCHEMA: u32 = 1;

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
/// a protocol violation, refused loud.
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

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
    Enroll {
        wire_schema: u32,
        worker_id: String,
        endpoint_nonce: String,
        /// hex `HMAC-SHA256(secret, worker_id ‖ endpoint_nonce ‖ "hello")`.
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

/// Read one frame; `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<WireFrame>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!(
            "frame length {len} exceeds the {MAX_FRAME_BYTES} B cap"
        )));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::other(format!("undecodable frame: {e}")))
}

/// The enrollment proof: hex `HMAC-SHA256(secret, worker_id ‖
/// endpoint_nonce ‖ "hello")` — computable only by a principal that can
/// read the meta volume's `job:enroll` record.
pub fn enroll_hmac(secret: &[u8], worker_id: &str, endpoint_nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(worker_id.as_bytes());
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
    /// Worker-side block fill (shared storage — the data plane).
    fn write_block(&self, dest: &DestTuple, data: &[u8]) -> std::io::Result<()>;
    /// Coordinator-side verify-read before publish.
    fn read_block(&self, dest: &DestTuple) -> std::io::Result<Vec<u8>>;
}

/// Production seam until the VL4 movers land: Noop shards plan zero
/// blocks, so allocation and device access are structurally
/// unreachable — reaching them is a bug, refused loud.
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
            "NoopDeviceSeam cannot allocate destinations — mutating shards land with the VL4 movers",
        ))
    }
    fn write_block(&self, _dest: &DestTuple, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "NoopDeviceSeam has no device — mutating shards land with the VL4 movers",
        ))
    }
    fn read_block(&self, _dest: &DestTuple) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "NoopDeviceSeam has no device — mutating shards land with the VL4 movers",
        ))
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
            allocs: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// The allocation ledger: one entry per non-empty `allocate` call,
    /// in call order.
    pub fn allocations(&self) -> Vec<Vec<DestTuple>> {
        self.allocs.lock().clone()
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
    /// Verify-read sampling in permille under TLS. **Ignored on
    /// plaintext transports: the Issue-30 law forces 1000.**
    pub verify_sample_permille: u32,
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
        }
    }
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

/// The §5.1.6 wire host: TCP(/TLS) listener + dispatcher + lease
/// sweeper on the job-fabric coordinator.
pub struct JobWireHost {
    fabric: Arc<JobFabric>,
    seam: Arc<dyn ShardDeviceSeam>,
    cfg: JobWireConfig,
    endpoint: SocketAddr,
    secret: Vec<u8>,
    transport: &'static str,
    verify_permille: u32,
    tls: Option<tokio_rustls::TlsAcceptor>,
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

        let (tls, transport, verify_permille) = match cfg.security.as_ref() {
            Some(sec) => {
                let server_cfg = rustls_server_config(sec)?;
                (
                    Some(tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg))),
                    "tls",
                    cfg.verify_sample_permille.min(1000),
                )
            }
            None => (None, "plaintext", 1000),
        };

        let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
        let endpoint = listener.local_addr()?;
        if transport == "plaintext" {
            // OQ-A default-permissive: the ONE loud line. HMAC gates
            // enrollment (integrity of enrollment, not of frames);
            // the Issue-30 law makes every mutating publish 100 %
            // verify-read, so a hijacked session cannot publish bytes
            // the coordinator has not itself read and checksummed.
            log::warn!(
                "job wire: listener {endpoint} is PLAINTEXT TCP (no ClusterSecurityConfig) — \
                 enrollment is HMAC-gated only; mutating publishes pay mandatory-100 % \
                 verify-reads (Issue-30; ≈2× device reads on remote-mutated bytes). \
                 Deploy ClusterSecurityConfig for TLS/mTLS + sampled verification."
            );
        } else {
            log::info!("job wire: listener {endpoint} (TLS via ClusterSecurityConfig)");
            if cfg.verify_sample_permille < 1000 {
                log::info!(
                    "job wire: verify-read sampling {}‰ (sanctioned under TLS)",
                    verify_permille
                );
            }
        }

        let host = Arc::new(Self {
            fabric,
            seam,
            endpoint,
            secret,
            transport,
            verify_permille,
            tls,
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

        let h1 = tokio::spawn(Self::accept_loop(Arc::clone(&host), listener));
        let h2 = tokio::spawn(Self::dispatcher_loop(Arc::clone(&host)));
        let h3 = tokio::spawn(Self::sweeper_loop(Arc::clone(&host)));
        host.handles.lock().extend([h1, h2, h3]);
        Ok(host)
    }

    /// The bound listener address.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// `"plaintext"` or `"tls"`.
    pub fn transport_mode(&self) -> &'static str {
        self.transport
    }

    /// Effective verify-read sampling (‰). **1000 on plaintext by the
    /// Issue-30 law, whatever was configured.**
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
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    log::warn!("job wire: accept failed: {e}");
                    continue;
                }
            };
            let host = Arc::clone(&self);
            let h = tokio::spawn(async move {
                let stream: BoxedStream = match host.tls.clone() {
                    Some(acceptor) => match acceptor.accept(tcp).await {
                        Ok(s) => Box::new(s),
                        Err(e) => {
                            log::warn!("job wire: TLS handshake with {peer} failed: {e}");
                            return;
                        }
                    },
                    None => Box::new(tcp),
                };
                host.serve_conn(stream, peer).await;
            });
            self.handles.lock().push(h);
        }
    }

    /// One connection: enrollment gate, then the session frame loop.
    async fn serve_conn(self: &Arc<Self>, stream: BoxedStream, peer: SocketAddr) {
        let mut stream = stream;
        let hello =
            match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stream)).await {
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
                    log::warn!("job wire: {peer}: no hello within 10 s");
                    return;
                }
            };
        let WireFrame::Enroll {
            wire_schema,
            worker_id,
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
        let expected = enroll_hmac(&self.secret, &worker_id, &endpoint_nonce);
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

        // Session frame loop.
        loop {
            let frame = match read_frame(&mut rd).await {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("job wire: session {} read error: {e}", session.id);
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
            let Some((job_id, ctl)) = self.fabric.claim_next() else {
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
            let fence = self.fence.lock().await;
            if let Some(f) = fence.as_ref() {
                let key = f.key;
                let holds = f.holds.clone();
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
                let cfg = rustls_client_config(sec)?;
                let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
                // The ClusterSecurityConfig node certs carry
                // localhost/127.0.0.1 SANs (dht.rs construction).
                let name = rustls::pki_types::ServerName::try_from("localhost")
                    .expect("literal server name")
                    .to_owned();
                Box::new(connector.connect(name, tcp).await?)
            }
            None => Box::new(tcp),
        };
        let nonce = uuid::Uuid::new_v4().to_string();
        write_frame(
            &mut stream,
            &WireFrame::Enroll {
                wire_schema: WIRE_SCHEMA,
                worker_id: opts.worker_id.clone(),
                endpoint_nonce: nonce.clone(),
                hmac: enroll_hmac(secret, &opts.worker_id, &nonce),
                pr_key: opts.pr_key,
            },
        )
        .await?;
        match read_frame(&mut stream).await? {
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
            'fill: for chunk in shard.destinations.chunks(REVALIDATE_BATCH) {
                if !revalidate_lease(&mut lease_rx, assigned_deadline, &writer, &opts, ttl).await {
                    aborted = true;
                    break 'fill;
                }
                for dest in chunk {
                    let data = block_pattern(shard.shard_fencing, dest, shard.block_len as usize);
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
