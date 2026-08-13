//! DLM **S3 `cluster_wire`** — the ONE cluster transport every later
//! stage rides (pre-rc spec §6.7 *Transport*, §6.9 stage S3; execution
//! plan §6.3, rulings **D2**/**D8**/**D10**), red-first.
//!
//! What this binary pins, in the order the wire performs it:
//!
//! 1. **Binary framing** (postcard/bincode class — `serde_json` frame
//!    decode measured at **32.6 µs** for a 64-checksum mover shard
//!    against §6.5's **10 µs** custody budget). Length-prefixed,
//!    schema-versioned, per-class caps, chunk-bounded body commit, and a
//!    per-class body deadline — VAL-6's bounds carried FORWARD onto the
//!    transport they should have been on, never re-derived.
//! 2. **Zero-config mutual authentication** (D2): a **server-issued**
//!    single-use nonce inside a freshness window, a challenge-response
//!    possession proof of the `job:enroll` meta-KV secret, then a
//!    **session key derived from that secret** (channel-bindable) and a
//!    **per-frame MAC** with a per-direction sequence — so authentication
//!    survives past the handshake instead of ending at it. Storage trust
//!    is the root: whoever can read the shared metadata volume is
//!    definitionally inside the trust domain.
//! 3. **The ladder keys on an authenticated channel** — never on
//!    `transport == "tls"`. A CA-less `ClusterSecurityConfig` is REFUSED
//!    (the accept-everything certificate verifier is deleted from the
//!    tree), and verify-read sampling stays admissible only on a
//!    confidential+peer-authenticated channel.
//! 4. **Owner-side RPC runs on the connection's own named OS thread**
//!    (`sqz-clw-conn`, thread-per-connection since the rip-tokio wire
//!    conversion; bounded by the `ConnGate` cap) — never on the
//!    conveyor's task: the conveyor is a serialized ~0.78 ms server at
//!    ρ ≈ 0.92 and an RPC on it would multiply through the queueing
//!    formula (§6.5 item 1).
//! 5. **DISC-1 peer auto-discovery**: the shared volume IS the
//!    rendezvous — `client:{uuid}` records already carry the endpoint, so
//!    discovery is an enumeration, never a multicast protocol.
//! 6. **The RTT instrument** — the S8 price (risk **R1**, ruling D10)
//!    measured with THIS wire. Loopback here is a floor, never a fabric
//!    row.
//!
//! NOT here (KD-15, and S4 owns them): lock verbs. This wire carries
//! framing + authn + discovery + the job-wire port and nothing else.

use squeezefs::cluster_wire as cw;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Read as _, Write as _};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// The allocation instrument (the VAL-6 pattern): a per-thread PEAK
// single-allocation gauge. Thread-local and const-initialized, so the hook
// itself never allocates and a parallel test cannot pollute an armed window.
// ---------------------------------------------------------------------------

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

struct PeakAlloc;

// SAFETY: delegates verbatim to `System`; the accounting side effect is a
// const-initialized thread-local Cell update that never allocates.
unsafe impl GlobalAlloc for PeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn record(size: usize) {
    ARMED.with(|a| {
        if a.get() {
            PEAK.with(|p| p.set(p.get().max(size)));
        }
    });
}

#[global_allocator]
static GLOBAL: PeakAlloc = PeakAlloc;

fn peak_alloc<T>(f: impl FnOnce() -> T) -> (T, usize) {
    PEAK.with(|p| p.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, PEAK.with(|p| p.get()))
}

const SECRET: &[u8] = b"storage-trust-enrollment-secret-32b";

/// The mover-shard shape the 32.6 µs `serde_json` row was measured on
/// (64 destination checksums — blocks move over shared storage, only
/// their checksums ride the wire).
fn mover_shard_frame() -> cw::RpcFrame {
    let body: Vec<u8> = (0..64u64)
        .flat_map(|i| {
            let mut v = Vec::with_capacity(24);
            v.extend_from_slice(&i.to_le_bytes());
            v.extend_from_slice(&(4u64 * 1024 * 1024).to_le_bytes());
            v.extend_from_slice(&0x9e37_79b9_7f4a_7c15u64.wrapping_mul(i + 1).to_le_bytes());
            v
        })
        .collect();
    cw::RpcFrame::Call {
        id: 7,
        verb: cw::VERB_PING,
        body,
    }
}

// ---------------------------------------------------------------------------
// 1. Framing: binary, bounded, deadlined
// ---------------------------------------------------------------------------

#[test]
fn framing_is_binary_and_smaller_than_the_json_it_replaces() {
    // The measured reason the codec changed: VAL-6's own microbench
    // prices `serde_json` frame decode at 32.6 µs for this shape against
    // §6.5's 10 µs custody budget. A binary codec is not a preference,
    // it is the budget.
    let encode = |frame: &cw::RpcFrame| -> Vec<u8> {
        let mut wire: Vec<u8> = Vec::new();
        cw::write_plain_frame(&mut wire, cw::FrameClass::Bulk, frame).expect("encode");
        assert!(wire.len() > 4, "a frame is a length prefix plus a body");
        wire[4..].to_vec()
    };

    // A frame whose fields are all text: JSON would have to emit its
    // structure as braces, quotes and field names. Binary framing emits
    // none of them.
    let handshake = cw::RpcFrame::Challenge {
        schema: cw::CLUSTER_WIRE_SCHEMA,
        server_nonce: "0f1c2d3e-4a5b-6c7d-8e9f-a0b1c2d3e4f5".into(),
        freshness_ms: 30_000,
    };
    let body = encode(&handshake);
    for token in [b'{', b'}', b'"'] {
        assert!(
            !body.contains(&token),
            "JSON structure in the body means the codec did not change: {:?}",
            String::from_utf8_lossy(&body)
        );
    }
    for name in ["schema", "server_nonce", "freshness_ms", "Challenge"] {
        assert!(
            !String::from_utf8_lossy(&body).contains(name),
            "a binary frame carries no field NAMES ({name} found)"
        );
    }

    // And on the shape the 32.6 µs row was measured on, it is smaller
    // than the JSON it replaces — fewer bytes to move and to parse.
    let shard = mover_shard_frame();
    let binary = encode(&shard);
    let json = serde_json::to_vec(&shard).expect("json for the comparison");
    assert!(
        binary.len() < json.len(),
        "binary framing must be smaller than the JSON it replaces: {} vs {} B",
        binary.len(),
        json.len()
    );
    // The row the evidence note cites (`--nocapture`).
    eprintln!(
        "cluster_wire frame bytes (64-checksum mover shard): binary {} B vs json {} B ({:.2}×)",
        binary.len(),
        json.len(),
        json.len() as f64 / binary.len() as f64
    );
}

#[test]
fn frames_round_trip_through_every_class() {
    for class in [
        cw::FrameClass::Handshake,
        cw::FrameClass::Control,
        cw::FrameClass::Bulk,
    ] {
        let frame = cw::RpcFrame::Challenge {
            schema: cw::CLUSTER_WIRE_SCHEMA,
            server_nonce: "nonce-1".into(),
            freshness_ms: 30_000,
        };
        let mut buf: Vec<u8> = Vec::new();
        cw::write_plain_frame(&mut buf, class, &frame).expect("encode");
        let mut cur = std::io::Cursor::new(buf);
        let back: cw::RpcFrame = cw::read_plain_frame(&mut cur, class.cap(), None)
            .expect("decode")
            .expect("one frame");
        match back {
            cw::RpcFrame::Challenge { schema, .. } => {
                assert_eq!(schema, cw::CLUSTER_WIRE_SCHEMA)
            }
            other => panic!("round-trip changed the frame: {other:?}"),
        }
    }
    // Clean EOF at a frame boundary is `Ok(None)`, never an error.
    let mut empty = std::io::Cursor::new(Vec::new());
    let none: Option<cw::RpcFrame> =
        cw::read_plain_frame(&mut empty, cw::FrameClass::Bulk.cap(), None).expect("clean eof");
    assert!(none.is_none());
}

#[test]
fn class_caps_are_ordered_and_named() {
    // Three classes, because a handshake, a lock verb and a mover shard
    // are three different budgets. An unauthenticated peer spends the
    // smallest one.
    assert!(cw::FrameClass::Handshake.cap() < cw::FrameClass::Control.cap());
    assert!(cw::FrameClass::Control.cap() < cw::FrameClass::Bulk.cap());
    assert!(
        cw::FrameClass::Handshake.cap() <= cw::FrameClass::Bulk.cap() / 64,
        "the pre-authn class must be far below the bulk cap"
    );
    for class in [
        cw::FrameClass::Handshake,
        cw::FrameClass::Control,
        cw::FrameClass::Bulk,
    ] {
        assert!(
            class.cap_name().contains("FRAME_BYTES"),
            "a refusal must be able to name its own cap: {}",
            class.cap_name()
        );
    }
}

#[test]
fn oversize_frame_refuses_on_both_sides_naming_the_cap() {
    // Encode side: never emit past the class cap.
    let huge = cw::RpcFrame::Call {
        id: 1,
        verb: cw::VERB_PING,
        body: vec![0u8; cw::FrameClass::Handshake.cap() as usize + 64],
    };
    let mut buf: Vec<u8> = Vec::new();
    let err = cw::write_plain_frame(&mut buf, cw::FrameClass::Handshake, &huge)
        .expect_err("an oversize frame is never emitted");
    assert!(
        err.to_string()
            .contains(cw::FrameClass::Handshake.cap_name()),
        "the encode refusal names its class cap: {err}"
    );
    assert!(buf.is_empty(), "nothing is written when the cap refuses");

    // Decode side: a length prefix past the cap is a comparison, not
    // an allocation.
    let mut cur = std::io::Cursor::new((cw::FrameClass::Bulk.cap() + 1).to_be_bytes().to_vec());
    let err = cw::read_plain_frame::<_, cw::RpcFrame>(&mut cur, cw::FrameClass::Bulk.cap(), None)
        .expect_err("a length prefix past the cap refuses");
    assert!(
        err.to_string()
            .contains(&cw::FrameClass::Bulk.cap().to_string()),
        "the decode refusal names the cap it enforced: {err}"
    );
}

#[test]
fn lying_length_prefix_allocates_one_chunk_not_the_cap() {
    // Carried forward from VAL-6: `vec![0u8; len]` before `read_exact`
    // meant four attacker bytes bought a 16 MiB allocation per
    // connection. The property belongs to the transport now.
    let (res, peak) = peak_alloc(|| {
        let mut cur = std::io::Cursor::new(cw::FrameClass::Bulk.cap().to_be_bytes().to_vec());
        cw::read_plain_frame::<_, cw::RpcFrame>(&mut cur, cw::FrameClass::Bulk.cap(), None)
    });
    assert!(
        res.is_err(),
        "a body-less bulk-cap claim is EOF-truncated, not a frame"
    );
    assert!(
        peak <= cw::FRAME_CHUNK_BYTES * 2,
        "body allocation must stay chunk-bounded: peak {peak} B for a {} B claim",
        cw::FrameClass::Bulk.cap()
    );
}

#[test]
fn stalled_frame_body_hits_the_class_deadline() {
    // Deadlines ride the SOCKET since the rip-tokio conversion: each
    // blocking read is bounded by the caller-set `set_read_timeout`, and
    // the framer's `body_timeout` bounds the WHOLE body between chunks.
    // Socket-timeout expiry surfaces as `WouldBlock` on Linux
    // (`TimedOut` elsewhere) — the product's own `io_timed_out`
    // predicate reads both as a timeout, so this asserts the same class.
    let (mut client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    server
        .set_read_timeout(Some(Duration::from_millis(150)))
        .expect("socket read timeout");
    let mut head = 4096u32.to_be_bytes().to_vec();
    head.extend_from_slice(b"12345678");
    client.write_all(&head).expect("prefix + dribble");
    let started = std::time::Instant::now();
    let err = cw::read_plain_frame::<_, cw::RpcFrame>(
        &mut server,
        cw::FrameClass::Handshake.cap(),
        Some(Duration::from_millis(150)),
    )
    .expect_err("a stalled body must time out");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "the deadline surfaces as a timeout-class error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline fired, not an unbounded park"
    );
    drop(client);
}

#[test]
fn accept_backoff_ladder_doubles_and_caps() {
    // The bound carried forward, not re-derived: a persistent accept
    // error (EMFILE/ENFILE) must never spin a core.
    let first = cw::next_accept_backoff(None);
    assert_eq!(first, cw::ACCEPT_BACKOFF_START);
    let mut d = first;
    let mut steps = 0;
    while d < cw::ACCEPT_BACKOFF_MAX {
        let next = cw::next_accept_backoff(Some(d));
        assert!(next > d, "monotone: {next:?} after {d:?}");
        assert!(next <= cw::ACCEPT_BACKOFF_MAX, "capped: {next:?}");
        d = next;
        steps += 1;
        assert!(steps < 64);
    }
    assert_eq!(
        cw::next_accept_backoff(Some(cw::ACCEPT_BACKOFF_MAX)),
        cw::ACCEPT_BACKOFF_MAX,
        "idempotent at the cap"
    );
}

// ---------------------------------------------------------------------------
// 2. Zero-config mutual authentication (D2)
// ---------------------------------------------------------------------------

fn gate(freshness: Duration) -> cw::AuthnGate {
    cw::AuthnGate::new(
        SECRET.to_vec(),
        cw::AuthnConfig {
            freshness,
            ..cw::AuthnConfig::default()
        },
    )
}

#[test]
fn the_nonce_is_server_issued_single_use_and_freshness_windowed() {
    // The freshness window rides a deterministic clock SEAM (never a
    // sleep): the test advances the gate's own millisecond clock.
    let g = gate(Duration::from_secs(30));
    let challenge = g.issue_challenge();
    assert_eq!(challenge.schema, cw::CLUSTER_WIRE_SCHEMA);
    assert!(!challenge.server_nonce.is_empty());
    assert_eq!(challenge.freshness_ms, 30_000);
    assert_eq!(g.outstanding_challenges(), 1);

    let peer_nonce = "peer-entropy";
    let mac = cw::proof_mac(SECRET, "peer-a", &challenge.server_nonce, peer_nonce);
    let claim = cw::ProofClaim {
        schema: cw::CLUSTER_WIRE_SCHEMA,
        peer_id: "peer-a",
        server_nonce: &challenge.server_nonce,
        peer_nonce,
        mac: &mac,
    };
    match g.verify(claim.clone(), None) {
        cw::Verdict::Admit(_) => {}
        cw::Verdict::Refuse(r) => panic!("a fresh, well-formed proof must admit: {r}"),
    }
    assert_eq!(
        g.outstanding_challenges(),
        0,
        "an accepted nonce is CONSUMED"
    );
    // Replay of the identical hello: the MAC still verifies, and that
    // is exactly why the nonce registry has to be the gate.
    match g.verify(claim, None) {
        cw::Verdict::Refuse(r) => assert!(
            r.contains("replay") || r.contains("single-use") || r.contains("spent"),
            "the refusal says why: {r}"
        ),
        cw::Verdict::Admit(_) => panic!("a replayed hello must never admit"),
    }

    // Freshness: a nonce answered past the window is refused even
    // though the proof is arithmetically correct.
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let g = cw::AuthnGate::with_clock(
        SECRET.to_vec(),
        cw::AuthnConfig {
            freshness: Duration::from_millis(500),
            ..cw::AuthnConfig::default()
        },
        cw::WireClock::manual(clock.clone()),
    );
    let challenge = g.issue_challenge();
    let mac = cw::proof_mac(SECRET, "peer-b", &challenge.server_nonce, "n2");
    clock.fetch_add(1_500, Ordering::SeqCst);
    match g.verify(
        cw::ProofClaim {
            schema: cw::CLUSTER_WIRE_SCHEMA,
            peer_id: "peer-b",
            server_nonce: &challenge.server_nonce,
            peer_nonce: "n2",
            mac: &mac,
        },
        None,
    ) {
        cw::Verdict::Refuse(r) => assert!(
            r.contains("fresh") || r.contains("expired"),
            "the refusal names the window: {r}"
        ),
        cw::Verdict::Admit(_) => panic!("a stale challenge must not admit"),
    }
}

#[test]
fn proof_requires_the_storage_secret_and_bounds_the_peer_id() {
    let g = gate(Duration::from_secs(30));
    let c = g.issue_challenge();
    // Wrong secret ⇒ refused. This is the whole trust root: the proof
    // is computable only by a principal that can read the volume.
    let bad = cw::proof_mac(b"not-the-secret", "peer-a", &c.server_nonce, "n");
    match g.verify(
        cw::ProofClaim {
            schema: cw::CLUSTER_WIRE_SCHEMA,
            peer_id: "peer-a",
            server_nonce: &c.server_nonce,
            peer_nonce: "n",
            mac: &bad,
        },
        None,
    ) {
        cw::Verdict::Refuse(r) => {
            assert!(r.contains("mac") || r.contains("membership"), "reason: {r}")
        }
        cw::Verdict::Admit(_) => panic!("a proof under the wrong secret must never admit"),
    }
    // A schema mismatch refuses loud rather than being interpreted.
    let c = g.issue_challenge();
    let mac = cw::proof_mac(SECRET, "peer-a", &c.server_nonce, "n");
    match g.verify(
        cw::ProofClaim {
            schema: cw::CLUSTER_WIRE_SCHEMA + 7,
            peer_id: "peer-a",
            server_nonce: &c.server_nonce,
            peer_nonce: "n",
            mac: &mac,
        },
        None,
    ) {
        cw::Verdict::Refuse(r) => assert!(r.contains("schema"), "reason: {r}"),
        cw::Verdict::Admit(_) => panic!("a foreign schema must refuse"),
    }
    // An attacker-chosen identity is bounded — it lands in log lines
    // and durable records.
    let c = g.issue_challenge();
    let long = "x".repeat(cw::AuthnConfig::default().max_peer_id_bytes + 1);
    let mac = cw::proof_mac(SECRET, &long, &c.server_nonce, "n");
    match g.verify(
        cw::ProofClaim {
            schema: cw::CLUSTER_WIRE_SCHEMA,
            peer_id: &long,
            server_nonce: &c.server_nonce,
            peer_nonce: "n",
            mac: &mac,
        },
        None,
    ) {
        cw::Verdict::Refuse(r) => assert!(r.contains("peer_id") || r.contains("id"), "{r}"),
        cw::Verdict::Admit(_) => panic!("an unbounded identity must refuse"),
    }
}

#[test]
fn mac_eq_stays_byte_for_byte_and_length_checked() {
    // Spec §8 invariant: `mac_eq` is already a correct constant-time
    // comparison and must be preserved verbatim.
    assert!(cw::mac_eq("deadbeef", "deadbeef"));
    assert!(!cw::mac_eq("deadbeef", "deadbeee"));
    assert!(!cw::mac_eq("deadbeef", "deadbee"));
    assert!(!cw::mac_eq("", "x"));
    assert!(cw::mac_eq("", ""));
}

#[test]
fn session_key_binds_the_secret_both_nonces_the_peer_and_the_channel() {
    let base = cw::session_key(SECRET, "peer-a", "s-nonce", "p-nonce", None);
    assert_eq!(
        base,
        cw::session_key(SECRET, "peer-a", "s-nonce", "p-nonce", None),
        "both sides derive the same key from the same inputs — that is the point"
    );
    for other in [
        cw::session_key(b"other-secret", "peer-a", "s-nonce", "p-nonce", None),
        cw::session_key(SECRET, "peer-b", "s-nonce", "p-nonce", None),
        cw::session_key(SECRET, "peer-a", "s-nonce-2", "p-nonce", None),
        cw::session_key(SECRET, "peer-a", "s-nonce", "p-nonce-2", None),
        cw::session_key(
            SECRET,
            "peer-a",
            "s-nonce",
            "p-nonce",
            Some(b"tls-exporter"),
        ),
    ] {
        assert_ne!(
            base, other,
            "every input must be bound into the session key"
        );
    }
    // The key never leaks through Debug.
    let shown = format!("{base:?}");
    assert!(
        !shown.contains("["),
        "key material must never reach a log line: {shown}"
    );
}

#[test]
fn per_frame_mac_rejects_tamper_reorder_replay_and_reflection() {
    let key = cw::session_key(SECRET, "peer-a", "s", "p", None);
    let frame = cw::RpcFrame::Call {
        id: 1,
        verb: cw::VERB_PING,
        body: b"payload".to_vec(),
    };

    // Honest path: coordinator → peer, in order.
    let (mut tx, _) = cw::session_framers(&key, cw::Role::Coordinator);
    let (_, mut rx) = cw::session_framers(&key, cw::Role::Peer);
    let mut wire: Vec<u8> = Vec::new();
    tx.send(&mut wire, cw::FrameClass::Control, &frame)
        .expect("send");
    let first = wire.clone();
    tx.send(&mut wire, cw::FrameClass::Control, &frame)
        .expect("send 2");
    let mut cur = std::io::Cursor::new(wire.clone());
    for _ in 0..2 {
        let got: Option<cw::RpcFrame> = rx
            .recv(&mut cur, cw::FrameClass::Control.cap(), None)
            .expect("authenticated frames decode");
        assert!(got.is_some());
    }
    assert_eq!(tx.frames(), 2);
    assert_eq!(rx.frames(), 2);

    // Tamper: flip one payload byte.
    let mut tampered = first.clone();
    let last = tampered.len() - cw::MAC_BYTES - 1;
    tampered[last] ^= 0x40;
    let (_, mut rx) = cw::session_framers(&key, cw::Role::Peer);
    let err = rx
        .recv::<_, cw::RpcFrame>(
            &mut std::io::Cursor::new(tampered),
            cw::FrameClass::Control.cap(),
            None,
        )
        .expect_err("a tampered frame must not decode");
    assert!(
        err.to_string().contains("mac") || err.to_string().contains("authentic"),
        "the refusal says what failed: {err}"
    );

    // Replay of frame 1 in position 2: the sequence is inside the MAC.
    let mut replayed = first.clone();
    replayed.extend_from_slice(&first);
    let (_, mut rx) = cw::session_framers(&key, cw::Role::Peer);
    rx.recv::<_, cw::RpcFrame>(
        &mut std::io::Cursor::new(replayed.clone()),
        cw::FrameClass::Control.cap(),
        None,
    )
    .expect("first frame is honest");
    let mut cur = std::io::Cursor::new(replayed);
    let _ = rx.recv::<_, cw::RpcFrame>(&mut cur, cw::FrameClass::Control.cap(), None);
    let (_, mut rx2) = cw::session_framers(&key, cw::Role::Peer);
    let mut cur = std::io::Cursor::new(first.clone());
    rx2.recv::<_, cw::RpcFrame>(&mut cur, cw::FrameClass::Control.cap(), None)
        .expect("frame 1 at position 1");
    let mut cur = std::io::Cursor::new(first.clone());
    let err = rx2
        .recv::<_, cw::RpcFrame>(&mut cur, cw::FrameClass::Control.cap(), None)
        .expect_err("frame 1 replayed at position 2 must refuse");
    assert!(
        err.to_string().contains("mac") || err.to_string().contains("sequence"),
        "reason: {err}"
    );

    // Reflection: a coordinator→peer frame read by a COORDINATOR
    // (i.e. bounced back at its author) must refuse — the direction
    // tag is inside the MAC.
    let (_, mut rx_same_dir) = cw::session_framers(&key, cw::Role::Coordinator);
    let err = rx_same_dir
        .recv::<_, cw::RpcFrame>(
            &mut std::io::Cursor::new(first),
            cw::FrameClass::Control.cap(),
            None,
        )
        .expect_err("a reflected frame must refuse");
    assert!(err.to_string().contains("mac"), "reason: {err}");
}

#[test]
fn the_ladder_keys_on_an_authenticated_channel_never_on_tls_presence() {
    // VAL-6's re-keying, restated on cluster_wire's own state: the
    // verification-strength ladder needs a peer-authenticated,
    // CONFIDENTIAL channel. Storage-trust authn is what makes plaintext
    // *authenticated* (integrity + identity) — it does not make it
    // confidential, so sampling stays refused there.
    let plain_authed = cw::SessionAuthn {
        channel: cw::ChannelClass::Plaintext,
        proof_verified: true,
        mac_engaged: true,
    };
    assert!(plain_authed.authenticated());
    assert!(
        !plain_authed.verify_sampling_admissible(),
        "plaintext keeps mandatory-100 % verify-reads (Issue-30)"
    );
    let mtls = cw::SessionAuthn {
        channel: cw::ChannelClass::MutualTls,
        proof_verified: true,
        mac_engaged: true,
    };
    assert!(mtls.authenticated() && mtls.verify_sampling_admissible());
    for broken in [
        cw::SessionAuthn {
            channel: cw::ChannelClass::MutualTls,
            proof_verified: false,
            mac_engaged: true,
        },
        cw::SessionAuthn {
            channel: cw::ChannelClass::MutualTls,
            proof_verified: true,
            mac_engaged: false,
        },
    ] {
        assert!(
            !broken.authenticated() && !broken.verify_sampling_admissible(),
            "a missing rung is never a lesser class, it is unauthenticated"
        );
    }
}

#[test]
fn a_ca_less_security_config_is_refused_not_downgraded() {
    // The accept-everything certificate verifier is DELETED. A CA-less
    // `ClusterSecurityConfig` used to install `.dangerous()` client-side
    // and `with_no_client_auth()` server-side; on this wire it refuses.
    use squeezefs::tiering::cluster_tls::ClusterSecurityConfig;
    let err = match cw::tls_acceptor(&ClusterSecurityConfig::default()) {
        Err(e) => e,
        Ok(_) => panic!("a CA-less TLS config must refuse, never install a dangerous verifier"),
    };
    assert!(
        format!("{err}").contains("ca_key") || format!("{err}").contains("CA"),
        "the refusal names what is missing: {err}"
    );
    let err = match cw::tls_connector(&ClusterSecurityConfig {
        ca_cert: Some(vec![1, 2, 3]),
        ca_key: None,
    }) {
        Err(e) => e,
        Ok(_) => panic!("a CA cert with no key can never make an authenticated channel"),
    };
    assert!(format!("{err}").contains("ca_key") || format!("{err}").contains("CA key"));
}

// ---------------------------------------------------------------------------
// 4. Owner-side RPC runs on the connection's own named OS thread
// ---------------------------------------------------------------------------
//
// The pinned `ServicePool` (and its `current_service_thread` marker) was
// DELETED by the rip-tokio wire conversion: the listener now serves
// thread-per-connection, bounded by the `ConnGate` cap. The venue rule the
// pool test pinned — owner-side RPC never runs on the caller's/conveyor's
// runtime — survives on the new mechanism and is pinned below
// (`rpc_service_calls_execute_on_the_connections_own_named_thread`).

#[test]
fn default_service_threads_derivation_is_bounded() {
    // Retained across the conversion for the callers that size
    // `RpcListenerConfig::service_threads` with it (the value no longer
    // allocates lanes — serve threads are bounded by `max_connections`).
    // The derivation law is unchanged: derived from the core count,
    // never oversubscribing the box.
    let n = cw::default_service_threads();
    assert!(n >= 1, "at least one owner-side lane");
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    assert!(
        n <= cpus.max(1),
        "the derivation never oversubscribes the box: {n} lanes on {cpus} cpus"
    );
}

// ---------------------------------------------------------------------------
// 5. DISC-1 — the shared volume IS the rendezvous
// ---------------------------------------------------------------------------

fn reg(
    id: &str,
    endpoint: Option<&str>,
    fresh: bool,
    kind: &'static str,
) -> squeezefs::meta_backend::kv::backend::MountRegistration {
    squeezefs::meta_backend::kv::backend::MountRegistration {
        key: format!("client:{id}"),
        kind,
        id: id.to_string(),
        pid: Some(42),
        boot: None,
        heartbeat_ts: Some(1_700_000_000),
        age_secs: Some(if fresh { 3 } else { 900 }),
        heartbeat_fresh: fresh,
        holder_provably_dead: false,
        job_endpoint: endpoint.map(str::to_string),
    }
}

#[test]
fn discovery_enumerates_fresh_endpoints_skips_self_and_dedupes() {
    // DISC-1 (ruling D2: peers are AUTO-DISCOVERED, never manually
    // configured). No multicast protocol: `client:{uuid}` records already
    // carry the endpoint, so discovery is an enumeration of the records
    // the mount heartbeat already writes.
    let regs = vec![
        reg("a", Some("10.0.0.1:7000"), true, "client"),
        reg("b", Some("10.0.0.2:7000"), true, "client"),
        reg("b", Some("10.0.0.2:7000"), true, "client"), // same record, two volumes
        reg("self", Some("10.0.0.9:7000"), true, "client"),
        reg("stale", Some("10.0.0.3:7000"), false, "client"), // aged past the TTL
        reg("no-endpoint", None, true, "client"),             // a non-coordinator mount
        reg("writer", Some("10.0.0.4:7000"), true, "writer"), // a guard claim, not a peer
    ];
    let peers = cw::peers_from_registrations(&regs, Some("self"));
    let ids: Vec<&str> = peers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["a", "b"],
        "fresh client records with an endpoint, deduped, deterministic order; \
         self, stale, endpointless and writer records excluded"
    );
    assert_eq!(peers[0].endpoint, "10.0.0.1:7000");
    assert!(peers.iter().all(|p| p.fresh));
    // Without a self id every fresh endpoint is a peer (the offline
    // coordinator / `job worker` verb shape).
    assert_eq!(cw::peers_from_registrations(&regs, None).len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_reads_the_records_the_shared_volume_already_carries() {
    use squeezefs::meta_backend::Metadata;
    let meta_file = tempfile::NamedTempFile::new().expect("tempfile");
    squeezefs::meta_backend::kv::builder::format_v3(
        meta_file.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(b"{\"name\":\"clusterwire\"}".to_vec()),
        },
    )
    .await
    .expect("format v3");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta_file.path())
        .await
        .expect("open v3");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    // Exactly the record the mount heartbeat writes (fuse_client
    // `refresh_client_registration`): the endpoint is an ADDITIVE field.
    routed
        .setxattr(
            1,
            "client:coordinator-uuid",
            format!("{{\"ts\":{now},\"pid\":7,\"job_endpoint\":\"127.0.0.1:7100\"}}").as_bytes(),
        )
        .await
        .expect("write the registration");
    routed
        .setxattr(
            1,
            "client:reader-uuid",
            format!("{{\"ts\":{now},\"pid\":8}}").as_bytes(),
        )
        .await
        .expect("write a non-coordinator registration");

    let peers = cw::discover_peers(&routed, None).await;
    assert_eq!(
        peers.len(),
        1,
        "one endpoint-carrying live record: {peers:?}"
    );
    assert_eq!(peers[0].endpoint, "127.0.0.1:7100");
    assert_eq!(
        cw::discover_endpoint(&routed).await.as_deref(),
        Some("127.0.0.1:7100"),
        "the coordinator dial address comes off the rendezvous, not from configuration"
    );
    assert!(
        cw::discover_peers(&routed, Some("coordinator-uuid"))
            .await
            .is_empty(),
        "a mount never discovers itself"
    );
}

// ---------------------------------------------------------------------------
// 6. The RPC surface end to end + the RTT instrument (S8's price, risk R1)
// ---------------------------------------------------------------------------

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_peer_round_trips_rpc_over_the_wire() {
    let host = cw::RpcListener::start(listener_cfg(), SECRET.to_vec(), Arc::new(cw::PingService))
        .expect("listener starts");
    let endpoint = host.endpoint().to_string();

    let mut client = cw::RpcClient::connect(&endpoint, SECRET, "peer-1", None)
        .await
        .expect("storage-trust enrollment");
    assert!(
        client.authn().authenticated(),
        "the session is authenticated: proof + per-frame MAC"
    );
    assert_eq!(client.authn().channel, cw::ChannelClass::Plaintext);

    for i in 0..8u8 {
        let reply = client
            .call(cw::VERB_PING, vec![i; 16])
            .await
            .expect("ping round trip");
        assert_eq!(reply.status, 0);
        assert_eq!(reply.body, vec![i; 16], "the ping echoes its body");
    }
    let stats = host.stats();
    assert_eq!(stats.sessions_admitted, 1);
    assert_eq!(stats.requests_served, 8, "engagement is exact: {stats:?}");
    assert_eq!(stats.mac_failures, 0);
    assert_eq!(stats.admissions_refused, 0);
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_without_the_storage_secret_is_refused() {
    let host = cw::RpcListener::start(listener_cfg(), SECRET.to_vec(), Arc::new(cw::PingService))
        .expect("listener starts");
    let endpoint = host.endpoint().to_string();
    let err = cw::RpcClient::connect(&endpoint, b"not-the-secret", "impostor", None)
        .await
        .expect_err("no storage membership, no session");
    assert!(
        format!("{err}").contains("refused") || format!("{err}").contains("mac"),
        "the client surfaces the coordinator's reason: {err}"
    );
    // The refusal is counted, and no session was ever admitted.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let s = host.stats();
        if s.admissions_refused >= 1 {
            assert_eq!(s.sessions_admitted, 0);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "refusal not counted: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connections_are_capped_and_handles_pruned() {
    let mut cfg = listener_cfg();
    cfg.max_connections = 2;
    let host = cw::RpcListener::start(cfg, SECRET.to_vec(), Arc::new(cw::PingService))
        .expect("listener starts");
    let endpoint = host.endpoint().to_string();
    let mut held = Vec::new();
    for i in 0..2 {
        held.push(
            cw::RpcClient::connect(&endpoint, SECRET, &format!("peer-{i}"), None)
                .await
                .expect("inside the cap"),
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if cw::RpcClient::connect(&endpoint, SECRET, "over-cap", None)
            .await
            .is_err()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the cap must refuse the third connection"
        );
    }
    assert!(host.stats().connections_refused >= 1);
    drop(held);
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rtt_instrument_reports_a_counted_loopback_floor() {
    // Deliverable beyond code: the RTT number that PRICES S8 (risk R1,
    // ruling D10 — function-shipped metadata takes serial streams from
    // ~9,100 ops/s to 6.7–20 k/s at 50–150 µs RTT). Loopback is the
    // FLOOR: it measures framing + authn + wake, with the fabric term
    // set to ~0. A fabric row needs the venue (see the evidence note).
    let host = cw::RpcListener::start(listener_cfg(), SECRET.to_vec(), Arc::new(cw::PingService))
        .expect("listener starts");
    let report = cw::measure_rtt(&host.endpoint().to_string(), SECRET, "rtt-probe", 64, 0)
        .await
        .expect("the instrument runs");
    assert_eq!(report.samples, 64);
    assert!(
        report.median_us > 0.0 && report.median_us < 5_000.0,
        "a loopback median must be sane: {report:?}"
    );
    assert!(report.min_us <= report.median_us && report.median_us <= report.p99_us);
    assert!(report.p99_us <= report.max_us);
    assert_eq!(
        host.stats().requests_served,
        65,
        "every sample is a real authenticated round trip, not a local loop \
         (64 counted + the deliberately discarded warm-up)"
    );
    host.shutdown();
}

#[test]
fn a_hostile_peer_costs_the_listener_one_bounded_handshake() {
    // The pre-authn budget: a peer that connects and says nothing is
    // dropped at the handshake deadline, and one that lies about a body
    // length spends a chunk, not the cap. Raw sync sockets since the
    // rip-tokio conversion; the 5 s socket read timeout is the TEST
    // budget (a hung read fails loud as WouldBlock), never the contract.
    let mut cfg = listener_cfg();
    cfg.handshake_timeout = Duration::from_millis(200);
    let host = cw::RpcListener::start(cfg, SECRET.to_vec(), Arc::new(cw::PingService))
        .expect("listener starts");
    let endpoint = host.endpoint();

    let mut silent = std::net::TcpStream::connect(endpoint).expect("connect");
    silent
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("test-budget read bound");
    // The coordinator speaks first, then waits — and gives up.
    let _challenge: Option<cw::RpcFrame> =
        cw::read_plain_frame(&mut silent, cw::FrameClass::Handshake.cap(), None)
            .expect("the coordinator issues the challenge first");
    let mut buf = [0u8; 1];
    let closed = silent
        .read(&mut buf)
        .expect("the deadline fires well inside the test budget");
    assert_eq!(closed, 0, "a silent peer is dropped, not parked forever");

    let mut liar = std::net::TcpStream::connect(endpoint).expect("connect");
    liar.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("test-budget read bound");
    let _: Option<cw::RpcFrame> =
        cw::read_plain_frame(&mut liar, cw::FrameClass::Handshake.cap(), None).expect("challenge");
    liar.write_all(&u32::MAX.to_be_bytes())
        .expect("lie about the length");
    let n = liar.read(&mut buf).expect("the class cap refuses promptly");
    assert_eq!(
        n, 0,
        "a pre-authn frame past the hello class cap is dropped"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// Wiring sanity: the connection's own named serve thread is what serves
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_service_calls_execute_on_the_connections_own_named_thread() {
    // §6.7's venue rule on the rip-tokio mechanism: the pinned
    // `ServicePool` is deleted, and the service call now runs ON the
    // admitted connection's own named OS thread (`sqz-clw-conn` —
    // thread-per-connection, bounded by the ConnGate cap the
    // `concurrent_connections_are_capped_and_handles_pruned` leg pins) —
    // never on the caller's tokio runtime (a runtime worker would show
    // up here as `tokio-runtime-worker`) and never on the conveyor's
    // task.
    #[derive(Default)]
    struct VenueService {
        venues: std::sync::Mutex<Vec<(String, std::thread::ThreadId)>>,
    }
    impl cw::RpcService for VenueService {
        fn call(&self, req: cw::RpcRequest) -> cw::RpcResponse {
            let cur = std::thread::current();
            let name = cur.name().unwrap_or("<unnamed>").to_string();
            self.venues
                .lock()
                .expect("venue lock")
                .push((name, cur.id()));
            cw::RpcResponse {
                id: req.id,
                status: 0,
                body: req.body,
            }
        }
    }
    let svc = Arc::new(VenueService::default());
    let host = cw::RpcListener::start(listener_cfg(), SECRET.to_vec(), svc.clone())
        .expect("listener starts");
    let mut client = cw::RpcClient::connect(&host.endpoint().to_string(), SECRET, "venue", None)
        .await
        .expect("enroll");
    for _ in 0..4 {
        client
            .call(cw::VERB_PING, vec![1, 2, 3])
            .await
            .expect("call");
    }
    let venues = svc.venues.lock().expect("venue lock").clone();
    assert_eq!(venues.len(), 4, "every call was served");
    for (name, _) in &venues {
        assert_eq!(
            name, "sqz-clw-conn",
            "an owner-side RPC runs on the connection's own NAMED serve \
             thread (pidstat/perf attribution), never the caller's runtime"
        );
    }
    assert!(
        venues.windows(2).all(|w| w[0].1 == w[1].1),
        "one connection is served by ONE thread (thread-per-connection): {venues:?}"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// The RTT row instrument, on demand
// ---------------------------------------------------------------------------

/// The counted RTT row, run by name (never in the normal suite — a
/// measurement is not a gate):
///
/// ```text
/// # loopback FLOOR (framing + authn + wake; fabric term ~0)
/// cargo test --release --test cluster_wire_tests -- --ignored --nocapture rtt_row
///
/// # a REAL peer: point it at a live coordinator's cluster_wire endpoint
/// SQZ_CLW_RTT_ENDPOINT=10.0.0.2:7100 SQZ_CLW_RTT_SECRET_HEX=<job:enroll secret> \
/// SQZ_CLW_RTT_SAMPLES=5000 cargo test --release --test cluster_wire_tests -- \
///   --ignored --nocapture rtt_row
/// ```
///
/// The secret is the shared volume's `job:enroll` record — i.e. the probe
/// authenticates exactly like any other peer (`squeezefs job worker` reads
/// the same record), which is the point of storage trust.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement, not a gate: run by name (see the doc comment)"]
async fn rtt_row() {
    let samples: usize = std::env::var("SQZ_CLW_RTT_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let payload: usize = std::env::var("SQZ_CLW_RTT_PAYLOAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    match std::env::var("SQZ_CLW_RTT_ENDPOINT").ok() {
        Some(endpoint) => {
            let hex = std::env::var("SQZ_CLW_RTT_SECRET_HEX")
                .expect("a remote row needs SQZ_CLW_RTT_SECRET_HEX (the job:enroll secret)");
            let secret = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex secret"))
                .collect::<Vec<u8>>();
            let report = cw::measure_rtt(&endpoint, &secret, "rtt-probe", samples, payload)
                .await
                .expect("the remote coordinator answered");
            eprintln!("cluster_wire RTT [REMOTE {endpoint}] {report:?}");
        }
        None => {
            let host =
                cw::RpcListener::start(listener_cfg(), SECRET.to_vec(), Arc::new(cw::PingService))
                    .expect("listener starts");
            let report = cw::measure_rtt(
                &host.endpoint().to_string(),
                SECRET,
                "rtt-probe",
                samples,
                payload,
            )
            .await
            .expect("the instrument runs");
            eprintln!(
                "cluster_wire RTT [LOOPBACK FLOOR — not a fabric row] {report:?} \
                 (served {})",
                host.stats().requests_served
            );
            host.shutdown();
        }
    }
}
