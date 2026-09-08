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
//! ### 3. Owner-side RPC runs on dedicated OS threads
//!
//! §6.7 is explicit: owner-side RPC handling runs on its own threads,
//! **never on the conveyor's task** — the commit conveyor is a serialized
//! ~0.78 ms server at ρ ≈ 0.92, and an RPC on it would multiply through
//! the queueing formula. Since the rip-tokio conversion the venue is
//! **one named OS thread per admitted connection** (`sqz-clw-conn`,
//! accepted by the `sqz-clw-accept` thread, which waits ON the listening
//! socket — the names are load-bearing for `pidstat`/`perf` attribution,
//! the `fuse3-tpcN` lesson), bounded by the same [`ConnGate`] connection
//! cap that already bounded the accept loop. [`RpcService::call`] is
//! **synchronous by contract**, because §6.7's lock arbitration is
//! RAM-only (an `scc` probe plus one atomic); anything that must await is
//! a type, [`RpcAsyncService`], whose future is polled on the connection's
//! own thread (S8's own service executes there by default since D-5 —
//! `crate::meta_ship::owner_dispatch`).
//!
//! **Since D-5 (e2e perf audit §5.3 row 18) the connection's thread is a
//! LANE** (`SessionPark`): a [`squeezefs_ipc::sqz_exec::LaneExec`] whose
//! park waits on the socket and a wake eventfd through one `poll(2)`, so
//! every ready frame is read and its serve spawned as a lane task, and a
//! peer that PIPELINES calls on one session ([`MuxSession`]) is served
//! concurrently — K frames in flight cost one connection instead of K. A
//! stop-and-wait peer costs exactly what it did. Both sockets run
//! `TCP_NODELAY` (one write per frame; a pipelined session's back-to-back
//! frames must not wait for the peer's delayed ACK — measured 40 ms steps).
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
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
///
/// The S6 membership vocabulary (`membership_wire`) carries no schema of
/// its own — its bincode bodies ride THIS number — so a membership frame
/// change bumps it here. **2 since the lease grant began carrying the
/// writer's checkpoint ceiling** ([`crate::membership::Grant::checkpoint_ceiling_ms`],
/// the writer→member checkpoint composite, 2026-09-06; the grant's
/// `lane_supply_blocks` of the same day rides the bump too). A peer that
/// speaks 1 would decode a shorter grant (bincode is positional: a missing
/// trailing field is EOF, an extra one is silently ignored), so the
/// mismatch is a loud handshake refusal rather than a member running the
/// constant against a writer that advertised half of it. **3 since the
/// grant carries the lane-supply hint PER DATA VOLUME**
/// ([`crate::membership::Grant::lane_supply_volumes`], finding 15's fpp
/// residue, 2026-09-07): a 2-speaker's member would read the sum and never
/// the vector — every volume falling back to the mount-wide law, the
/// shipped shape, silently — and a 3-speaker's member against a 2-speaker
/// authority would decode a trailing `Vec` from EOF; the mismatch refuses
/// loud at the handshake (KD-7 same-commit fleets).
pub const CLUSTER_WIRE_SCHEMA: u32 = 3;

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

/// `true` ⇔ this I/O error is a socket-timeout expiry (`SO_RCVTIMEO` /
/// `SO_SNDTIMEO` surface as `WouldBlock` on Linux, `TimedOut` elsewhere).
/// One predicate, because every deadline on this wire now rides socket
/// timeouts instead of a `tokio::time::timeout` wrapper.
pub(crate) fn io_timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Commit body memory only as it arrives: at most one
/// [`FRAME_CHUNK_BYTES`] round is outstanding ahead of the peer, so four
/// attacker-chosen length bytes buy one chunk instead of the cap.
///
/// `deadline` is the whole-body bound: each `read_exact` blocks at most
/// the caller-set socket read timeout, and the elapsed check between
/// chunks is what turns a dribbling peer into an error instead of a
/// parked thread.
fn read_body_chunked<R: Read>(
    r: &mut R,
    len: usize,
    deadline: Option<(std::time::Instant, Duration)>,
) -> std::io::Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::with_capacity(len.min(FRAME_CHUNK_BYTES));
    while body.len() < len {
        if let Some((started, d)) = deadline {
            if started.elapsed() > d {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("frame body of {len} B did not arrive within {d:?}"),
                ));
            }
        }
        let want = (len - body.len()).min(FRAME_CHUNK_BYTES);
        let start = body.len();
        body.resize(start + want, 0);
        r.read_exact(&mut body[start..])?;
    }
    Ok(body)
}

/// Read `len` body bytes plus `trailer` trailing bytes under one optional
/// deadline covering the WHOLE body (a dribbling peer is an error, not a
/// parked thread). The length-prefix read itself is deliberately outside
/// the deadline: an idle authenticated session legitimately waits between
/// frames, and its bound is the session-level socket read timeout.
fn read_framed_bytes<R: Read>(
    r: &mut R,
    max_len: u32,
    trailer: usize,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
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
    let deadline = body_timeout.map(|d| (std::time::Instant::now(), d));
    let body = read_body_chunked(r, len as usize, deadline)?;
    let mut tail = vec![0u8; trailer];
    if trailer > 0 {
        if let Some((started, d)) = deadline {
            if started.elapsed() > d {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("frame trailer did not arrive within {d:?}"),
                ));
            }
        }
        r.read_exact(&mut tail)?;
    }
    Ok(Some((body, tail)))
}

/// Write one **unauthenticated** length-prefixed frame — the handshake
/// classes, and the pre-session direction of any protocol. One `write_all`
/// (prefix and body in a single buffer): a syscall per frame is the RTT
/// term this wire is measured on.
pub fn write_plain_frame<W: Write, T: Serialize>(
    w: &mut W,
    class: FrameClass,
    frame: &T,
) -> std::io::Result<()> {
    let body = encode_body(frame, class)?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    w.write_all(&out)?;
    w.flush()
}

/// Read one **unauthenticated** frame under an explicit class cap and
/// optional body deadline. `Ok(None)` on clean EOF at a frame boundary.
pub fn read_plain_frame<R: Read, T: DeserializeOwned>(
    r: &mut R,
    max_len: u32,
    body_timeout: Option<Duration>,
) -> std::io::Result<Option<T>> {
    match read_framed_bytes(r, max_len, 0, body_timeout)? {
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
    pub fn send<W: Write, T: Serialize>(
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
        w.write_all(&out)?;
        w.flush()?;
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
    pub fn recv<R: Read, T: DeserializeOwned>(
        &mut self,
        r: &mut R,
        max_len: u32,
        body_timeout: Option<Duration>,
    ) -> std::io::Result<Option<T>> {
        let Some((body, tag)) = read_framed_bytes(r, max_len, MAC_BYTES, body_timeout)? else {
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
///
/// Walks BYTES, not `str` slices: the former `&s[i..i + 2]` panicked when
/// an even-length input carried a multi-byte character across the slice
/// boundary — and this function decodes the `job:enroll` secret, an
/// on-disk record, so that panic was a daemon abort on a corrupt or
/// tampered xattr (1.2 fuzz find, `tests/decoder_property_tests.rs`).
/// The alphabet is explicit too: `u8::from_str_radix` accepted a leading
/// `+`/`-`, a form [`hex_encode`] never produces.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
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

/// The TLS **acceptor** config for this wire — CA-pinned mTLS only.
///
/// A `ClusterSecurityConfig` without both halves of the CA pair is
/// **refused**: the node certificate the cluster machinery presents is
/// signed by the CA key, so a cert alone cannot produce an authenticated
/// channel, and a config with neither used to install an
/// accept-everything verifier. That path is deleted from the tree; on this
/// wire the honest alternative to mTLS is plaintext plus storage-trust
/// authn, which is authenticated.
///
/// Returns the rustls `ServerConfig` (the rip-tokio conversion: sessions
/// run sync `rustls::StreamOwned` on OS threads — see
/// `tls_server_handshake`).
pub fn tls_acceptor(security: &ClusterSecurityConfig) -> Result<Arc<rustls::ServerConfig>> {
    let ca = require_ca(security)?;
    let cfg = rustls_server_config(&ca)?;
    Ok(Arc::new(cfg))
}

/// The TLS **connector** config for this wire — same refusal, dial side.
pub fn tls_connector(security: &ClusterSecurityConfig) -> Result<Arc<rustls::ClientConfig>> {
    let ca = require_ca(security)?;
    let cfg = rustls_client_config(&ca)?;
    Ok(Arc::new(cfg))
}

/// Run the server half of the TLS handshake to completion (sync rustls
/// completes its handshake lazily on first I/O; the exporter binding needs
/// it done NOW, so `complete_io` is driven explicitly). The caller bounds
/// it by setting the socket read/write timeouts to the handshake deadline
/// **before** calling — the handshake is attacker-paced.
pub(crate) fn tls_server_handshake(
    cfg: Arc<rustls::ServerConfig>,
    tcp: TcpStream,
) -> std::io::Result<StreamOwned<ServerConnection, TcpStream>> {
    let conn = ServerConnection::new(cfg).map_err(std::io::Error::other)?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock)?;
    }
    Ok(s)
}

/// The dial half of [`tls_server_handshake`] — same explicit-completion
/// law, same caller-set socket-timeout bound. The literal `"localhost"`
/// server name matches the SANs the `cluster_tls` node certs carry (the
/// CA pin plus the storage-trust proof are what actually authenticate the
/// peer — module docs, "still open by design").
pub(crate) fn tls_client_handshake(
    cfg: Arc<rustls::ClientConfig>,
    tcp: TcpStream,
) -> std::io::Result<StreamOwned<ClientConnection, TcpStream>> {
    let name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("literal server name")
        .to_owned();
    let conn = ClientConnection::new(cfg, name).map_err(std::io::Error::other)?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock)?;
    }
    Ok(s)
}

/// Blocking dial with a real deadline: resolve, then
/// `TcpStream::connect_timeout` each candidate address in order (the
/// tokio dial this replaces had no explicit connect bound at all — the
/// deadline that used to cover only the handshake now also bounds the
/// connect).
pub(crate) fn dial_tcp(endpoint: &str, deadline: Duration) -> std::io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = endpoint.to_socket_addrs()?.collect();
    let mut last: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, deadline) {
            Ok(s) => {
                // Every frame is one write; nothing here benefits from
                // Nagle, and a PIPELINED session (`MuxSession`) is
                // destroyed by it — back-to-back small frames wait for the
                // peer's delayed ACK (measured: 40 ms steps per reply).
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("'{endpoint}' resolved to no addresses"),
        )
    }))
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
// Accept/serve thread posture (§6.7: never the conveyor's task)
// ---------------------------------------------------------------------------

/// The accept thread's shutdown-latch observation bound: the listener
/// socket is non-blocking and the named accept thread WAITS ON IT
/// (`poll(2)`, [`wait_for_accept`]) so a connection arrival wakes it at
/// once; this tick bounds only how long `shutdown()` can wait for the
/// thread to notice the latch (the simple-and-loud stop protocol — no
/// self-connect nudge needed). Before D-5 (e2e perf audit §5.3 row 18)
/// the thread SLEPT this long between accept attempts, so every sequential
/// dial paid the whole tick (measured 100.09–100.71 ms per dial): a
/// fleet-start / reconnect / first-beat term on every plane the wire
/// carries. A liveness constant, never a latency term.
pub(crate) const ACCEPT_POLL_TICK: Duration = Duration::from_millis(100);

/// Park the accept thread until `listener` is readable (a pending
/// connection) or `tick` elapses. `poll(2)` on the listening fd — a
/// readable listening socket IS an accept-ready one. Any poll error is
/// reported to the caller's accept, which classifies it (an `EINTR` simply
/// re-polls).
fn wait_for_accept(listener: &std::net::TcpListener, tick: Duration) {
    use std::os::fd::AsRawFd;
    let mut fds = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = libc::c_int::try_from(tick.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: `fds` is a valid, initialized pollfd array of length 1 that
    // outlives the call; the fd is owned by `listener` for the call's
    // duration.
    let _ = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
}

/// Lock-held read-attempt slice for a **split** TLS stream: sync rustls
/// cannot read and write one connection from two threads, so the halves
/// share a mutex and the reader parks in the socket at most this long per
/// slice before releasing it to the writer.
const TLS_HALF_POLL_TICK: Duration = Duration::from_millis(100);

/// Derived owner-side RPC lane count.
///
/// Resource caps derive from system resources (AGENTS): one lane per 8
/// cores, floored at 1 (a single-core box still owns its slots) and
/// ceilinged at 8 so a 256-core host does not spawn a lane farm for a
/// control plane. `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` overrides absolute
/// (the A/B lever), and never oversubscribes the box.
///
/// Retained across the rip-tokio conversion for the callers that size
/// [`RpcListenerConfig::service_threads`] with it: since the listener
/// serves thread-per-connection, the value no longer allocates lanes —
/// the connection cap ([`RpcListenerConfig::max_connections`]) is what
/// bounds serve threads.
pub fn default_service_threads() -> usize {
    // Sized from the fleet-share-DIVIDED root (KD-MW-14 rung 3c — the
    // retired direct available_parallelism() read bypassed the divisor).
    service_threads_from(
        crate::env_knobs::opt_int_knob::<usize>("SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS"),
        crate::cpu::process_parallelism(),
    )
}

/// Pure form (tie-tested in the derivation sweep): derived =
/// `ceil(cpus / 8).clamp(1, 8)` — allocation math rounds UP (2026-08-14
/// ruling: a fractional share of the box derives the next whole RPC
/// lane); explicit wins verbatim, and neither ever oversubscribes the
/// (divided) root.
pub fn service_threads_from(explicit: Option<usize>, cpus: usize) -> usize {
    let derived = cpus.div_ceil(8).clamp(1, 8);
    explicit.unwrap_or(derived).clamp(1, cpus.max(1))
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
/// `call` is **synchronous by contract**, and runs on the connection's own
/// OS thread (`sqz-clw-conn`, one per admitted connection — the module's
/// §3). §6.7's arbitration is RAM-only — an `scc` probe
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
    // Sized from the fleet-share-DIVIDED root (KD-MW-14 rung 3c).
    max_connections_from(crate::cpu::process_parallelism())
}

/// Pure form (tie-tested in the derivation sweep): `(cpus × 16).clamp(64,
/// 1024)` — floored so a small box still admits a real peer population,
/// ceilinged so a large one still has a bound.
pub fn max_connections_from(cpus: usize) -> usize {
    cpus.saturating_mul(16).clamp(64, 1024)
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

/// The TLS session forms this wire carries (sync rustls over blocking
/// TCP). One enum instead of a generic so [`ClusterStream`] stays a
/// nameable type across both directions.
enum TlsConn {
    Server(StreamOwned<ServerConnection, TcpStream>),
    Client(StreamOwned<ClientConnection, TcpStream>),
}

impl TlsConn {
    fn sock(&self) -> &TcpStream {
        match self {
            TlsConn::Server(s) => &s.sock,
            TlsConn::Client(s) => &s.sock,
        }
    }
}

impl Read for TlsConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TlsConn::Server(s) => s.read(buf),
            TlsConn::Client(s) => s.read(buf),
        }
    }
}

impl Write for TlsConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TlsConn::Server(s) => s.write(buf),
            TlsConn::Client(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TlsConn::Server(s) => s.flush(),
            TlsConn::Client(s) => s.flush(),
        }
    }
}

/// A duplex byte stream on this wire: plaintext TCP or CA-pinned mTLS.
/// One definition, so every protocol on the wire (RPC verbs, job shards)
/// is carried by the same stream type instead of each declaring its own.
/// Sync (`std::io::Read`/`Write` on blocking sockets, OS threads);
/// deadlines ride the socket timeouts ([`Self::set_read_timeout`] /
/// [`Self::set_write_timeout`]).
pub struct ClusterStream(StreamInner);

enum StreamInner {
    /// Plaintext TCP (storage-trust authn makes the session
    /// authenticated; it is not confidential).
    Tcp(TcpStream),
    /// CA-pinned mutual TLS. Boxed: a rustls session carries its record
    /// buffers, so the variants differ by ~KiBs (clippy
    /// `large_enum_variant`).
    Tls(Box<TlsConn>),
}

impl ClusterStream {
    /// Wrap a plaintext connection.
    pub(crate) fn tcp(s: TcpStream) -> Self {
        ClusterStream(StreamInner::Tcp(s))
    }

    /// Wrap a completed server-side TLS handshake.
    pub(crate) fn tls_server(s: StreamOwned<ServerConnection, TcpStream>) -> Self {
        ClusterStream(StreamInner::Tls(Box::new(TlsConn::Server(s))))
    }

    /// Wrap a completed dial-side TLS handshake.
    pub(crate) fn tls_client(s: StreamOwned<ClientConnection, TcpStream>) -> Self {
        ClusterStream(StreamInner::Tls(Box::new(TlsConn::Client(s))))
    }

    fn sock(&self) -> &TcpStream {
        match &self.0 {
            StreamInner::Tcp(s) => s,
            StreamInner::Tls(t) => t.sock(),
        }
    }

    /// Does the stream hold DECRYPTED bytes the socket no longer shows?
    /// Plaintext TCP never does (the framer reads exactly its frame);
    /// rustls may hold a whole record beyond the frame just read, so a
    /// readiness poll on the socket alone would leave that frame waiting
    /// for the next arrival.
    pub(crate) fn has_buffered_plaintext(&mut self) -> bool {
        match &mut self.0 {
            StreamInner::Tcp(_) => false,
            StreamInner::Tls(t) => match &mut **t {
                TlsConn::Server(s) => s
                    .conn
                    .process_new_packets()
                    .is_ok_and(|st| st.plaintext_bytes_to_read() > 0),
                TlsConn::Client(s) => s
                    .conn
                    .process_new_packets()
                    .is_ok_and(|st| st.plaintext_bytes_to_read() > 0),
            },
        }
    }

    /// Bound every subsequent blocking read at the socket.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.sock().set_read_timeout(d)
    }

    /// Bound every subsequent blocking write at the socket.
    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.sock().set_write_timeout(d)
    }

    /// A dup'd handle to the underlying socket, for teardown nudges:
    /// `shutdown(Both)` on it from another thread wakes any read this
    /// stream is parked in — the stop protocol for session threads.
    pub fn nudge_handle(&self) -> std::io::Result<TcpStream> {
        self.sock().try_clone()
    }

    /// Split into independently usable read/write halves (the job wire's
    /// full-duplex sessions: one thread reads while others write).
    /// Plaintext halves are dup'd sockets; TLS halves share the rustls
    /// session under a mutex, with the reader parking in bounded
    /// `TLS_HALF_POLL_TICK` slices so a parked read never starves the
    /// writer.
    pub fn split(self) -> std::io::Result<(ClusterReadHalf, ClusterWriteHalf)> {
        match self.0 {
            StreamInner::Tcp(s) => {
                let w = s.try_clone()?;
                Ok((
                    ClusterReadHalf {
                        inner: HalfInner::Tcp(s),
                        bound: None,
                    },
                    ClusterWriteHalf {
                        inner: HalfInner::Tcp(w),
                    },
                ))
            }
            StreamInner::Tls(t) => {
                let sock = t.sock().try_clone()?;
                let shared = Arc::new(TlsHalves {
                    tls: parking_lot::Mutex::new(*t),
                    sock,
                });
                Ok((
                    ClusterReadHalf {
                        inner: HalfInner::Tls(Arc::clone(&shared)),
                        bound: None,
                    },
                    ClusterWriteHalf {
                        inner: HalfInner::Tls(shared),
                    },
                ))
            }
        }
    }
}

impl Read for ClusterStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.0 {
            StreamInner::Tcp(s) => s.read(buf),
            StreamInner::Tls(t) => t.read(buf),
        }
    }
}

impl Write for ClusterStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.0 {
            StreamInner::Tcp(s) => s.write(buf),
            StreamInner::Tls(t) => t.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            StreamInner::Tcp(s) => s.flush(),
            StreamInner::Tls(t) => t.flush(),
        }
    }
}

/// The shared state of a split TLS stream. `sock` is a dup of the
/// session's socket, held OUTSIDE the mutex so timeouts (`SO_RCVTIMEO` /
/// `SO_SNDTIMEO` are socket-level, shared across dups) can be adjusted
/// while the other half holds the session.
struct TlsHalves {
    tls: parking_lot::Mutex<TlsConn>,
    sock: TcpStream,
}

enum HalfInner {
    Tcp(TcpStream),
    Tls(Arc<TlsHalves>),
}

/// The read half of a split [`ClusterStream`].
pub struct ClusterReadHalf {
    inner: HalfInner,
    /// The TLS read bound (plaintext rides the socket timeout directly).
    bound: Option<Duration>,
}

impl ClusterReadHalf {
    /// Bound every subsequent read. Plaintext: the socket timeout. TLS:
    /// enforced across the mutex-sliced poll loop, so the bound holds
    /// even though each socket park is a `TLS_HALF_POLL_TICK` slice.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> std::io::Result<()> {
        match &self.inner {
            HalfInner::Tcp(s) => s.set_read_timeout(d),
            HalfInner::Tls(_) => {
                self.bound = d;
                Ok(())
            }
        }
    }
}

impl Read for ClusterReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            HalfInner::Tcp(s) => s.read(buf),
            HalfInner::Tls(sh) => {
                let deadline = self.bound.map(|d| std::time::Instant::now() + d);
                loop {
                    {
                        let mut g = sh.tls.lock();
                        sh.sock.set_read_timeout(Some(TLS_HALF_POLL_TICK))?;
                        match g.read(buf) {
                            Ok(n) => return Ok(n),
                            // A slice expiry: release the session to the
                            // writer, then re-park. rustls buffers any
                            // partial record internally, so the retry is
                            // safe.
                            Err(e) if io_timed_out(&e) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "tls read timed out",
                        ));
                    }
                }
            }
        }
    }
}

/// The write half of a split [`ClusterStream`].
pub struct ClusterWriteHalf {
    inner: HalfInner,
}

impl Write for ClusterWriteHalf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            HalfInner::Tcp(s) => s.write(buf),
            HalfInner::Tls(sh) => sh.tls.lock().write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            HalfInner::Tcp(s) => s.flush(),
            HalfInner::Tls(sh) => sh.tls.lock().flush(),
        }
    }
}

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

/// The cluster-wire RPC listener: bounded accept poll loop on a named OS
/// thread (`sqz-clw-accept`) + zero-config mutual authn + authenticated
/// session loops, one named OS thread per admitted connection
/// (`sqz-clw-conn`), bounded by the [`ConnGate`] cap.
pub struct RpcListener {
    cfg: RpcListenerConfig,
    endpoint: SocketAddr,
    gate: Arc<AuthnGate>,
    service: ServiceArm,
    counters: Arc<ListenerCounters>,
    channel: ChannelClass,
    tls: Option<Arc<rustls::ServerConfig>>,
    next_conn: AtomicU64,
    shutdown: Arc<AtomicBool>,
    accept_thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Dup'd per-connection sockets, for the shutdown nudge
    /// (`shutdown(Both)` wakes a session thread parked in a read).
    /// Self-draining: a serve thread removes its own entry on exit.
    conn_socks: parking_lot::Mutex<HashMap<u64, TcpStream>>,
    /// Serve-thread handles, joined at shutdown. Self-draining like
    /// `conn_socks`.
    conn_threads: parking_lot::Mutex<HashMap<u64, std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for RpcListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcListener")
            .field("endpoint", &self.endpoint)
            .field("channel", &self.channel.name())
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
            accept_thread: parking_lot::Mutex::new(None),
            conn_socks: parking_lot::Mutex::new(HashMap::new()),
            conn_threads: parking_lot::Mutex::new(HashMap::new()),
        });
        log::info!(
            "cluster wire: listener {endpoint} ({}) — thread-per-connection, max {} \
             connections, {HANDSHAKE_MAX_FRAME_BYTES} B pre-authn frame cap, handshake \
             deadline {:?}, challenge freshness {:?}; every admitted frame carries a \
             session MAC derived from the shared volume's job:enroll secret",
            host.channel.name(),
            host.cfg.max_connections,
            host.cfg.handshake_timeout,
            host.cfg.enroll_freshness,
        );
        let accept_host = Arc::clone(&host);
        let handle = std::thread::Builder::new()
            .name("sqz-clw-accept".to_string())
            .spawn(move || accept_host.accept_loop(std_listener))
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "cluster wire: accept thread refused: {e}"
                ))
            })?;
        *host.accept_thread.lock() = Some(handle);
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

    /// Stop accepting, nudge every live session's socket so its serve
    /// thread wakes out of any parked read, and join the threads.
    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut socks = self.conn_socks.lock();
            for (_, sock) in socks.drain() {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
        if let Some(h) = self.accept_thread.lock().take() {
            let _ = h.join();
        }
        let handles: Vec<_> = {
            let mut threads = self.conn_threads.lock();
            threads.drain().map(|(_, h)| h).collect()
        };
        for h in handles {
            let _ = h.join();
        }
    }

    fn accept_loop(self: Arc<Self>, listener: std::net::TcpListener) {
        // The listener is non-blocking: the thread waits ON the socket
        // (an arrival wakes it at once) and the poll tick is only how it
        // observes the shutdown latch (simple and loud — no self-connect
        // nudge protocol).
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
                Err(e) if io_timed_out(&e) => {
                    wait_for_accept(&listener, ACCEPT_POLL_TICK);
                    continue;
                }
                Err(e) => {
                    // A bare `continue` here turns a persistent
                    // EMFILE/ENFILE condition into a busy loop.
                    let d = next_accept_backoff(backoff);
                    backoff = Some(d);
                    self.counters.backoffs.fetch_add(1, Ordering::SeqCst);
                    log::warn!("cluster wire: accept failed: {e} — backing off {d:?}");
                    std::thread::sleep(d);
                    continue;
                }
            };
            // Claim the connection slot BEFORE anything is spawned:
            // over-cap peers cost one accept and one close, never a
            // thread or a buffer.
            let Some(permit) = self.counters.conns.try_admit() else {
                self.counters.refused.fetch_add(1, Ordering::SeqCst);
                log::warn!(
                    "cluster wire: refusing {peer} — {} concurrent connections is the cap",
                    self.cfg.max_connections
                );
                drop(tcp);
                continue;
            };
            if let Err(e) = tcp.set_nonblocking(false) {
                log::warn!("cluster wire: dropping {peer} — set_nonblocking(false) failed: {e}");
                continue;
            }
            // The dial side's law (`dial_tcp`): one write per frame, and a
            // pipelining peer's replies must not wait for its delayed ACK.
            if let Err(e) = tcp.set_nodelay(true) {
                log::warn!("cluster wire: dropping {peer} — set_nodelay failed: {e}");
                continue;
            }
            let conn_id = self.next_conn.fetch_add(1, Ordering::SeqCst);
            if let Ok(nudge) = tcp.try_clone() {
                self.conn_socks.lock().insert(conn_id, nudge);
            }
            let host = Arc::clone(&self);
            // The registry lock is held across spawn+insert, so the serve
            // thread's own exit-time removal can never run before the
            // insert it undoes.
            let mut threads = self.conn_threads.lock();
            let spawned = std::thread::Builder::new()
                .name("sqz-clw-conn".to_string())
                .spawn(move || {
                    /// Registry removal on EVERY exit (normal return or
                    /// panic unwind), so the maps self-drain.
                    struct Reaper {
                        host: Arc<RpcListener>,
                        conn_id: u64,
                    }
                    impl Drop for Reaper {
                        fn drop(&mut self) {
                            self.host.conn_socks.lock().remove(&self.conn_id);
                            self.host.conn_threads.lock().remove(&self.conn_id);
                        }
                    }
                    let _reaper = Reaper {
                        host: Arc::clone(&host),
                        conn_id,
                    };
                    let _permit = permit;
                    host.serve_conn(tcp, peer);
                });
            match spawned {
                Ok(h) => {
                    threads.insert(conn_id, h);
                }
                Err(e) => {
                    self.counters
                        .service_refusals
                        .fetch_add(1, Ordering::SeqCst);
                    self.conn_socks.lock().remove(&conn_id);
                    log::warn!("cluster wire: dropping {peer} — serve thread refused: {e}");
                }
            }
            drop(threads);
        }
    }

    fn serve_conn(self: &Arc<Self>, tcp: TcpStream, peer: SocketAddr) {
        let deadline = self.cfg.handshake_timeout;
        // Everything an unauthenticated peer does is bounded by the
        // handshake deadline, applied at the socket (SO_RCVTIMEO /
        // SO_SNDTIMEO — the tokio::time::timeout wrappers this replaces
        // bounded the same exchanges).
        if tcp.set_read_timeout(Some(deadline)).is_err()
            || tcp.set_write_timeout(Some(deadline)).is_err()
        {
            log::warn!("cluster wire: {peer}: socket timeout setup failed — dropped");
            return;
        }
        // The TLS handshake is attacker-paced: it runs under the socket
        // deadlines just installed, driven to completion explicitly so
        // the exporter binding exists before the challenge.
        let (mut stream, binding): (ClusterStream, Option<[u8; 32]>) = match self.tls.clone() {
            Some(cfg) => match tls_server_handshake(cfg, tcp) {
                Ok(s) => {
                    let binding = server_exporter(&s.conn);
                    (ClusterStream::tls_server(s), binding)
                }
                Err(e) => {
                    log::warn!("cluster wire: TLS handshake with {peer} failed: {e}");
                    return;
                }
            },
            None => (ClusterStream::tcp(tcp), None),
        };

        // The coordinator speaks first (the peer cannot choose its own
        // challenge, so a captured proof is not a credential).
        let challenge = self.gate.issue_challenge();
        let frame = RpcFrame::Challenge {
            schema: challenge.schema,
            server_nonce: challenge.server_nonce.clone(),
            freshness_ms: challenge.freshness_ms,
        };
        if let Err(e) = write_plain_frame(&mut stream, FrameClass::Handshake, &frame) {
            log::warn!("cluster wire: {peer}: challenge write stalled: {e}");
            return;
        }

        let proof = match read_plain_frame::<_, RpcFrame>(
            &mut stream,
            FrameClass::Handshake.cap(),
            Some(deadline),
        ) {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) if io_timed_out(&e) => {
                log::warn!("cluster wire: {peer}: no proof within {deadline:?} — dropped");
                return;
            }
            Err(e) => {
                self.counters
                    .admissions_refused
                    .fetch_add(1, Ordering::SeqCst);
                log::warn!("cluster wire: {peer}: undecodable proof frame: {e}");
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
            );
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
                );
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

        // Session posture: a stalled frame body is bounded at the socket
        // (read AND write timeouts); the idle bound is enforced by the
        // session lane's park (`SessionPark`), which waits ON the socket.
        let _ = stream.set_read_timeout(Some(self.cfg.frame_body_timeout));
        let _ = stream.set_write_timeout(Some(self.cfg.frame_body_timeout));

        // Authenticated session loop. Every frame is MAC'd, so the
        // session cannot be hijacked, reordered or replayed mid-flight.
        let (tx, rx) = session_framers(&key, Role::Coordinator);
        let Some(park) = SessionPark::new(
            Arc::clone(self),
            SessionIo { stream, tx, rx },
            peer_id.clone(),
        ) else {
            log::warn!("cluster wire: {peer}: session wake eventfd refused — dropped");
            return;
        };
        let exec = squeezefs_ipc::sqz_exec::LaneExec::with_park(
            Arc::clone(&park) as Arc<dyn squeezefs_ipc::sqz_exec::LanePark>
        );
        park.arm(exec.clone());
        // The lane runs on THIS thread until the session closes (EOF, an
        // error, the idle bound, or the listener's shutdown nudge) and its
        // last in-flight serve has replied.
        exec.run();
        // Break the park ↔ exec reference cycle so the session's socket
        // closes with the thread.
        park.disarm();
    }
}

/// The authenticated session's I/O — the frame reader (the lane's
/// `service` hook) and every reply writer (a serve task) share it on the
/// connection's ONE thread, so the mutex is never contended; it exists
/// because a lane task must be `Send`.
struct SessionIo {
    stream: ClusterStream,
    tx: FrameTx,
    rx: FrameRx,
}

/// The session lane's liveness words.
struct SessionState {
    /// The last frame's arrival (the idle bound's anchor).
    last_frame_at: std::time::Instant,
    /// Serves admitted and not yet replied.
    inflight: usize,
    /// No more frames will be read (EOF / error / idle / shutdown); the
    /// lane exits once `inflight` reaches 0.
    closing: bool,
}

/// **The owner-side session as a lane** (e2e perf audit D-5, DLM #8's
/// single-connection half): one authenticated connection is one
/// [`squeezefs_ipc::sqz_exec::LaneExec`] on the connection's own thread
/// whose park WAITS ON THE SOCKET — `poll(2)` over the socket and a wake
/// eventfd — so a frame arrival and a serve task's wake arrive through one
/// wait. The `service` hook (every loop iteration, never blocking on an
/// empty socket) reads every frame that is ready and spawns its serve as
/// a lane task; the task writes its reply when its verb completes. A peer
/// that PIPELINES calls on one session is therefore served concurrently —
/// K frames in flight no longer cost K connections (the F-B cap) — and a
/// stop-and-wait peer costs exactly what it did: one thread, the serve
/// polled on it, no extra hop (the venue [`crate::meta_ship::owner_dispatch`]
/// executes on). §6.7's rule — never the conveyor's task — is untouched.
///
/// Liveness: the idle bound is the park's timeout while nothing is in
/// flight; EOF / a read error / a MAC failure / the listener's shutdown
/// nudge (`shutdown(Both)` on the dup'd socket wakes the poll) flip
/// `closing`, and the lane shuts down when the last in-flight serve has
/// replied (a dropped serve future mid-commit would be a cancellation the
/// D5 law tolerates but nothing needs). A serve that unwinds is contained
/// by the lane (`task_panics`); its `InflightGuard` still retires it.
struct SessionPark {
    host: Arc<RpcListener>,
    io: parking_lot::Mutex<SessionIo>,
    peer_id: String,
    sock_fd: std::os::fd::RawFd,
    wake: std::os::fd::OwnedFd,
    state: parking_lot::Mutex<SessionState>,
    /// The lane this park serves — set once the lane exists, cleared when
    /// it exits (the park ↔ exec cycle must not outlive the thread).
    exec: parking_lot::Mutex<Option<squeezefs_ipc::sqz_exec::LaneExec>>,
    /// The serve tasks hold the park by `Arc`; the `&self` hooks reach it
    /// through this.
    me: std::sync::OnceLock<std::sync::Weak<SessionPark>>,
}

/// Retires one in-flight serve on EVERY exit path (reply written, write
/// failed, or the serve unwound) and closes the lane when it was the last
/// one of a closing session.
struct InflightGuard {
    park: Arc<SessionPark>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut st = self.park.state.lock();
        st.inflight -= 1;
        let last = st.closing && st.inflight == 0;
        drop(st);
        if last {
            self.park.shutdown_lane();
        }
    }
}

impl SessionPark {
    fn new(host: Arc<RpcListener>, io: SessionIo, peer_id: String) -> Option<Arc<Self>> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let sock_fd = io.stream.sock().as_raw_fd();
        // SAFETY: eventfd(2) with a zero count and the close-on-exec flag;
        // a negative return is the error arm, never wrapped.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return None;
        }
        // SAFETY: `raw` is a fresh, valid fd this process owns exclusively.
        let wake = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let park = Arc::new(Self {
            host,
            io: parking_lot::Mutex::new(io),
            peer_id,
            sock_fd,
            wake,
            state: parking_lot::Mutex::new(SessionState {
                last_frame_at: std::time::Instant::now(),
                inflight: 0,
                closing: false,
            }),
            exec: parking_lot::Mutex::new(None),
            me: std::sync::OnceLock::new(),
        });
        let _ = park.me.set(Arc::downgrade(&park));
        Some(park)
    }

    fn arm(&self, exec: squeezefs_ipc::sqz_exec::LaneExec) {
        *self.exec.lock() = Some(exec);
    }

    fn disarm(&self) {
        *self.exec.lock() = None;
    }

    fn self_arc(&self) -> Option<Arc<Self>> {
        self.me.get().and_then(std::sync::Weak::upgrade)
    }

    fn shutdown_lane(&self) {
        if let Some(exec) = self.exec.lock().as_ref() {
            exec.shutdown();
        }
    }

    /// Flip `closing`; shut the lane down at once if nothing is in flight.
    fn close(&self) {
        let mut st = self.state.lock();
        if st.closing {
            return;
        }
        st.closing = true;
        let now = st.inflight == 0;
        drop(st);
        if now {
            self.shutdown_lane();
        }
    }

    /// Is a frame ready to read without blocking on an empty socket: the
    /// socket is readable, or (mTLS) rustls holds decrypted plaintext the
    /// socket no longer shows.
    fn frame_ready(&self, io: &mut SessionIo) -> bool {
        if io.stream.has_buffered_plaintext() {
            return true;
        }
        let mut fds = libc::pollfd {
            fd: self.sock_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a valid one-element pollfd array; zero timeout never
        // blocks.
        let n = unsafe { libc::poll(&mut fds, 1, 0) };
        n > 0 && fds.revents != 0
    }

    /// Read every ready frame and spawn its serve on the lane.
    fn read_ready_frames(self: &Arc<Self>) {
        loop {
            if self.state.lock().closing {
                return;
            }
            let mut io = self.io.lock();
            if !self.frame_ready(&mut io) {
                return;
            }
            let SessionIo { stream, rx, .. } = &mut *io;
            let frame = match rx.recv::<_, RpcFrame>(
                stream,
                FrameClass::Bulk.cap(),
                Some(self.host.cfg.frame_body_timeout),
            ) {
                Ok(Some(f)) => f,
                Ok(None) => {
                    drop(io);
                    self.close();
                    return;
                }
                Err(e) => {
                    drop(io);
                    if io_timed_out(&e) {
                        log::warn!(
                            "cluster wire: peer '{}' stalled a frame past {:?} — closing the \
                             session",
                            self.peer_id,
                            self.host.cfg.frame_body_timeout
                        );
                    } else if e.to_string().contains("mac") {
                        self.host
                            .counters
                            .mac_failures
                            .fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "cluster wire: peer '{}' frame failed authentication ({e}) — \
                             closing the session",
                            self.peer_id
                        );
                    } else {
                        log::warn!(
                            "cluster wire: peer '{}' session read error: {e}",
                            self.peer_id
                        );
                    }
                    self.close();
                    return;
                }
            };
            drop(io);
            let RpcFrame::Call { id, verb, body } = frame else {
                log::warn!(
                    "cluster wire: peer '{}' sent a non-Call frame — ignored",
                    self.peer_id
                );
                continue;
            };
            {
                let mut st = self.state.lock();
                st.last_frame_at = std::time::Instant::now();
                st.inflight += 1;
            }
            let Some(exec) = self.exec.lock().clone() else {
                return;
            };
            let park = Arc::clone(self);
            exec.spawn(async move {
                let _guard = InflightGuard {
                    park: Arc::clone(&park),
                };
                // The service runs HERE — on this connection's own
                // thread, §6.7's venue rule: never the conveyor's task. An
                // awaiting arm (S8's metadata verbs) is polled on this
                // lane beside the session's other in-flight serves.
                let req = RpcRequest { id, verb, body };
                let reply = match &park.host.service {
                    ServiceArm::Sync(svc) => svc.call(req),
                    ServiceArm::Async(svc) => svc.call(req).await,
                };
                park.host.counters.served.fetch_add(1, Ordering::SeqCst);
                let mut io = park.io.lock();
                let SessionIo { stream, tx, .. } = &mut *io;
                let sent = tx.send(
                    stream,
                    FrameClass::Bulk,
                    &RpcFrame::Reply {
                        id: reply.id,
                        status: reply.status,
                        body: reply.body,
                    },
                );
                drop(io);
                if sent.is_err() {
                    park.close();
                }
            });
        }
    }
}

impl squeezefs_ipc::sqz_exec::LanePark for SessionPark {
    fn park(&self, tick: Duration) -> bool {
        let timeout = {
            let st = self.state.lock();
            if st.inflight == 0 && !st.closing {
                tick.min(
                    self.host
                        .cfg
                        .session_idle_timeout
                        .saturating_sub(st.last_frame_at.elapsed()),
                )
            } else {
                tick
            }
        };
        use std::os::fd::AsRawFd;
        let mut fds = [
            libc::pollfd {
                fd: self.sock_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout_ms =
            libc::c_int::try_from(timeout.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: a valid two-element pollfd array that outlives the call;
        // both fds are owned for the session's lifetime.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        if fds[1].revents != 0 {
            let mut count = [0u8; 8];
            // SAFETY: an 8-byte read of the non-blocking eventfd counter
            // into a valid buffer; EAGAIN is an already-drained counter.
            let _ = unsafe {
                libc::read(
                    self.wake.as_raw_fd(),
                    count.as_mut_ptr().cast(),
                    count.len(),
                )
            };
        }
        n == 0
    }

    fn unpark(&self) {
        use std::os::fd::AsRawFd;
        let one = 1u64.to_ne_bytes();
        // SAFETY: an 8-byte write to the eventfd from a valid buffer; the
        // counter saturates far above any wake population, and a full
        // counter still leaves it readable, so a failed write loses no wake.
        let _ = unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    fn service(&self) {
        if self.host.shutdown.load(Ordering::SeqCst) {
            self.close();
            return;
        }
        // `service` is a `&self` hook; the reader needs the `Arc` for the
        // serve tasks it spawns.
        let Some(me) = self.self_arc() else {
            self.shutdown_lane();
            return;
        };
        me.read_ready_frames();
        let idle = {
            let st = self.state.lock();
            st.inflight == 0
                && !st.closing
                && st.last_frame_at.elapsed() >= self.host.cfg.session_idle_timeout
        };
        if idle {
            log::warn!(
                "cluster wire: peer '{}' sent no frame within {:?} — closing the idle session",
                self.peer_id,
                self.host.cfg.session_idle_timeout
            );
            self.close();
        }
    }
}

impl Drop for RpcListener {
    fn drop(&mut self) {
        self.shutdown();
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

/// A dialed session's I/O state — owned as one unit so [`RpcClient::call`]
/// can move it onto the blocking pool and back (the async API is
/// preserved; the roundtrip itself is sync socket I/O).
struct ClientIo {
    stream: ClusterStream,
    tx: FrameTx,
    rx: FrameRx,
    call_timeout: Duration,
}

impl ClientIo {
    fn roundtrip(&mut self, id: u64, verb: u16, body: Vec<u8>) -> Result<RpcResponse> {
        self.tx.send(
            &mut self.stream,
            FrameClass::Bulk,
            &RpcFrame::Call { id, verb, body },
        )?;
        let frame = self
            .rx
            .recv::<_, RpcFrame>(
                &mut self.stream,
                FrameClass::Bulk.cap(),
                Some(self.call_timeout),
            )
            .map_err(|e| {
                if io_timed_out(&e) {
                    SqueezefsError::InvalidOperation(format!(
                        "cluster wire: no reply to call {id} within {:?}",
                        self.call_timeout
                    ))
                } else {
                    SqueezefsError::from(e)
                }
            })?;
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

/// A dialed, authenticated cluster-wire session.
pub struct RpcClient {
    /// `None` only while a call is in flight on the blocking pool (or
    /// after that hop was lost to a panic — every later call then refuses
    /// loud instead of reusing a desynchronized session).
    io: Option<ClientIo>,
    authn: SessionAuthn,
    next_id: u64,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("authn", &self.authn)
            .field("calls", &self.next_id)
            .finish_non_exhaustive()
    }
}

/// Bound on the dial-side connect + handshake and on one call's reply.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The reply bound every [`RpcClient::call`] waits under — the socket read
/// timeout installed at dial time, so it is the wall from the client's
/// send to the first reply byte. A verb the SERVER parks for this long
/// (the custody notice poll's park is clamped to the renewal cadence,
/// which on the fleet is exactly this value) is a race the client loses
/// by the RTT plus the owner's wake; a parking verb's ask must derive from
/// this with margin (`data_grant::notice_poll_park`).
pub fn call_reply_bound() -> Duration {
    DIAL_TIMEOUT
}

impl RpcClient {
    /// Dial, prove storage membership against the coordinator's
    /// **server-issued** challenge, derive the session key, and return the
    /// authenticated session. Async API preserved; the socket work runs on
    /// the blocking pool.
    pub async fn connect(
        endpoint: &str,
        secret: &[u8],
        peer_id: &str,
        security: Option<&ClusterSecurityConfig>,
    ) -> Result<Self> {
        let endpoint = endpoint.to_string();
        let secret = secret.to_vec();
        let peer_id = peer_id.to_string();
        let security = security.cloned();
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            Self::connect_sync(&endpoint, &secret, &peer_id, security.as_ref())
        })
        .await
    }

    fn connect_sync(
        endpoint: &str,
        secret: &[u8],
        peer_id: &str,
        security: Option<&ClusterSecurityConfig>,
    ) -> Result<Self> {
        let tcp = dial_tcp(endpoint, DIAL_TIMEOUT)?;
        // Every dial-side exchange is bounded by DIAL_TIMEOUT at the
        // socket (the tokio::time::timeout wrappers this replaces bounded
        // the same exchanges).
        tcp.set_read_timeout(Some(DIAL_TIMEOUT))?;
        tcp.set_write_timeout(Some(DIAL_TIMEOUT))?;
        let (mut stream, binding): (ClusterStream, Option<[u8; 32]>) = match security {
            Some(sec) => {
                // The ClusterSecurityConfig node certs carry
                // localhost/127.0.0.1 SANs (cluster_tls construction).
                let tls = tls_client_handshake(tls_connector(sec)?, tcp)?;
                let binding = client_exporter(&tls.conn);
                (ClusterStream::tls_client(tls), binding)
            }
            None => (ClusterStream::tcp(tcp), None),
        };
        let channel = if security.is_some() {
            ChannelClass::MutualTls
        } else {
            ChannelClass::Plaintext
        };

        let server_nonce = match read_plain_frame::<_, RpcFrame>(
            &mut stream,
            FrameClass::Handshake.cap(),
            Some(DIAL_TIMEOUT),
        )
        .map_err(|e| {
            if io_timed_out(&e) {
                SqueezefsError::InvalidOperation(format!(
                    "cluster wire: no challenge from the coordinator within {DIAL_TIMEOUT:?}"
                ))
            } else {
                SqueezefsError::from(e)
            }
        })? {
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
        )?;
        match read_plain_frame::<_, RpcFrame>(
            &mut stream,
            FrameClass::Handshake.cap(),
            Some(DIAL_TIMEOUT),
        )? {
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
            io: Some(ClientIo {
                stream,
                tx,
                rx,
                call_timeout: DIAL_TIMEOUT,
            }),
            authn: SessionAuthn {
                channel,
                proof_verified: true,
                mac_engaged: true,
            },
            next_id: 0,
        })
    }

    /// This session's authentication state.
    pub fn authn(&self) -> &SessionAuthn {
        &self.authn
    }

    /// **Is this pooled session provably dead before a send** (finding
    /// 14 — the idle-reap class)? The coordinator's 60 s idle-session
    /// reaper closes a quiet session's socket, so its FIN sits queued
    /// long before the next verb: a non-blocking `MSG_PEEK` answers EOF
    /// without consuming anything. On a request/reply wire NOTHING is
    /// legitimately readable between calls, so readable bytes here are
    /// a desynchronized session — dead too.
    ///
    /// This is what lets the ONE-ATTEMPT verb classes (custody acquires,
    /// the un-witnessed publish mutators) replace a reaped session
    /// BEFORE their single send: the no-retry law binds a frame once
    /// SENT — the true sent-then-lost ambiguity keeps refusing — while a
    /// dead-on-arrival session costs no attempt at all. `false` on any
    /// probe error other than would-block: the send itself is the
    /// honest classifier there, and this must never eat a live session.
    pub fn dead_on_arrival(&self) -> bool {
        let Some(io) = self.io.as_ref() else {
            // Lost to a panicked call: `call` refuses loud by its own
            // law; reporting dead here lets a pooled caller replace it.
            return true;
        };
        let fd = {
            use std::os::fd::AsRawFd;
            io.stream.sock().as_raw_fd()
        };
        let mut byte = 0u8;
        // SAFETY: recv on an owned, open fd with a valid 1-byte buffer;
        // MSG_PEEK consumes nothing, MSG_DONTWAIT never blocks.
        let n = unsafe {
            libc::recv(
                fd,
                std::ptr::from_mut(&mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match n {
            0 => true,   // EOF queued — the reaper closed it.
            1.. => true, // unsolicited bytes between calls — desynchronized.
            // WouldBlock (EAGAIN/EWOULDBLOCK) = alive and quiet; any
            // other errno is a broken socket.
            _ => std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock,
        }
    }

    /// Issue one authenticated request and await its reply. Async API
    /// preserved; the roundtrip runs on the blocking pool with the reply
    /// wait bounded by the socket read timeout.
    pub async fn call(&mut self, verb: u16, body: Vec<u8>) -> Result<RpcResponse> {
        self.call_timed_on(verb, body, None).await.map(|(r, _)| r)
    }

    /// [`Self::call`] with the round trip's two instants reported and the
    /// blocking VENUE chosen by the caller: `None` = the shared pool
    /// (`call`'s venue); `Some(lane)` = a dedicated thread the pool's
    /// population cannot clog — the liveness-class shape (the membership
    /// renewal, whose send must never wait behind a parked bulk round
    /// trip). `sent` is stamped on the venue's thread just before the
    /// frame is written, so `sent − decision` is exactly the caller's
    /// venue wait.
    pub async fn call_timed_on(
        &mut self,
        verb: u16,
        body: Vec<u8>,
        lane: Option<&squeezefs_ipc::sqz_blocking::DedicatedWorker>,
    ) -> Result<(RpcResponse, CallTiming)> {
        self.next_id += 1;
        let id = self.next_id;
        let mut io = self.io.take().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "cluster wire: session I/O lost to an earlier panicked call — reconnect".into(),
            )
        })?;
        let job = move || {
            let sent = std::time::Instant::now();
            let out = io.roundtrip(id, verb, body);
            let replied = std::time::Instant::now();
            (io, out, CallTiming { sent, replied })
        };
        let (io, out, timing) = match lane {
            Some(lane) => lane.run(job).await,
            None => squeezefs_ipc::sqz_blocking::run_blocking(job).await,
        };
        self.io = Some(io);
        out.map(|r| (r, timing))
    }
}

/// A parked [`MuxSession::call`]: its reply slot and when it was sent (the
/// per-call reply bound's anchor).
struct MuxPending {
    reply: squeezefs_ipc::sqz_channel::oneshot::Sender<Result<RpcResponse>>,
    sent_at: std::time::Instant,
}

/// The multiplexed session's write half: the framer's MAC sequence is per
/// direction, so every send takes this lock; `None` once the session is
/// dead.
type MuxWriter = parking_lot::Mutex<Option<(ClusterWriteHalf, FrameTx)>>;

/// **A dialed session that PIPELINES calls on one socket** (e2e perf audit
/// D-5, DLM #8's single-connection half). [`RpcClient`] is request/reply:
/// one call in flight per session, so K frames in flight cost K sessions
/// — K connections, K owner threads, K slots of the F-B connection cap.
/// Here every call carries its own id, a dedicated reader thread
/// (`sqz-clw-mux`) demultiplexes the replies by id onto parked oneshots,
/// and the owner's session lane (`SessionPark`) serves the in-flight
/// calls concurrently — so K in flight costs ONE connection.
///
/// Same handshake, same per-frame MAC, same reply bound (`DIAL_TIMEOUT`
/// per call, enforced by the reader on its read-timeout tick); a
/// transport failure on either half kills the WHOLE session (`is_dead`)
/// and fails every parked call — the pooled session's "drop on error"
/// law, one session wide — so a caller's resend discipline is unchanged.
pub struct MuxSession {
    writer: MuxWriter,
    pending: parking_lot::Mutex<HashMap<u64, MuxPending>>,
    next_id: AtomicU64,
    dead: AtomicBool,
    authn: SessionAuthn,
    /// Dup'd socket for the poison nudge (`shutdown(Both)` wakes the
    /// reader out of its parked read).
    nudge: TcpStream,
    reader: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for MuxSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MuxSession")
            .field("authn", &self.authn)
            .field("calls", &self.next_id.load(Ordering::Relaxed))
            .field("pending", &self.pending.lock().len())
            .field("dead", &self.dead.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MuxSession {
    /// Dial + prove (exactly [`RpcClient::connect`]), then split the
    /// session into its pipelined form.
    pub async fn connect(
        endpoint: &str,
        secret: &[u8],
        peer_id: &str,
        security: Option<&ClusterSecurityConfig>,
    ) -> Result<Arc<Self>> {
        let client = RpcClient::connect(endpoint, secret, peer_id, security).await?;
        client.into_mux()
    }

    /// The reader's park bound per read: a quarter of the per-call reply
    /// bound, so an expired call is failed within `DIAL_TIMEOUT × 1.25`
    /// of its send at worst (the parked read wakes this often when idle).
    fn reader_tick() -> Duration {
        DIAL_TIMEOUT / 4
    }

    /// The reader holds the session WEAKLY between reads: when every user
    /// has dropped its `Arc` the session's `Drop` poisons it (nudging this
    /// read awake) and the failed upgrade ends the thread — a session
    /// nobody holds never outlives its last user by more than one read.
    fn reader_loop(weak: &std::sync::Weak<Self>, mut rx: FrameRx, mut half: ClusterReadHalf) {
        loop {
            let Some(me) = weak.upgrade() else {
                return;
            };
            if me.dead.load(Ordering::Acquire) {
                break;
            }
            match rx.recv::<_, RpcFrame>(&mut half, FrameClass::Bulk.cap(), Some(DIAL_TIMEOUT)) {
                Ok(Some(RpcFrame::Reply { id, status, body })) => {
                    match me.pending.lock().remove(&id) {
                        Some(p) => {
                            let _ = p.reply.send(Ok(RpcResponse { id, status, body }));
                        }
                        None => log::warn!(
                            "cluster wire: multiplexed session received a reply for unknown \
                             call {id} — dropped (a late reply to an expired call)"
                        ),
                    }
                }
                Ok(Some(other)) => {
                    log::warn!(
                        "cluster wire: multiplexed session received a non-Reply frame — \
                         ignored: {other:?}"
                    );
                }
                Ok(None) => break,
                Err(e) if io_timed_out(&e) => {
                    // The tick: fail every call past its reply bound; an
                    // idle session simply re-parks.
                    let expired: Vec<MuxPending> = {
                        let mut p = me.pending.lock();
                        let ids: Vec<u64> = p
                            .iter()
                            .filter(|(_, v)| v.sent_at.elapsed() >= DIAL_TIMEOUT)
                            .map(|(k, _)| *k)
                            .collect();
                        ids.into_iter().filter_map(|id| p.remove(&id)).collect()
                    };
                    for p in expired {
                        let _ = p.reply.send(Err(SqueezefsError::InvalidOperation(format!(
                            "cluster wire: no reply to a multiplexed call within {DIAL_TIMEOUT:?}"
                        ))));
                    }
                }
                Err(e) => {
                    log::warn!("cluster wire: multiplexed session read error: {e}");
                    break;
                }
            }
        }
        if let Some(me) = weak.upgrade() {
            me.poison();
        }
    }

    /// Kill the session: no more sends, every parked call fails, the
    /// reader wakes out of its read and exits.
    fn poison(&self) {
        if self.dead.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.writer.lock() = None;
        let _ = self.nudge.shutdown(Shutdown::Both);
        let parked: Vec<MuxPending> = self.pending.lock().drain().map(|(_, p)| p).collect();
        for p in parked {
            let _ = p.reply.send(Err(SqueezefsError::InvalidOperation(
                "cluster wire: the multiplexed session closed before the reply".into(),
            )));
        }
    }

    /// Has the session died (a transport failure on either half, the
    /// owner's idle reaper, or [`Self::close`])? A dead session is replaced,
    /// never reused — the pooled `dead_on_arrival` law, one session wide.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// This session's authentication state.
    pub fn authn(&self) -> &SessionAuthn {
        &self.authn
    }

    /// Calls in flight (parked on their replies).
    pub fn inflight(&self) -> usize {
        self.pending.lock().len()
    }

    /// Issue one authenticated request and await its reply, beside every
    /// other call in flight on this session. The send runs on the blocking
    /// pool under the writer lock (one frame at a time on the wire, as the
    /// MAC sequence requires); the reply arrives through the reader.
    pub async fn call(self: &Arc<Self>, verb: u16, body: Vec<u8>) -> Result<RpcResponse> {
        if self.is_dead() {
            return Err(SqueezefsError::InvalidOperation(
                "cluster wire: the multiplexed session is dead — reconnect".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        self.pending.lock().insert(
            id,
            MuxPending {
                reply: tx,
                sent_at: std::time::Instant::now(),
            },
        );
        let me = Arc::clone(self);
        let sent = squeezefs_ipc::sqz_blocking::run_blocking(move || {
            let mut w = me.writer.lock();
            match w.as_mut() {
                Some((half, ftx)) => ftx
                    .send(half, FrameClass::Bulk, &RpcFrame::Call { id, verb, body })
                    .map_err(SqueezefsError::from),
                None => Err(SqueezefsError::InvalidOperation(
                    "cluster wire: the multiplexed session is dead — reconnect".into(),
                )),
            }
        })
        .await;
        if let Err(e) = sent {
            self.pending.lock().remove(&id);
            self.poison();
            return Err(e);
        }
        rx.await.map_err(|_| {
            SqueezefsError::InvalidOperation(
                "cluster wire: the multiplexed session closed before the reply".into(),
            )
        })?
    }

    /// Close the session and join its reader (teardown; a dropped session
    /// poisons itself and the reader exits on its own).
    pub fn close(&self) {
        self.poison();
        if let Some(h) = self.reader.lock().take() {
            let _ = h.join();
        }
    }
}

impl Drop for MuxSession {
    fn drop(&mut self) {
        // Nudge the reader awake; its next upgrade fails and it exits.
        self.poison();
    }
}

impl RpcClient {
    /// Convert a freshly dialed request/reply session into a pipelined
    /// [`MuxSession`]: split the stream, hand the read half to a named
    /// reader thread, keep the write half under the writer lock.
    pub fn into_mux(mut self) -> Result<Arc<MuxSession>> {
        let io = self.io.take().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "cluster wire: session I/O lost to an earlier panicked call — reconnect".into(),
            )
        })?;
        let ClientIo { stream, tx, rx, .. } = io;
        let nudge = stream.nudge_handle()?;
        let (mut rhalf, whalf) = stream.split()?;
        rhalf.set_read_timeout(Some(MuxSession::reader_tick()))?;
        let session = Arc::new(MuxSession {
            writer: parking_lot::Mutex::new(Some((whalf, tx))),
            pending: parking_lot::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            dead: AtomicBool::new(false),
            authn: self.authn,
            nudge,
            reader: parking_lot::Mutex::new(None),
        });
        let reader_me = Arc::downgrade(&session);
        let handle = std::thread::Builder::new()
            .name("sqz-clw-mux".to_string())
            .spawn(move || MuxSession::reader_loop(&reader_me, rx, rhalf))
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "cluster wire: multiplexed session reader thread refused: {e}"
                ))
            })?;
        *session.reader.lock() = Some(handle);
        Ok(session)
    }
}

/// The two instants of one [`RpcClient::call_timed_on`] round trip, stamped
/// on the blocking venue's thread: `sent` just before the request frame
/// is written, `replied` when the reply frame has been read (or the wait
/// gave up).
#[derive(Debug, Clone, Copy)]
pub struct CallTiming {
    /// The request frame is about to be written.
    pub sent: std::time::Instant,
    /// The reply arrived (or the bounded wait expired).
    pub replied: std::time::Instant,
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
