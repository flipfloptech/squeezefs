//! **DLM stage S3 — `cluster_wire`**: the ONE cluster transport every
//! later stage rides (pre-rc spec §6.7 *Transport*, §6.9 stage **S3**;
//! execution plan §6.3; rulings **D2**, **D8**, **D10**).
//!
//! `job_wire` was "the right shape and the wrong implementation for a
//! custody-bearing protocol" (§6.7). Rather than extend it, this module
//! is the transport it should have been on, and `job_wire` is ported onto
//! it — which closes **VAL-6** structurally and leaves **one** cluster
//! transport in the tree instead of two that must be kept in agreement.
//!
//! # What lives here, and why each piece is where it is
//!
//! ### 1. Framing — binary, because the budget says so
//!
//! Length-prefixed (`u32` BE), schema-versioned, **binary** (bincode with
//! varint integers). The measured reason: VAL-6's own `job_wire_frame`
//! microbench priced `serde_json` frame decode at **32.6 µs** for a
//! 64-checksum mover shard against §6.5's **10 µs** custody budget —
//! corroborating §6.7's ~1 µs/frame note at the small end. A JSON frame
//! on a lock-acquire path would spend the whole latency budget parsing
//! text. Evidence: `.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`.
//!
//! Three frame **classes** — a handshake, a lock verb and a mover shard
//! are three different budgets — each with its own cap and deadline:
//! [`FrameClass::Handshake`] (8 KiB, what an *unauthenticated* peer gets
//! to spend), [`FrameClass::Control`] (1 MiB, the S4+ verb class), and
//! [`FrameClass::Bulk`] (16 MiB, shard descriptors and result proposals).
//! VAL-6's bounds are carried **forward** here, never re-derived: body
//! memory is committed [`FRAME_CHUNK_BYTES`] at a time *as bytes arrive*
//! (a lying length prefix costs one chunk, not the cap), every started
//! body carries a deadline, connections are capped by an RAII permit
//! claimed before any task exists, accept errors ride an exponential
//! backoff ladder ([`next_accept_backoff`]), and finished connection
//! handles are pruned instead of retained per connection ever accepted.
//!
//! ### 2. Zero-config mutual authentication (ruling **D2**)
//!
//! The listener stays configurable and **default-open**, peers are
//! **auto-discovered**, so authentication cannot be an operator's
//! deployment step. The root of trust is **storage**: whoever can read
//! the shared metadata volume's `job:enroll` record is definitionally
//! inside the trust domain — that is what makes zero-config sound, and it
//! grants exactly what shared-storage access already grants.
//!
//! The ladder, in order, all of it in ONE place ([`AuthnGate::verify`]):
//!
//! 1. The **coordinator speaks first** with a [`Challenge`] carrying a
//!    **server-issued** nonce, single-use inside a freshness window (the
//!    peer cannot choose its own challenge, so a captured hello is not a
//!    credential).
//! 2. The peer answers with [`proof_mac`] — `HMAC-SHA256(secret, peer_id
//!    ‖ server_nonce ‖ peer_nonce ‖ "hello")`, compared with the
//!    byte-for-byte [`mac_eq`] (spec §8 invariant, preserved verbatim).
//! 3. Both sides derive a **session key** from the same secret and both
//!    nonces ([`session_key`]), optionally **bound to the channel** (the
//!    TLS exporter), and every subsequent frame carries a **per-frame
//!    MAC** over `direction ‖ sequence ‖ length ‖ body`
//!    ([`FrameTx`]/[`FrameRx`]). Authentication therefore survives past
//!    the handshake: tamper, reorder, replay and reflection are all
//!    refused mid-session, which is what a custody-bearing protocol needs
//!    and what enrollment-only authentication could never give.
//!
//! **The accept-everything certificate verifier is deleted.** A CA-less
//! `ClusterSecurityConfig` is refused ([`tls_acceptor`]/[`tls_connector`])
//! instead of silently installing `.dangerous()` client-side and
//! `with_no_client_auth()` server-side. The verification-strength ladder
//! keys on [`SessionAuthn`] — an *authenticated* channel — never on
//! `transport == "tls"`; and sampling stays admissible only where the
//! channel is also confidential ([`SessionAuthn::verify_sampling_admissible`]),
//! so storage-trust authn strengthens plaintext without weakening the
//! Issue-30 verify-read law.
//!
//! ### 3. Owner-side RPC runs on pinned service threads
//!
//! §6.7 is explicit: owner-side RPC handling runs on pinned service
//! threads (the `ipc_service.rs` pattern), **never on the conveyor's
//! task** — the commit conveyor is a serialized ~0.78 ms server at
//! ρ ≈ 0.92, and an RPC on it would multiply through the queueing
//! formula. [`ServicePool`] is that venue: named OS threads (so
//! `pidstat`/`perf` attribution works — the `fuse3-tpcN` lesson), NUMA
//! pinned through the same `numa_core` partition the IPC host uses, each
//! running a current-thread runtime with a `LocalSet`. [`RpcService::call`]
//! is **synchronous by contract**, because §6.7's lock arbitration is
//! RAM-only (an `scc` probe plus one atomic); anything that must await
//! belongs on an explicit handoff, exactly as `ipc_service` does — and
//! since **S8** that handoff is a type, [`RpcAsyncService`], whose future
//! is polled on the same pinned lane (S8's own service hops to the
//! runtime that owns the metadata backend's tasks itself, visibly).
//!
//! Deliberately **not** io_uring: TLS peers and network TCP/TLS stacks are
//! the sanctioned non-uring exception (AGENTS "Not uring" row).
//!
//! ### 4. DISC-1 — the shared volume IS the rendezvous
//!
//! Endpoint publication already exists: the mount heartbeat's
//! `client:{uuid}` record carries `job_endpoint`. Discovery is therefore
//! an **enumeration** of records ([`discover_peers`]) — no multicast, no
//! seed list, no configured peers (ruling D2). This interim form reads the
//! records as they are today; the final form rides **S6**'s lease-based
//! membership so discovery adds **zero** load to the ino-1 hotspot, which
//! §6.5 item 3 measures as saturating at ~4,550 clients (each beat is a
//! full journal transaction under an exclusive `I{1}` guard, and the read
//! side is `listxattr(1)` plus one `getxattr` per client).
//!
//! ### 5. The RTT instrument
//!
//! [`measure_rtt`] is the deliverable beyond code: one authenticated
//! request/response round trip on THIS wire, which prices **S8** (risk
//! **R1**, accepted as ruling **D10**) before S8 is designed. A loopback
//! row is a **floor** — framing + authn + wake with the fabric term at
//! ~0; a fabric row needs the venue, substrate and discipline named in
//! `.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`.
//!
//! # Scope discipline (KD-15)
//!
//! **This is not a networked DLM.** There are no lock verbs on this wire:
//! S4 owns those and plugs into [`RpcService`] with its own verb numbers.
//! S3 ships framing, authentication, discovery, the pinned RPC venue, and
//! the `job_wire` port. **S8**'s metadata verbs are the same discipline:
//! the vocabulary lives in [`crate::meta_ship`] and plugs in through
//! [`RpcAsyncService`]; this module never learns what a verb means.

use crate::error::{Result, SqueezefsError};
use crate::meta_backend::RoutedMetaBackend;
use crate::tiering::cluster_tls::{
    rustls_client_config, rustls_server_config, ClusterCa, ClusterSecurityConfig,
};

use bincode::Options as _;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ---------------------------------------------------------------------------
// Schema + framing constants
// ---------------------------------------------------------------------------

/// The cluster-wire **RPC vocabulary** schema this build speaks (the verb
/// surface S4 extends). A peer presenting any other value is refused loud,
/// naming the field — never interpreted, because a custody-bearing
/// protocol has no safe guess.
///
/// Two-level versioning, deliberately: the FRAMING (length prefix, class
/// caps, MAC layout) is this module's byte layout and changes only with a
/// transport change, while each protocol riding the wire versions its own
/// vocabulary — `job_wire::WIRE_SCHEMA` is at 3 for exactly that reason.
/// One shared number would force an unrelated protocol's peers to
/// re-enroll whenever the other one added a field.
pub const CLUSTER_WIRE_SCHEMA: u32 = 1;

/// The **pre-authentication** frame class cap: a challenge/proof pair is a
/// few hundred bytes, so this is all an unauthenticated peer gets to
/// spend (and the body is streamed in [`FRAME_CHUNK_BYTES`] rounds even
/// inside it).
pub const HANDSHAKE_MAX_FRAME_BYTES: u32 = 8 * 1024;

/// The **verb** class cap (S4+ lock RPC, membership, revocation): big
/// enough for a batched reclaim frame carrying a client's whole token set
/// (§6.10 R6), small enough that it is not a bulk channel.
pub const CONTROL_MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// The **bulk** class cap: shard descriptors and result proposals. Blocks
/// move over shared storage, not the wire, so anything larger is a
/// protocol violation.
pub const BULK_MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Frame bodies are committed to memory this much at a time: the length
/// prefix is a *claim*, never an allocation authority (VAL-6).
pub const FRAME_CHUNK_BYTES: usize = 64 * 1024;

/// Per-frame authentication tag width (HMAC-SHA256, untruncated).
pub const MAC_BYTES: usize = 32;

/// First rung of the accept-error backoff ladder: a bare `continue` turned
/// a persistent `EMFILE`/`ENFILE` into a busy loop.
pub const ACCEPT_BACKOFF_START: Duration = Duration::from_millis(5);

/// Backoff ceiling — small enough that a transient fd exhaustion recovers
/// promptly once the pressure lifts.
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

/// A frame's size/deadline class. Three, because a handshake, a verb and a
/// shard are three different budgets — and the smallest one is what an
/// unauthenticated peer is allowed to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameClass {
    /// Pre-authentication: challenge, proof, admission, refusal.
    Handshake,
    /// Authenticated verbs (S4+: acquire/revoke/renew/meta).
    Control,
    /// Authenticated bulk: shard descriptors, result proposals.
    Bulk,
}

impl FrameClass {
    /// The class byte cap.
    pub fn cap(self) -> u32 {
        match self {
            FrameClass::Handshake => HANDSHAKE_MAX_FRAME_BYTES,
            FrameClass::Control => CONTROL_MAX_FRAME_BYTES,
            FrameClass::Bulk => BULK_MAX_FRAME_BYTES,
        }
    }

    /// The constant's own name, so a refusal can name the cap it enforced
    /// rather than printing a bare number an operator has to look up.
    pub fn cap_name(self) -> &'static str {
        match self {
            FrameClass::Handshake => "HANDSHAKE_MAX_FRAME_BYTES",
            FrameClass::Control => "CONTROL_MAX_FRAME_BYTES",
            FrameClass::Bulk => "BULK_MAX_FRAME_BYTES",
        }
    }
}

/// The binary codec: bincode with varint integers, little endian, trailing
/// bytes rejected (a frame with a tail is a protocol violation, not a
/// prefix match). `limit` bounds decode-side allocation from a lying
/// in-body length field — the reason untrusted input must never be
/// deserialized with an unbounded configuration.
fn codec(limit: u64) -> impl bincode::Options {
    bincode::DefaultOptions::new().with_limit(limit)
}

/// Encode one frame body, refusing past the class cap and naming it.
///
/// The encoder is deliberately UNLIMITED and the cap is checked after: a
/// frame we build ourselves is trusted input, and bincode's own
/// size-limit error ("the size limit has been reached") cannot name which
/// class cap it hit, which is the whole point of the refusal. Decode is
/// the opposite — untrusted, so bounded (see [`decode_body`]).
fn encode_body<T: Serialize>(frame: &T, class: FrameClass) -> std::io::Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(frame)
        .map_err(|e| std::io::Error::other(format!("frame encode failed: {e}")))?;
    if body.len() > class.cap() as usize {
        return Err(std::io::Error::other(format!(
            "frame body of {} B exceeds the {:?} class cap {} = {} B",
            body.len(),
            class,
            class.cap_name(),
            class.cap()
        )));
    }
    Ok(body)
}

/// Decode one frame body under a size limit equal to the delivered bytes:
/// a lying in-body length field can then never become an allocation
/// authority (the reason untrusted input is never deserialized with an
/// unbounded configuration).
fn decode_body<T: DeserializeOwned>(body: &[u8]) -> std::io::Result<T> {
    codec(body.len() as u64)
        .deserialize(body)
        .map_err(|e| std::io::Error::other(format!("undecodable frame: {e}")))
}

/// Commit body memory only as it arrives: at most one
/// [`FRAME_CHUNK_BYTES`] round is outstanding ahead of the peer, so four
/// attacker-chosen length bytes buy one chunk instead of the cap.
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

/// Read `len` body bytes plus `trailer` trailing bytes under one optional
/// deadline covering the WHOLE body (a dribbling peer is an error, not a
/// parked task). The length-prefix read itself is deliberately outside the
/// deadline: an idle authenticated session legitimately waits between
/// frames, and its bound is the session idle timeout.
async fn read_framed_bytes<R: AsyncRead + Unpin>(
    r: &mut R,
    max_len: u32,
    trailer: usize,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
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
    let read = async {
        let body = read_body_chunked(r, len as usize).await?;
        let mut tail = vec![0u8; trailer];
        if trailer > 0 {
            r.read_exact(&mut tail).await?;
        }
        Ok::<_, std::io::Error>((body, tail))
    };
    let out = match body_timeout {
        Some(d) => tokio::time::timeout(d, read).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("frame body of {len} B did not arrive within {d:?}"),
            )
        })??,
        None => read.await?,
    };
    Ok(Some(out))
}

/// Write one **unauthenticated** length-prefixed frame — the handshake
/// classes, and the pre-session direction of any protocol. One `write_all`
/// (prefix and body in a single buffer): a syscall per frame is the RTT
/// term this wire is measured on.
pub async fn write_plain_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    class: FrameClass,
    frame: &T,
) -> std::io::Result<()> {
    let body = encode_body(frame, class)?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    w.write_all(&out).await?;
    w.flush().await
}

/// Read one **unauthenticated** frame under an explicit class cap and
/// optional body deadline. `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_plain_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    max_len: u32,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<T>> {
    match read_framed_bytes(r, max_len, 0, body_timeout).await? {
        None => Ok(None),
        Some((body, _)) => decode_body(&body).map(Some),
    }
}

// ---------------------------------------------------------------------------
// Session keys + per-frame authentication
// ---------------------------------------------------------------------------

/// A derived per-session key. `Debug` is **redacted**: key material must
/// never reach a log line, and this type is carried by structs that do get
/// logged.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKey([u8; 32]);

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

/// Derive the session key both sides compute independently:
/// `HMAC-SHA256(secret, label ‖ peer_id ‖ server_nonce ‖ peer_nonce ‖
/// binding)`. Every input is bound in — a different peer, a different
/// challenge, a different channel is a different key.
///
/// `binding` is the **channel binding** when the session runs over TLS
/// (the RFC 5705 exporter output, identical on both ends), which is what
/// makes the per-frame MAC exporter-bound rather than merely
/// secret-bound.
pub fn session_key(
    secret: &[u8],
    peer_id: &str,
    server_nonce: &str,
    peer_nonce: &str,
    binding: Option<&[u8]>,
) -> SessionKey {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"squeezefs-cluster-wire/v1/session");
    mac.update(peer_id.as_bytes());
    mac.update(b"\x00");
    mac.update(server_nonce.as_bytes());
    mac.update(b"\x00");
    mac.update(peer_nonce.as_bytes());
    mac.update(b"\x00");
    mac.update(binding.unwrap_or(b""));
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    SessionKey(key)
}

/// Which end of the session a framer sits on. The direction tag is inside
/// every frame MAC, so a frame reflected at its own author fails to
/// verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The listening side (the volume owner / job coordinator).
    Coordinator,
    /// The dialing side (a remote worker, a client node).
    Peer,
}

const DIR_COORDINATOR_TO_PEER: u8 = 0x01;
const DIR_PEER_TO_COORDINATOR: u8 = 0x02;

/// Authenticated frame **sender**: one monotonic sequence per direction.
#[derive(Debug)]
pub struct FrameTx {
    key: SessionKey,
    dir: u8,
    seq: u64,
}

/// Authenticated frame **receiver**: the expected sequence is inside the
/// MAC, so a reordered or replayed frame cannot verify.
#[derive(Debug)]
pub struct FrameRx {
    key: SessionKey,
    dir: u8,
    seq: u64,
}

/// The framer pair for one end of an authenticated session.
pub fn session_framers(key: &SessionKey, role: Role) -> (FrameTx, FrameRx) {
    let (tx_dir, rx_dir) = match role {
        Role::Coordinator => (DIR_COORDINATOR_TO_PEER, DIR_PEER_TO_COORDINATOR),
        Role::Peer => (DIR_PEER_TO_COORDINATOR, DIR_COORDINATOR_TO_PEER),
    };
    (
        FrameTx {
            key: key.clone(),
            dir: tx_dir,
            seq: 0,
        },
        FrameRx {
            key: key.clone(),
            dir: rx_dir,
            seq: 0,
        },
    )
}

fn frame_mac(key: &SessionKey, dir: u8, seq: u64, body: &[u8]) -> [u8; MAC_BYTES] {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(&[dir]);
    mac.update(&seq.to_be_bytes());
    mac.update(&(body.len() as u32).to_be_bytes());
    mac.update(body);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; MAC_BYTES];
    tag.copy_from_slice(&out);
    tag
}

/// Constant-time tag comparison (XOR-accumulate, never short-circuit) —
/// the [`mac_eq`] law applied to raw bytes.
fn tag_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

impl FrameTx {
    /// Send one authenticated frame: `len ‖ body ‖ mac`, one `write_all`.
    pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(
        &mut self,
        w: &mut W,
        class: FrameClass,
        frame: &T,
    ) -> std::io::Result<()> {
        let body = encode_body(frame, class)?;
        let tag = frame_mac(&self.key, self.dir, self.seq, &body);
        let mut out = Vec::with_capacity(4 + body.len() + MAC_BYTES);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&tag);
        w.write_all(&out).await?;
        w.flush().await?;
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }

    /// Frames sent on this direction (the sequence, i.e. the engagement
    /// gauge).
    pub fn frames(&self) -> u64 {
        self.seq
    }
}

impl FrameRx {
    /// Receive one authenticated frame. Verification failure is a session
    /// error, never a skipped frame: tamper, reorder, replay and
    /// reflection are indistinguishable to the receiver and all fatal.
    pub async fn recv<R: AsyncRead + Unpin, T: DeserializeOwned>(
        &mut self,
        r: &mut R,
        max_len: u32,
        body_timeout: Option<Duration>,
    ) -> std::io::Result<Option<T>> {
        let Some((body, tag)) = read_framed_bytes(r, max_len, MAC_BYTES, body_timeout).await?
        else {
            return Ok(None);
        };
        let expect = frame_mac(&self.key, self.dir, self.seq, &body);
        if !tag_eq(&expect, &tag) {
            return Err(std::io::Error::other(format!(
                "frame mac verification failed at expected sequence {} — tampered, \
                 reordered, replayed or reflected frame",
                self.seq
            )));
        }
        self.seq = self.seq.wrapping_add(1);
        decode_body(&body).map(Some)
    }

    /// Frames verified on this direction.
    pub fn frames(&self) -> u64 {
        self.seq
    }
}

// ---------------------------------------------------------------------------
// The authentication gate (ONE implementation of the ladder)
// ---------------------------------------------------------------------------

/// The enrollment proof: hex `HMAC-SHA256(secret, peer_id ‖ server_nonce ‖
/// peer_nonce ‖ "hello")` — computable only by a principal that can read
/// the meta volume's `job:enroll` record, and bound to the coordinator's
/// single-use challenge, so a captured proof is not a reusable credential.
pub fn proof_mac(secret: &[u8], peer_id: &str, server_nonce: &str, peer_nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(peer_id.as_bytes());
    mac.update(server_nonce.as_bytes());
    mac.update(peer_nonce.as_bytes());
    mac.update(b"hello");
    hex_encode(&mac.finalize().into_bytes())
}

/// Lower-case hex of `bytes`.
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode lower/upper-case hex; `None` on any non-hex input.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Constant-time-ish comparison for the enrollment proof (both sides are
/// fixed-length hex MACs; XOR-accumulate, never short-circuit).
///
/// Spec §8 invariant: this comparison was already correct and is preserved
/// **byte for byte** across the S3 port.
pub fn mac_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The gate's clock. `Monotonic` is production; `Manual` is the
/// deterministic test seam (freshness windows must be provable without a
/// sleep, and the tree's law is seams, never sleeps).
#[derive(Debug, Clone)]
pub enum WireClock {
    /// Process-monotonic milliseconds.
    Monotonic(std::time::Instant),
    /// Test-driven milliseconds.
    Manual(Arc<AtomicU64>),
}

impl WireClock {
    /// The production clock.
    pub fn monotonic() -> Self {
        WireClock::Monotonic(std::time::Instant::now())
    }

    /// A clock the caller advances by storing into the counter.
    pub fn manual(ms: Arc<AtomicU64>) -> Self {
        WireClock::Manual(ms)
    }

    fn now_ms(&self) -> u64 {
        match self {
            WireClock::Monotonic(base) => base.elapsed().as_millis() as u64,
            WireClock::Manual(ms) => ms.load(Ordering::SeqCst),
        }
    }
}

impl Default for WireClock {
    fn default() -> Self {
        WireClock::monotonic()
    }
}

/// Authentication bounds. Every field is a bound on what an
/// **unauthenticated** peer can cost or claim.
#[derive(Debug, Clone)]
pub struct AuthnConfig {
    /// How long a coordinator-issued challenge stays acceptable.
    pub freshness: Duration,
    /// Cap on a peer's self-declared identity — it lands in log lines and
    /// durable records, so it is bounded like every other attacker-chosen
    /// field.
    pub max_peer_id_bytes: usize,
    /// Outstanding-challenge ceiling (memory an unauthenticated
    /// population can make the coordinator hold).
    pub nonce_cap: usize,
    /// The schema this gate admits.
    pub schema: u32,
}

impl Default for AuthnConfig {
    fn default() -> Self {
        Self {
            freshness: Duration::from_secs(30),
            max_peer_id_bytes: 256,
            nonce_cap: 4096,
            schema: CLUSTER_WIRE_SCHEMA,
        }
    }
}

/// A coordinator-issued challenge: the nonce the proof must answer, plus
/// the window the coordinator will accept it in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    /// The gate's schema.
    pub schema: u32,
    /// Single-use nonce, chosen by the **coordinator**.
    pub server_nonce: String,
    /// The freshness window, advertised so a peer knows to reconnect
    /// rather than retry a dead challenge.
    pub freshness_ms: u64,
}

/// A peer's answered proof, decoded out of whatever vocabulary its
/// protocol uses (job-shard enrollment, RPC handshake, …). One ladder
/// verifies them all.
#[derive(Debug, Clone)]
pub struct ProofClaim<'a> {
    /// Schema the peer claims to speak.
    pub schema: u32,
    /// The peer's self-declared identity.
    pub peer_id: &'a str,
    /// The nonce the coordinator issued on THIS connection.
    pub server_nonce: &'a str,
    /// The peer's own entropy contribution (bound into the session key).
    pub peer_nonce: &'a str,
    /// Hex [`proof_mac`].
    pub mac: &'a str,
}

/// The gate's verdict. `Refuse` carries the operator-facing reason, which
/// is also what the wire returns to the peer.
#[derive(Debug)]
pub enum Verdict {
    /// Admitted: the session key both sides now hold.
    Admit(SessionKey),
    /// Refused, with the reason.
    Refuse(String),
}

/// Outcome of consuming a challenge nonce.
enum NonceOutcome {
    /// Issued by this gate, inside its window, first use.
    Fresh,
    /// Issued, but the freshness window has closed.
    Expired,
    /// Never issued, or already used — a replay.
    UnknownOrReplayed,
}

/// Single-use nonces inside a freshness window, bounded in memory.
struct NonceRegistry {
    issued: HashMap<String, u64>,
    order: VecDeque<String>,
    cap: usize,
}

impl NonceRegistry {
    fn new(cap: usize) -> Self {
        Self {
            issued: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn issue(&mut self, now_ms: u64, freshness: Duration) -> String {
        self.prune(now_ms, freshness);
        let nonce = uuid::Uuid::new_v4().to_string();
        self.issued.insert(nonce.clone(), now_ms);
        self.order.push_back(nonce.clone());
        nonce
    }

    /// Single use: a fresh nonce is REMOVED as it is accepted, so the
    /// second presentation of the same hello is a replay.
    fn consume(&mut self, nonce: &str, now_ms: u64, freshness: Duration) -> NonceOutcome {
        match self.issued.remove(nonce) {
            Some(issued) if now_ms.saturating_sub(issued) <= freshness.as_millis() as u64 => {
                NonceOutcome::Fresh
            }
            Some(_) => NonceOutcome::Expired,
            None => NonceOutcome::UnknownOrReplayed,
        }
    }

    fn prune(&mut self, now_ms: u64, freshness: Duration) {
        let window = freshness.as_millis() as u64;
        while let Some(front) = self.order.front() {
            let stale = self
                .issued
                .get(front)
                .is_none_or(|t| now_ms.saturating_sub(*t) > window);
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

/// The **one** implementation of the zero-config mutual-authn ladder.
/// Every protocol on this wire decodes its own vocabulary into a
/// [`ProofClaim`] and calls [`AuthnGate::verify`], so the security
/// decisions — order, schema, identity bound, constant-time compare,
/// single use, freshness, key derivation — exist in exactly one place and
/// cannot drift between vocabularies.
pub struct AuthnGate {
    secret: Vec<u8>,
    cfg: AuthnConfig,
    clock: WireClock,
    nonces: parking_lot::Mutex<NonceRegistry>,
}

impl std::fmt::Debug for AuthnGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never reaches a log line.
        f.debug_struct("AuthnGate")
            .field("schema", &self.cfg.schema)
            .field("freshness", &self.cfg.freshness)
            .field("outstanding_challenges", &self.outstanding_challenges())
            .finish_non_exhaustive()
    }
}

impl AuthnGate {
    /// A gate on the production clock.
    pub fn new(secret: Vec<u8>, cfg: AuthnConfig) -> Self {
        Self::with_clock(secret, cfg, WireClock::monotonic())
    }

    /// A gate on an explicit clock (the deterministic freshness seam).
    pub fn with_clock(secret: Vec<u8>, cfg: AuthnConfig, clock: WireClock) -> Self {
        let nonces = parking_lot::Mutex::new(NonceRegistry::new(cfg.nonce_cap));
        Self {
            secret,
            cfg,
            clock,
            nonces,
        }
    }

    /// The coordinator speaks first: issue this connection's challenge.
    pub fn issue_challenge(&self) -> Challenge {
        let now = self.clock.now_ms();
        let nonce = self.nonces.lock().issue(now, self.cfg.freshness);
        Challenge {
            schema: self.cfg.schema,
            server_nonce: nonce,
            freshness_ms: self.cfg.freshness.as_millis() as u64,
        }
    }

    /// Outstanding (issued, unanswered, unexpired) challenges.
    pub fn outstanding_challenges(&self) -> usize {
        self.nonces.lock().issued.len()
    }

    /// The bounds this gate enforces (a protocol's own frames may need to
    /// quote them in a refusal).
    pub fn config(&self) -> &AuthnConfig {
        &self.cfg
    }

    /// Verify one proof and, on success, derive the session key.
    ///
    /// Order is load-bearing: schema, then the identity bound, then the
    /// constant-time MAC compare, and **only then** the nonce consume —
    /// so a peer that cannot produce a valid MAC can never spend a
    /// coordinator-issued nonce (the alternative lets an unauthenticated
    /// peer burn the challenge of the honest connection it raced).
    pub fn verify(&self, claim: ProofClaim<'_>, binding: Option<&[u8]>) -> Verdict {
        if claim.schema != self.cfg.schema {
            return Verdict::Refuse(format!(
                "wire_schema {} not supported (this coordinator speaks {}) — refusing rather \
                 than interpreting a foreign schema",
                claim.schema, self.cfg.schema
            ));
        }
        if claim.peer_id.len() > self.cfg.max_peer_id_bytes {
            return Verdict::Refuse(format!(
                "peer_id of {} B exceeds the {} B cap",
                claim.peer_id.len(),
                self.cfg.max_peer_id_bytes
            ));
        }
        let expected = proof_mac(
            &self.secret,
            claim.peer_id,
            claim.server_nonce,
            claim.peer_nonce,
        );
        if !mac_eq(&expected, claim.mac) {
            return Verdict::Refuse(
                "mac invalid (storage-membership proof failed — the peer cannot read the \
                 volume's job:enroll record)"
                    .to_string(),
            );
        }
        let now = self.clock.now_ms();
        match self
            .nonces
            .lock()
            .consume(claim.server_nonce, now, self.cfg.freshness)
        {
            NonceOutcome::Fresh => Verdict::Admit(session_key(
                &self.secret,
                claim.peer_id,
                claim.server_nonce,
                claim.peer_nonce,
                binding,
            )),
            NonceOutcome::Expired => Verdict::Refuse(format!(
                "enrollment challenge expired (freshness window {:?}) — reconnect",
                self.cfg.freshness
            )),
            NonceOutcome::UnknownOrReplayed => Verdict::Refuse(
                "enrollment nonce is unknown or already spent (replayed hello) — challenges \
                 are single-use"
                    .to_string(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Channel class + the verification-strength ladder's predicate
// ---------------------------------------------------------------------------

/// What the underlying byte stream is. There is no "TLS with no CA pin"
/// class any more: that configuration is refused
/// ([`tls_acceptor`]/[`tls_connector`]) rather than admitted as a weaker
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelClass {
    /// TCP. Storage-trust authn makes it *authenticated* (identity +
    /// per-frame integrity); it is not confidential.
    Plaintext,
    /// CA-pinned mutual TLS: authenticated **and** confidential.
    MutualTls,
}

impl ChannelClass {
    /// Is the byte stream confidential?
    pub fn confidential(self) -> bool {
        matches!(self, ChannelClass::MutualTls)
    }

    /// The operator-facing name.
    pub fn name(self) -> &'static str {
        match self {
            ChannelClass::Plaintext => "plaintext",
            ChannelClass::MutualTls => "mtls",
        }
    }
}

/// One session's authentication state — the predicate the
/// verification-strength ladder keys on, replacing `transport == "tls"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAuthn {
    /// The byte stream's class.
    pub channel: ChannelClass,
    /// The storage-membership proof verified against a server-issued,
    /// single-use, fresh nonce.
    pub proof_verified: bool,
    /// Per-frame MAC engaged for the rest of the session.
    pub mac_engaged: bool,
}

impl SessionAuthn {
    /// **Authenticated** ≡ the peer proved storage membership on a
    /// server-issued challenge **and** every subsequent frame carries the
    /// session MAC. A missing rung is not a lesser class; it is
    /// unauthenticated.
    pub fn authenticated(&self) -> bool {
        self.proof_verified && self.mac_engaged
    }

    /// May the Issue-30 verify-read ladder sample below 100 %?
    ///
    /// Only on an authenticated **and confidential** channel. Storage-trust
    /// authn strengthens plaintext (it can no longer be hijacked or
    /// forged) without making it private, and the verify-read is what
    /// bounds what a remote peer can publish — so plaintext keeps
    /// mandatory-100 % verify-reads exactly as VAL-6 left it.
    pub fn verify_sampling_admissible(&self) -> bool {
        self.authenticated() && self.channel.confidential()
    }

    /// The class name for logs and stats (`plaintext`, `mtls`, or
    /// `unauthenticated` while a rung is missing).
    pub fn class_name(&self) -> &'static str {
        if self.authenticated() {
            self.channel.name()
        } else {
            "unauthenticated"
        }
    }
}

/// The TLS **acceptor** for this wire — CA-pinned mTLS only.
///
/// A `ClusterSecurityConfig` without both halves of the CA pair is
/// **refused**: the node certificate the cluster machinery presents is
/// signed by the CA key, so a cert alone cannot produce an authenticated
/// channel, and a config with neither used to install an
/// accept-everything verifier. That path is deleted from the tree; on this
/// wire the honest alternative to mTLS is plaintext plus storage-trust
/// authn, which is authenticated.
pub fn tls_acceptor(security: &ClusterSecurityConfig) -> Result<tokio_rustls::TlsAcceptor> {
    let ca = require_ca(security)?;
    let cfg = rustls_server_config(&ca)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

/// The TLS **connector** for this wire — same refusal, dial side.
pub fn tls_connector(security: &ClusterSecurityConfig) -> Result<tokio_rustls::TlsConnector> {
    let ca = require_ca(security)?;
    let cfg = rustls_client_config(&ca)?;
    Ok(tokio_rustls::TlsConnector::from(Arc::new(cfg)))
}

fn require_ca(security: &ClusterSecurityConfig) -> Result<ClusterCa> {
    security.ca_pair().ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "cluster wire: TLS requires the cluster CA pair (ca_cert + ca_key) — a CA cert \
             with no key cannot sign this node's certificate, and a config with neither \
             would mean an accept-everything verifier, which this wire does not have. \
             Configure the CA, or run plaintext with storage-trust authentication."
                .into(),
        )
    })
}

// ---------------------------------------------------------------------------
// The pinned service pool (§6.7: never the conveyor's task)
// ---------------------------------------------------------------------------

thread_local! {
    /// The pool index of the current thread, or `usize::MAX` off-pool.
    /// Const-initialized so the probe never allocates.
    static SERVICE_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// The pool index of the calling thread, or `None` when the caller is not
/// a service thread. The venue probe: an owner-side RPC that observes
/// `None` is running somewhere it must not.
pub fn current_service_thread() -> Option<usize> {
    let idx = SERVICE_THREAD.with(|c| c.get());
    (idx != usize::MAX).then_some(idx)
}

/// Derived owner-side RPC lane count.
///
/// Resource caps derive from system resources (AGENTS): one lane per 8
/// cores, floored at 1 (a single-core box still owns its slots) and
/// ceilinged at 8 so a 256-core host does not spawn a lane farm for a
/// control plane. `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` overrides absolute
/// (the A/B lever), and never oversubscribes the box.
pub fn default_service_threads() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let derived = (cpus / 8).clamp(1, 8);
    let want = crate::env_knobs::opt_int_knob::<usize>("SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS")
        .unwrap_or(derived);
    want.clamp(1, cpus.max(1))
}

type PoolJob = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Per-lane submission depth. Bounded by law (no unbounded channels on a
/// reachable path): a full lane refuses the connection loudly instead of
/// growing without limit.
const POOL_LANE_DEPTH: usize = 1024;

/// The owner-side RPC venue: pinned, named OS threads, each running a
/// current-thread runtime with a `LocalSet`.
///
/// This is the `ipc_service.rs` pattern, and it exists for the reason
/// §6.5 item 1 states: the commit conveyor is a serialized ~0.78 ms
/// server at ρ ≈ 0.92, so an RPC dispatched onto its task multiplies
/// through the queueing formula. Connections shard across lanes by key,
/// and the thread names are load-bearing for `pidstat`/`perf` attribution
/// (the `fuse3-tpcN` lesson).
pub struct ServicePool {
    lanes: parking_lot::Mutex<Vec<tokio::sync::mpsc::Sender<PoolJob>>>,
    threads: parking_lot::Mutex<Vec<std::thread::JoinHandle<()>>>,
    width: usize,
    shutdown: AtomicBool,
}

impl std::fmt::Debug for ServicePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServicePool")
            .field("threads", &self.width)
            .field("shutdown", &self.shutdown.load(Ordering::Relaxed))
            .finish()
    }
}

impl ServicePool {
    /// Start `threads` pinned lanes named `{prefix}{index}`.
    pub fn start(prefix: &'static str, threads: usize) -> std::io::Result<Arc<Self>> {
        let width = threads.max(1);
        let nodes = crate::numa_core::topology().owner_nodes(width);
        let mut lanes = Vec::with_capacity(width);
        let mut handles = Vec::with_capacity(width);
        for idx in 0..width {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<PoolJob>(POOL_LANE_DEPTH);
            let node = nodes.get(idx).copied();
            let handle = std::thread::Builder::new()
                .name(format!("{prefix}{idx}"))
                .spawn(move || {
                    SERVICE_THREAD.with(|c| c.set(idx));
                    // NUMA: the same CPU-weighted partition the IPC host
                    // pins to (∩ process mask, never widened). Gated
                    // internally — a single-node host is a no-op.
                    if let Some(node) = node {
                        crate::numa::pin_service_thread(node);
                    }
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            log::error!("cluster wire: service lane {idx} has no runtime: {e}");
                            return;
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        while let Some(job) = rx.recv().await {
                            tokio::task::spawn_local(job);
                        }
                    });
                })?;
            lanes.push(tx);
            handles.push(handle);
        }
        Ok(Arc::new(Self {
            lanes: parking_lot::Mutex::new(lanes),
            threads: parking_lot::Mutex::new(handles),
            width,
            shutdown: AtomicBool::new(false),
        }))
    }

    /// Lane count.
    pub fn threads(&self) -> usize {
        self.width
    }

    /// Run `fut` on the lane `key` shards to. Refuses (rather than
    /// growing) when the lane's bounded queue is full, and after
    /// shutdown.
    pub fn spawn_on<F>(&self, key: u64, fut: F) -> std::io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(std::io::Error::other(
                "cluster wire: service pool is shut down",
            ));
        }
        let idx = (key % self.width as u64) as usize;
        let lane = {
            let lanes = self.lanes.lock();
            lanes.get(idx).cloned()
        };
        let Some(lane) = lane else {
            return Err(std::io::Error::other(
                "cluster wire: service pool has no lanes",
            ));
        };
        lane.try_send(Box::pin(fut)).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => std::io::Error::other(format!(
                "cluster wire: service lane {idx} is at its {POOL_LANE_DEPTH}-job depth"
            )),
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                std::io::Error::other(format!("cluster wire: service lane {idx} is gone"))
            }
        })
    }

    /// Stop the lanes and join them.
    ///
    /// Dropping the senders is what ends each lane's receive loop; the
    /// `LocalSet` then aborts whatever it still holds. Joining OS threads
    /// blocks the caller (RES-12's teardown class), so an async caller
    /// that cares should hop through `spawn_blocking`.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.lanes.lock().clear();
        let handles: Vec<_> = std::mem::take(&mut *self.threads.lock());
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for ServicePool {
    fn drop(&mut self) {
        if !self.shutdown.load(Ordering::SeqCst) {
            self.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// DISC-1 — peer auto-discovery over the shared volume
// ---------------------------------------------------------------------------

/// One discovered cluster peer. Auto-discovered, never configured (ruling
/// D2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterPeer {
    /// The mount registration's uuid.
    pub id: String,
    /// The `ip:port` the peer publishes.
    pub endpoint: String,
    /// Heartbeat age at the read instant.
    pub age_secs: Option<u64>,
    /// Live under the ONE staleness law
    /// (`fuse_client::CLIENT_STALE_TTL_SECS`).
    pub fresh: bool,
}

/// The DISC-1 projection: `client:{uuid}` registrations → peers.
///
/// Pure, so the whole selection law is testable without a volume set:
/// **client** records only (a `writer_claim` is a guard, not a peer),
/// **fresh** heartbeats only, an endpoint required (a non-coordinator
/// mount publishes none), `self_id` excluded, deduplicated by id —
/// records are per-volume, so a set of N volumes shows the same peer N
/// times — and ordered by id, so discovery is deterministic rather than
/// dependent on xattr enumeration order.
pub fn peers_from_registrations(
    regs: &[crate::meta_backend::kv::backend::MountRegistration],
    self_id: Option<&str>,
) -> Vec<ClusterPeer> {
    let mut by_id: std::collections::BTreeMap<String, ClusterPeer> =
        std::collections::BTreeMap::new();
    for r in regs {
        if r.kind != "client" || !r.heartbeat_fresh {
            continue;
        }
        if self_id.is_some_and(|s| s == r.id) {
            continue;
        }
        let Some(endpoint) = r.job_endpoint.as_ref() else {
            continue;
        };
        by_id.entry(r.id.clone()).or_insert_with(|| ClusterPeer {
            id: r.id.clone(),
            endpoint: endpoint.clone(),
            age_secs: r.age_secs,
            fresh: r.heartbeat_fresh,
        });
    }
    by_id.into_values().collect()
}

/// Enumerate the cluster's live peers off the shared metadata volume set —
/// **the volume IS the rendezvous**, so there is no discovery protocol to
/// run, no multicast group to join and no seed list to configure.
///
/// Read-only and safe on probe backends. Cost note (§6.5 item 3): this
/// interim form pays `listxattr(1)` plus one `getxattr` per record per
/// volume, i.e. it rides the same ino-1 hotspot the heartbeat plane
/// already saturates at ~4,550 clients. **S6** moves membership onto
/// lease-based liveness and this function moves with it — discovery then
/// adds zero load to that hotspot. Until S6, callers should discover on
/// demand (dial, reconnect) and never on a per-operation path.
pub async fn discover_peers(
    meta: &Arc<RoutedMetaBackend>,
    self_id: Option<&str>,
) -> Vec<ClusterPeer> {
    let mut regs = Vec::new();
    for vol in &meta.volumes {
        regs.extend(vol.mount_registrations().await);
    }
    peers_from_registrations(&regs, self_id)
}

/// The address this node publishes in its mount registration: the
/// interface the default route would use (a UDP connect for the route
/// lookup — no packet is sent), falling back to loopback on isolated
/// boxes, where same-host peers still reach it. The publication half of
/// DISC-1: a peer is discoverable because it wrote where to reach it, not
/// because anyone configured it.
pub fn local_advertise_ip() -> std::net::IpAddr {
    std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|s| {
            s.connect(("192.0.2.1", 9))?; // TEST-NET-1: route lookup only
            Ok(s.local_addr()?.ip())
        })
        .unwrap_or_else(|_| std::net::IpAddr::from([127, 0, 0, 1]))
}

/// The live coordinator's endpoint (the first discovered peer publishing
/// one). `None` ⇒ no live mount publishes an endpoint on this volume set.
pub async fn discover_endpoint(meta: &Arc<RoutedMetaBackend>) -> Option<String> {
    discover_peers(meta, None)
        .await
        .into_iter()
        .next()
        .map(|p| p.endpoint)
}

// ---------------------------------------------------------------------------
// The RPC surface (S4 plugs its verbs in here)
// ---------------------------------------------------------------------------

/// The one verb S3 ships: an authenticated echo, which is what makes
/// [`measure_rtt`] a measurement of THIS wire rather than of a mock. S4's
/// lock verbs take numbers above it.
pub const VERB_PING: u16 = 0;

/// Status: the call succeeded.
pub const RPC_OK: u16 = 0;
/// Status: the verb is not implemented by this owner.
pub const RPC_UNKNOWN_VERB: u16 = 1;

/// One RPC request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcRequest {
    /// Client-chosen correlation id, echoed in the reply.
    pub id: u64,
    /// The verb.
    pub verb: u16,
    /// Verb-defined body.
    pub body: Vec<u8>,
}

/// One RPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse {
    /// The request's correlation id.
    pub id: u64,
    /// [`RPC_OK`] or a verb-defined status.
    pub status: u16,
    /// Verb-defined body.
    pub body: Vec<u8>,
}

/// Every frame this wire's RPC vocabulary can carry — handshake and
/// session in ONE enum, so a reader can decode whatever arrives next
/// without a mode flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcFrame {
    /// Coordinator → peer, first frame on the connection.
    Challenge {
        /// The gate's schema.
        schema: u32,
        /// Server-issued single-use nonce.
        server_nonce: String,
        /// Freshness window, ms.
        freshness_ms: u64,
    },
    /// Peer → coordinator: the storage-membership proof.
    Prove {
        /// The peer's schema.
        schema: u32,
        /// The peer's identity.
        peer_id: String,
        /// The nonce from this connection's challenge.
        server_nonce: String,
        /// The peer's entropy contribution.
        peer_nonce: String,
        /// Hex [`proof_mac`].
        mac: String,
    },
    /// Coordinator → peer: admitted. Every frame after this one carries a
    /// session MAC.
    Admitted {
        /// The gate's schema.
        schema: u32,
    },
    /// Coordinator → peer: refused, with the reason.
    Refused {
        /// Operator-facing reason.
        reason: String,
    },
    /// Peer → coordinator, authenticated.
    Call {
        /// Correlation id.
        id: u64,
        /// The verb.
        verb: u16,
        /// Verb-defined body.
        body: Vec<u8>,
    },
    /// Coordinator → peer, authenticated.
    Reply {
        /// Correlation id.
        id: u64,
        /// Status.
        status: u16,
        /// Verb-defined body.
        body: Vec<u8>,
    },
}

/// An owner-side RPC implementation.
///
/// `call` is **synchronous by contract**, and runs on a pinned
/// [`ServicePool`] lane. §6.7's arbitration is RAM-only — an `scc` probe
/// plus one atomic — so synchronous is the right shape; anything that must
/// await (a metadata commit, a device barrier) belongs on an explicit
/// handoff to the runtime, exactly as `ipc_service` hands off, and must
/// never be awaited inline on a lane.
pub trait RpcService: Send + Sync + 'static {
    /// Serve one request.
    fn call(&self, req: RpcRequest) -> RpcResponse;
}

/// An owner-side RPC implementation whose verbs must **await** — DLM
/// stage **S8**'s function-shipped metadata being the first one (spec
/// §6.7 decision 1: a shipped verb runs the owner's ordinary `Metadata`
/// call, which commits through the M7 conveyor and therefore parks on a
/// oneshot).
///
/// This is the *explicit handoff* [`RpcService`]'s contract names, made a
/// type instead of a convention. The venue rule is unchanged and is what
/// the split protects: the future is polled on the pinned lane that
/// received the frame (never on the conveyor's task), and an
/// implementation that needs a different runtime for its work — S8's
/// service hands the batch to the runtime that owns the backend's tasks —
/// performs that hop itself, visibly, rather than having the transport
/// guess.
pub trait RpcAsyncService: Send + Sync + 'static {
    /// Serve one request, asynchronously.
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>>;
}

/// Which arm a listener serves: the synchronous lock-verb shape or the
/// awaiting metadata shape. One enum rather than two listeners, so the
/// accept loop, the authn gate, the bounds and the counters have exactly
/// one implementation.
enum ServiceArm {
    Sync(Arc<dyn RpcService>),
    Async(Arc<dyn RpcAsyncService>),
}

impl ServiceArm {
    async fn call(&self, req: RpcRequest) -> RpcResponse {
        match self {
            ServiceArm::Sync(svc) => svc.call(req),
            ServiceArm::Async(svc) => svc.call(req).await,
        }
    }
}

/// The built-in ping service: the RTT instrument's server half, and the
/// reference shape for S4's implementations (the listener owns the
/// `requests_served` gauge, so the service itself counts nothing).
#[derive(Debug)]
pub struct PingService;

impl RpcService for PingService {
    fn call(&self, req: RpcRequest) -> RpcResponse {
        if req.verb != VERB_PING {
            return RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: Vec::new(),
            };
        }
        RpcResponse {
            id: req.id,
            status: RPC_OK,
            body: req.body,
        }
    }
}

/// Listener configuration. Defaults are ruling **D2**'s posture: the
/// listener is configurable and **open by default** on an ephemeral port,
/// because peers are auto-discovered — which is exactly why every bound
/// below is non-negotiable.
#[derive(Debug, Clone)]
pub struct RpcListenerConfig {
    /// Bind address. `0.0.0.0:0` = D2's mount posture (published through
    /// the mount registration).
    pub bind_addr: SocketAddr,
    /// CA-pinned mTLS when set. A CA-less config is refused, never
    /// downgraded.
    pub security: Option<ClusterSecurityConfig>,
    /// Concurrent-connection cap.
    pub max_connections: usize,
    /// Deadline covering everything an unauthenticated peer does.
    pub handshake_timeout: Duration,
    /// Deadline for an authenticated frame body once its prefix arrived.
    pub frame_body_timeout: Duration,
    /// Idle bound on an admitted session.
    pub session_idle_timeout: Duration,
    /// Challenge freshness window.
    pub enroll_freshness: Duration,
    /// Owner-side RPC lanes.
    pub service_threads: usize,
}

impl Default for RpcListenerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().expect("literal addr"),
            security: None,
            max_connections: default_max_connections(),
            handshake_timeout: Duration::from_secs(10),
            frame_body_timeout: Duration::from_secs(30),
            session_idle_timeout: Duration::from_secs(60),
            enroll_freshness: Duration::from_secs(30),
            service_threads: default_service_threads(),
        }
    }
}

/// Default concurrent-connection cap: derived from the core count (caps
/// derive from system resources), floored so a small box still admits a
/// real peer population and ceilinged so a large one still has a bound.
pub fn default_max_connections() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .saturating_mul(16)
        .clamp(64, 1024)
}

/// Listener counters. `mac_failures` and `service_refusals` are
/// must-stay-0 tripwires on a healthy cluster; `accept_backoffs` is the
/// fd-exhaustion gauge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RpcStats {
    /// Live accepted connections.
    pub live_connections: u64,
    /// Connections closed unserved at the cap.
    pub connections_refused: u64,
    /// Accept-error backoff engagements.
    pub accept_backoffs: u64,
    /// Sessions admitted by the authn gate.
    pub sessions_admitted: u64,
    /// Handshakes refused (bad proof, stale/replayed nonce, schema, …).
    pub admissions_refused: u64,
    /// Authenticated requests served.
    pub requests_served: u64,
    /// Per-frame MAC verification failures (tamper/reorder/replay).
    pub mac_failures: u64,
    /// Connections dropped because a service lane was full.
    pub service_refusals: u64,
    /// Outstanding challenges.
    pub outstanding_challenges: u64,
}

/// The concurrent-connection budget: one implementation for every
/// listener on this wire (VAL-6's bound, carried forward rather than
/// re-derived per protocol).
#[derive(Debug, Clone)]
pub struct ConnGate {
    live: Arc<AtomicU64>,
    cap: usize,
}

impl ConnGate {
    /// A gate admitting at most `cap` concurrent connections.
    pub fn new(cap: usize) -> Self {
        Self {
            live: Arc::new(AtomicU64::new(0)),
            cap: cap.max(1),
        }
    }

    /// The cap.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Live admitted connections.
    pub fn live(&self) -> u64 {
        self.live.load(Ordering::SeqCst)
    }

    /// Claim a slot, or `None` at the cap. Claim BEFORE spawning anything:
    /// an over-cap peer must cost one accept and one close, never a task
    /// or a buffer.
    pub fn try_admit(&self) -> Option<ConnPermit> {
        let cap = self.cap as u64;
        let mut live = self.live.load(Ordering::SeqCst);
        loop {
            if live >= cap {
                return None;
            }
            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ConnPermit {
                        live: Arc::clone(&self.live),
                    })
                }
                Err(observed) => live = observed,
            }
        }
    }
}

/// One admitted connection's slot: RAII, so every exit path (TLS failure,
/// handshake timeout, clean departure, panic) returns it.
#[derive(Debug)]
pub struct ConnPermit {
    live: Arc<AtomicU64>,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A type-erased duplex byte stream: plaintext TCP or CA-pinned mTLS. One
/// definition, so every protocol on this wire (RPC verbs, job shards) is
/// carried by the same stream type instead of each declaring its own.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

/// The boxed form of [`Duplex`].
pub type ClusterStream = Box<dyn Duplex>;

struct ListenerCounters {
    conns: ConnGate,
    refused: AtomicU64,
    backoffs: AtomicU64,
    admitted: AtomicU64,
    admissions_refused: AtomicU64,
    served: AtomicU64,
    mac_failures: AtomicU64,
    service_refusals: AtomicU64,
}

/// The cluster-wire RPC listener: bounded accept loop + zero-config
/// mutual authn + authenticated session loops, every one of them on a
/// pinned [`ServicePool`] lane.
pub struct RpcListener {
    cfg: RpcListenerConfig,
    endpoint: SocketAddr,
    gate: Arc<AuthnGate>,
    service: ServiceArm,
    pool: Arc<ServicePool>,
    counters: Arc<ListenerCounters>,
    channel: ChannelClass,
    tls: Option<tokio_rustls::TlsAcceptor>,
    next_conn: AtomicU64,
    shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for RpcListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcListener")
            .field("endpoint", &self.endpoint)
            .field("channel", &self.channel.name())
            .field("lanes", &self.pool.threads())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl RpcListener {
    /// Bind and start serving. Synchronous: the listener owns its own
    /// pinned lanes, so it does not need — and deliberately does not
    /// borrow — the caller's runtime for its work.
    pub fn start(
        cfg: RpcListenerConfig,
        secret: Vec<u8>,
        service: Arc<dyn RpcService>,
    ) -> Result<Arc<Self>> {
        Self::start_arm(cfg, secret, ServiceArm::Sync(service))
    }

    /// [`Self::start`] serving an **awaiting** service (DLM S8's
    /// function-shipped metadata). Same accept loop, same authn gate,
    /// same bounds and same counters — only the call shape differs.
    pub fn start_async(
        cfg: RpcListenerConfig,
        secret: Vec<u8>,
        service: Arc<dyn RpcAsyncService>,
    ) -> Result<Arc<Self>> {
        Self::start_arm(cfg, secret, ServiceArm::Async(service))
    }

    fn start_arm(
        cfg: RpcListenerConfig,
        secret: Vec<u8>,
        service: ServiceArm,
    ) -> Result<Arc<Self>> {
        let (tls, channel) = match cfg.security.as_ref() {
            Some(sec) => (Some(tls_acceptor(sec)?), ChannelClass::MutualTls),
            None => (None, ChannelClass::Plaintext),
        };
        let std_listener = std::net::TcpListener::bind(cfg.bind_addr).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "cluster wire: bind {} failed: {e}",
                cfg.bind_addr
            ))
        })?;
        std_listener.set_nonblocking(true).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("cluster wire: set_nonblocking failed: {e}"))
        })?;
        let endpoint = std_listener.local_addr().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("cluster wire: local_addr failed: {e}"))
        })?;
        let pool = ServicePool::start("sqz-cluster-svc", cfg.service_threads).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("cluster wire: service pool failed: {e}"))
        })?;
        let cfg_max_conns = cfg.max_connections;
        let gate = Arc::new(AuthnGate::new(
            secret,
            AuthnConfig {
                freshness: cfg.enroll_freshness,
                nonce_cap: cfg.max_connections.saturating_mul(4).max(64),
                ..AuthnConfig::default()
            },
        ));
        let host = Arc::new(Self {
            cfg,
            endpoint,
            gate,
            service,
            pool,
            counters: Arc::new(ListenerCounters {
                conns: ConnGate::new(cfg_max_conns),
                refused: AtomicU64::new(0),
                backoffs: AtomicU64::new(0),
                admitted: AtomicU64::new(0),
                admissions_refused: AtomicU64::new(0),
                served: AtomicU64::new(0),
                mac_failures: AtomicU64::new(0),
                service_refusals: AtomicU64::new(0),
            }),
            channel,
            tls,
            next_conn: AtomicU64::new(1),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        log::info!(
            "cluster wire: listener {endpoint} ({}) — {} lanes, max {} connections, \
             {HANDSHAKE_MAX_FRAME_BYTES} B pre-authn frame cap, handshake deadline {:?}, \
             challenge freshness {:?}; every admitted frame carries a session MAC derived \
             from the shared volume's job:enroll secret",
            host.channel.name(),
            host.pool.threads(),
            host.cfg.max_connections,
            host.cfg.handshake_timeout,
            host.cfg.enroll_freshness,
        );
        let accept_host = Arc::clone(&host);
        host.pool
            .spawn_on(0, async move {
                accept_host.accept_loop(std_listener).await;
            })
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("cluster wire: accept lane refused: {e}"))
            })?;
        Ok(host)
    }

    /// The bound address.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// The byte-stream class this listener serves.
    pub fn channel(&self) -> ChannelClass {
        self.channel
    }

    /// Counters.
    pub fn stats(&self) -> RpcStats {
        let c = &self.counters;
        RpcStats {
            live_connections: c.conns.live(),
            connections_refused: c.refused.load(Ordering::SeqCst),
            accept_backoffs: c.backoffs.load(Ordering::SeqCst),
            sessions_admitted: c.admitted.load(Ordering::SeqCst),
            admissions_refused: c.admissions_refused.load(Ordering::SeqCst),
            requests_served: c.served.load(Ordering::SeqCst),
            mac_failures: c.mac_failures.load(Ordering::SeqCst),
            service_refusals: c.service_refusals.load(Ordering::SeqCst),
            outstanding_challenges: self.gate.outstanding_challenges() as u64,
        }
    }

    /// Stop accepting, end the sessions, and join the lanes.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.pool.shutdown();
    }

    async fn accept_loop(self: Arc<Self>, std_listener: std::net::TcpListener) {
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                log::error!("cluster wire: listener adoption failed: {e}");
                return;
            }
        };
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
                    // A bare `continue` here turns a persistent
                    // EMFILE/ENFILE condition into a busy loop.
                    let d = next_accept_backoff(backoff);
                    backoff = Some(d);
                    self.counters.backoffs.fetch_add(1, Ordering::SeqCst);
                    log::warn!("cluster wire: accept failed: {e} — backing off {d:?}");
                    tokio::time::sleep(d).await;
                    continue;
                }
            };
            // Claim the connection slot BEFORE anything is spawned:
            // over-cap peers cost one accept and one close, never a task
            // or a buffer.
            let Some(permit) = self.counters.conns.try_admit() else {
                self.counters.refused.fetch_add(1, Ordering::SeqCst);
                log::warn!(
                    "cluster wire: refusing {peer} — {} concurrent connections is the cap",
                    self.cfg.max_connections
                );
                drop(tcp);
                continue;
            };
            let conn_id = self.next_conn.fetch_add(1, Ordering::SeqCst);
            let host = Arc::clone(&self);
            if let Err(e) = self.pool.spawn_on(conn_id, async move {
                let _permit = permit;
                host.serve_conn(tcp, peer).await;
            }) {
                self.counters
                    .service_refusals
                    .fetch_add(1, Ordering::SeqCst);
                log::warn!("cluster wire: dropping {peer} — {e}");
            }
        }
    }

    async fn serve_conn(self: &Arc<Self>, tcp: tokio::net::TcpStream, peer: SocketAddr) {
        let deadline = self.cfg.handshake_timeout;
        // The TLS handshake is attacker-paced: bound it.
        let (mut stream, binding): (ClusterStream, Option<[u8; 32]>) = match self.tls.clone() {
            Some(acceptor) => match tokio::time::timeout(deadline, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => {
                    let binding = server_exporter(s.get_ref().1);
                    (Box::new(s), binding)
                }
                Ok(Err(e)) => {
                    log::warn!("cluster wire: TLS handshake with {peer} failed: {e}");
                    return;
                }
                Err(_) => {
                    log::warn!("cluster wire: TLS handshake with {peer} exceeded {deadline:?}");
                    return;
                }
            },
            None => (Box::new(tcp), None),
        };

        // The coordinator speaks first (the peer cannot choose its own
        // challenge, so a captured proof is not a credential).
        let challenge = self.gate.issue_challenge();
        let frame = RpcFrame::Challenge {
            schema: challenge.schema,
            server_nonce: challenge.server_nonce.clone(),
            freshness_ms: challenge.freshness_ms,
        };
        if tokio::time::timeout(
            deadline,
            write_plain_frame(&mut stream, FrameClass::Handshake, &frame),
        )
        .await
        .is_err()
        {
            log::warn!("cluster wire: {peer}: challenge write stalled past {deadline:?}");
            return;
        }

        let proof = match tokio::time::timeout(
            deadline,
            read_plain_frame::<_, RpcFrame>(
                &mut stream,
                FrameClass::Handshake.cap(),
                Some(deadline),
            ),
        )
        .await
        {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                self.counters
                    .admissions_refused
                    .fetch_add(1, Ordering::SeqCst);
                log::warn!("cluster wire: {peer}: undecodable proof frame: {e}");
                return;
            }
            Err(_) => {
                log::warn!("cluster wire: {peer}: no proof within {deadline:?} — dropped");
                return;
            }
        };
        let RpcFrame::Prove {
            schema,
            peer_id,
            server_nonce,
            peer_nonce,
            mac,
        } = proof
        else {
            self.counters
                .admissions_refused
                .fetch_add(1, Ordering::SeqCst);
            let _ = write_plain_frame(
                &mut stream,
                FrameClass::Handshake,
                &RpcFrame::Refused {
                    reason: "expected Prove".into(),
                },
            )
            .await;
            return;
        };
        let verdict = self.gate.verify(
            ProofClaim {
                schema,
                peer_id: &peer_id,
                server_nonce: &server_nonce,
                peer_nonce: &peer_nonce,
                mac: &mac,
            },
            binding.as_ref().map(|b| &b[..]),
        );
        let key = match verdict {
            Verdict::Admit(key) => key,
            Verdict::Refuse(reason) => {
                self.counters
                    .admissions_refused
                    .fetch_add(1, Ordering::SeqCst);
                log::warn!("cluster wire: {peer}: peer '{peer_id}' refused: {reason}");
                let _ = write_plain_frame(
                    &mut stream,
                    FrameClass::Handshake,
                    &RpcFrame::Refused { reason },
                )
                .await;
                return;
            }
        };
        if write_plain_frame(
            &mut stream,
            FrameClass::Handshake,
            &RpcFrame::Admitted {
                schema: CLUSTER_WIRE_SCHEMA,
            },
        )
        .await
        .is_err()
        {
            return;
        }
        self.counters.admitted.fetch_add(1, Ordering::SeqCst);
        let authn = SessionAuthn {
            channel: self.channel,
            proof_verified: true,
            mac_engaged: true,
        };
        log::info!(
            "cluster wire: peer '{peer_id}' admitted from {peer} ({}, storage-trust proof, \
             per-frame MAC{})",
            authn.class_name(),
            if binding.is_some() {
                ", exporter-bound"
            } else {
                ""
            }
        );

        // Authenticated session loop. Every frame is MAC'd, so the
        // session cannot be hijacked, reordered or replayed mid-flight.
        let (mut tx, mut rx) = session_framers(&key, Role::Coordinator);
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let frame = match tokio::time::timeout(
                self.cfg.session_idle_timeout,
                rx.recv::<_, RpcFrame>(
                    &mut stream,
                    FrameClass::Bulk.cap(),
                    Some(self.cfg.frame_body_timeout),
                ),
            )
            .await
            {
                Ok(Ok(Some(f))) => f,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    if e.to_string().contains("mac") {
                        self.counters.mac_failures.fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "cluster wire: peer '{peer_id}' frame failed authentication \
                             ({e}) — closing the session"
                        );
                    } else {
                        log::warn!("cluster wire: peer '{peer_id}' session read error: {e}");
                    }
                    break;
                }
                Err(_) => {
                    log::warn!(
                        "cluster wire: peer '{peer_id}' sent no frame within {:?} — closing \
                         the idle session",
                        self.cfg.session_idle_timeout
                    );
                    break;
                }
            };
            let RpcFrame::Call { id, verb, body } = frame else {
                log::warn!("cluster wire: peer '{peer_id}' sent a non-Call frame — ignored");
                continue;
            };
            // The service runs HERE — on this pinned lane, which is the
            // whole point of §6.7's venue rule. An awaiting arm (S8's
            // metadata verbs) yields on this lane's `LocalSet`, so the
            // lane keeps serving its other sessions while one verb's
            // commit is in flight; it never migrates the work onto the
            // conveyor's task.
            let reply = self.service.call(RpcRequest { id, verb, body }).await;
            self.counters.served.fetch_add(1, Ordering::SeqCst);
            if tx
                .send(
                    &mut stream,
                    FrameClass::Bulk,
                    &RpcFrame::Reply {
                        id: reply.id,
                        status: reply.status,
                        body: reply.body,
                    },
                )
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

/// The RFC 5705 exporter label both ends use. Mixing the exporter output
/// into the session key makes the per-frame MAC **channel-bound**, so a
/// session key cannot be lifted onto a different TLS connection.
const EXPORTER_LABEL: &[u8] = b"squeezefs-cluster-wire/v1/binding";

/// Server-side exporter capture (the job-shard listener uses the same
/// binding as the RPC listener — one channel-binding law).
pub fn server_exporter(conn: &rustls::ServerConnection) -> Option<[u8; 32]> {
    conn.export_keying_material([0u8; 32], EXPORTER_LABEL, None)
        .ok()
}

/// Dial-side exporter capture.
pub fn client_exporter(conn: &rustls::ClientConnection) -> Option<[u8; 32]> {
    conn.export_keying_material([0u8; 32], EXPORTER_LABEL, None)
        .ok()
}

// ---------------------------------------------------------------------------
// The dial side
// ---------------------------------------------------------------------------

/// A dialed, authenticated cluster-wire session.
pub struct RpcClient {
    stream: ClusterStream,
    tx: FrameTx,
    rx: FrameRx,
    authn: SessionAuthn,
    next_id: u64,
    call_timeout: Duration,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("authn", &self.authn)
            .field("calls", &self.next_id)
            .finish_non_exhaustive()
    }
}

/// Bound on the dial-side handshake and on one call's reply.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

impl RpcClient {
    /// Dial, prove storage membership against the coordinator's
    /// **server-issued** challenge, derive the session key, and return the
    /// authenticated session.
    pub async fn connect(
        endpoint: &str,
        secret: &[u8],
        peer_id: &str,
        security: Option<&ClusterSecurityConfig>,
    ) -> Result<Self> {
        let tcp = tokio::net::TcpStream::connect(endpoint).await?;
        let (mut stream, binding): (ClusterStream, Option<[u8; 32]>) = match security {
            Some(sec) => {
                let connector = tls_connector(sec)?;
                // The ClusterSecurityConfig node certs carry
                // localhost/127.0.0.1 SANs (cluster_tls construction).
                let name = rustls::pki_types::ServerName::try_from("localhost")
                    .expect("literal server name")
                    .to_owned();
                let tls = connector.connect(name, tcp).await?;
                let binding = client_exporter(tls.get_ref().1);
                (Box::new(tls), binding)
            }
            None => (Box::new(tcp), None),
        };
        let channel = if security.is_some() {
            ChannelClass::MutualTls
        } else {
            ChannelClass::Plaintext
        };

        let server_nonce = match tokio::time::timeout(
            DIAL_TIMEOUT,
            read_plain_frame::<_, RpcFrame>(
                &mut stream,
                FrameClass::Handshake.cap(),
                Some(DIAL_TIMEOUT),
            ),
        )
        .await
        .map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "cluster wire: no challenge from the coordinator within {DIAL_TIMEOUT:?}"
            ))
        })?? {
            Some(RpcFrame::Challenge {
                schema,
                server_nonce,
                ..
            }) => {
                if schema != CLUSTER_WIRE_SCHEMA {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "cluster wire: coordinator speaks schema {schema}, this build speaks \
                         {CLUSTER_WIRE_SCHEMA}"
                    )));
                }
                server_nonce
            }
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "cluster wire: expected a Challenge first, got {other:?}"
                )))
            }
        };
        let peer_nonce = uuid::Uuid::new_v4().to_string();
        write_plain_frame(
            &mut stream,
            FrameClass::Handshake,
            &RpcFrame::Prove {
                schema: CLUSTER_WIRE_SCHEMA,
                peer_id: peer_id.to_string(),
                server_nonce: server_nonce.clone(),
                peer_nonce: peer_nonce.clone(),
                mac: proof_mac(secret, peer_id, &server_nonce, &peer_nonce),
            },
        )
        .await?;
        match read_plain_frame::<_, RpcFrame>(
            &mut stream,
            FrameClass::Handshake.cap(),
            Some(DIAL_TIMEOUT),
        )
        .await?
        {
            Some(RpcFrame::Admitted { .. }) => {}
            Some(RpcFrame::Refused { reason }) => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "cluster wire: enrollment refused by the coordinator: {reason}"
                )))
            }
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "cluster wire: unexpected admission reply: {other:?}"
                )))
            }
        }
        let key = session_key(
            secret,
            peer_id,
            &server_nonce,
            &peer_nonce,
            binding.as_ref().map(|b| &b[..]),
        );
        let (tx, rx) = session_framers(&key, Role::Peer);
        Ok(Self {
            stream,
            tx,
            rx,
            authn: SessionAuthn {
                channel,
                proof_verified: true,
                mac_engaged: true,
            },
            next_id: 0,
            call_timeout: DIAL_TIMEOUT,
        })
    }

    /// This session's authentication state.
    pub fn authn(&self) -> &SessionAuthn {
        &self.authn
    }

    /// Issue one authenticated request and await its reply.
    pub async fn call(&mut self, verb: u16, body: Vec<u8>) -> Result<RpcResponse> {
        self.next_id += 1;
        let id = self.next_id;
        self.tx
            .send(
                &mut self.stream,
                FrameClass::Bulk,
                &RpcFrame::Call { id, verb, body },
            )
            .await?;
        let frame = tokio::time::timeout(
            self.call_timeout,
            self.rx.recv::<_, RpcFrame>(
                &mut self.stream,
                FrameClass::Bulk.cap(),
                Some(self.call_timeout),
            ),
        )
        .await
        .map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "cluster wire: no reply to call {id} within {:?}",
                self.call_timeout
            ))
        })??;
        match frame {
            Some(RpcFrame::Reply {
                id: reply_id,
                status,
                body,
            }) => {
                if reply_id != id {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "cluster wire: reply id {reply_id} does not match call {id}"
                    )));
                }
                Ok(RpcResponse {
                    id: reply_id,
                    status,
                    body,
                })
            }
            Some(other) => Err(SqueezefsError::InvalidOperation(format!(
                "cluster wire: unexpected frame in reply position: {other:?}"
            ))),
            None => Err(SqueezefsError::InvalidOperation(
                "cluster wire: the coordinator closed the session".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The RTT instrument (prices S8 — risk R1, ruling D10)
// ---------------------------------------------------------------------------

/// A counted round-trip-time row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RttReport {
    /// Samples taken (excluding the discarded warm-up).
    pub samples: usize,
    /// Payload bytes per direction.
    pub payload_bytes: usize,
    /// Fastest sample.
    pub min_us: f64,
    /// Median — the figure to quote.
    pub median_us: f64,
    /// 99th percentile.
    pub p99_us: f64,
    /// Slowest sample.
    pub max_us: f64,
}

/// Measure the authenticated request/response RTT on THIS wire.
///
/// This is the number that prices **S8** (spec §6.10 risk **R1**, accepted
/// as ruling **D10**): a function-shipped metadata operation costs one of
/// these round trips *plus* the owner's own work, and §6.5 item 1's
/// arithmetic — a 250 µs fabric RTT taking creates from 9,090/s to
/// 2,778/s — is what makes the measured value a go/no-go input rather
/// than a curiosity.
///
/// **Label discipline**: a loopback run measures framing, authentication
/// and wake with the fabric term at ~0, and is therefore a **floor**,
/// never a fabric row. A fabric row needs the venue and the standing
/// measurement rules (two-substrate, A-B-B-A, sustained ≥ 60 s) — see
/// `.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`.
pub async fn measure_rtt(
    endpoint: &str,
    secret: &[u8],
    peer_id: &str,
    samples: usize,
    payload_bytes: usize,
) -> Result<RttReport> {
    let mut client = RpcClient::connect(endpoint, secret, peer_id, None).await?;
    let body = vec![0xa5u8; payload_bytes];
    // One discarded warm-up: the first call pays TCP/TLS window and page
    // faults that no steady-state operation pays.
    client.call(VERB_PING, body.clone()).await?;
    let mut us: Vec<f64> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = std::time::Instant::now();
        let reply = client.call(VERB_PING, body.clone()).await?;
        us.push(started.elapsed().as_secs_f64() * 1e6);
        if reply.status != RPC_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cluster wire: rtt probe got status {}",
                reply.status
            )));
        }
    }
    us.sort_by(|a, b| a.partial_cmp(b).expect("finite timings"));
    let pick = |q: f64| -> f64 {
        if us.is_empty() {
            return 0.0;
        }
        let idx = ((us.len() as f64 - 1.0) * q).round() as usize;
        us[idx.min(us.len() - 1)]
    };
    Ok(RttReport {
        samples: us.len(),
        payload_bytes,
        min_us: pick(0.0),
        median_us: pick(0.5),
        p99_us: pick(0.99),
        max_us: pick(1.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0x0f, 0xf0, 0xff];
        assert_eq!(hex_decode(&hex_encode(&bytes)), Some(bytes.to_vec()));
        assert_eq!(hex_decode("odd"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn the_proof_mac_is_the_shipped_enrollment_proof() {
        // The job wire's enrollment proof and this one are the SAME
        // function: the port must not change what a worker computes.
        let a = proof_mac(b"secret", "w-1", "s-nonce", "p-nonce");
        assert_eq!(a.len(), 64, "hex HMAC-SHA256");
        assert!(mac_eq(
            &a,
            &proof_mac(b"secret", "w-1", "s-nonce", "p-nonce")
        ));
        assert!(!mac_eq(
            &a,
            &proof_mac(b"secret", "w-2", "s-nonce", "p-nonce")
        ));
    }

    #[test]
    fn every_class_cap_is_a_multiple_of_the_commit_chunk() {
        for class in [FrameClass::Handshake, FrameClass::Control, FrameClass::Bulk] {
            assert!(class.cap() > 0);
        }
        assert!(FRAME_CHUNK_BYTES <= CONTROL_MAX_FRAME_BYTES as usize);
    }
}
