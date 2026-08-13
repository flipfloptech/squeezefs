//! VAL-6 (P0, pre-RC) — the §5.1.6 job-wire **untrusted-input bounds**,
//! red-first. This binary carries the pure/decode-side half (the
//! host-side connection, enrollment-freshness, and ladder legs live in
//! `tests/job_wire_tests.rs`, which owns the fabric fixture).
//!
//! The listener is reachable by any process that can route to the mount
//! (execution-plan D2: the user RULED the bind stays configurable and
//! DEFAULT `0.0.0.0` with auto-discovered peers), so every byte before
//! enrollment is attacker-chosen. What is pinned here:
//!
//! - **Frame allocation is chunk-bounded**: `read_frame` must not
//!   `vec![0u8; len]` a 16 MiB length prefix that no body backs. Body
//!   memory is committed only as bytes actually arrive
//!   ([`FRAME_CHUNK_BYTES`] at a time), so a lying length prefix costs
//!   one chunk, not the cap.
//! - **Per-class caps**: pre-enrollment frames ride the small
//!   [`MAX_HELLO_FRAME_BYTES`] class, not the post-enrollment
//!   [`MAX_FRAME_BYTES`] cap.
//! - **Per-class deadlines**: a started frame body that stalls is an
//!   error, not a parked task (the 10 s timeout used to cover the HELLO
//!   frame only).
//! - **Accept-error backoff**: the ladder that replaces the bare
//!   `continue` (an `EMFILE` condition was a busy loop).
//! - **The configuration surface**: `security` was hardwired `None` with
//!   no flag/env/config able to populate `ClusterSecurityConfig` — the
//!   listener recommended a configuration the binary could not express.
//!
//! NOT here (S3 `cluster_wire`): per-frame authentication after
//! enrollment, a session key derived from the storage secret, and the
//! zero-config mutual authn + peer discovery redesign (D2/DISC-1).

use squeezefs::job_wire::{
    next_accept_backoff, read_frame, read_frame_limited, write_frame, JobWireConfig, WireFrame,
    ACCEPT_BACKOFF_MAX, ACCEPT_BACKOFF_START, FRAME_CHUNK_BYTES, MAX_FRAME_BYTES,
    MAX_HELLO_FRAME_BYTES, WIRE_SCHEMA,
};

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write as _;
use std::time::Duration;

// ---------------------------------------------------------------------------
// The allocation instrument: a per-thread PEAK single-allocation gauge.
// Thread-local (const-initialized, so the hook itself never allocates) —
// a parallel test on another thread cannot pollute an armed window.
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

/// Run `f` with the peak-allocation gauge armed; returns `(output, peak)`.
fn peak_alloc<T>(f: impl FnOnce() -> T) -> (T, usize) {
    PEAK.with(|p| p.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, PEAK.with(|p| p.get()))
}

/// A length prefix with no body behind it (the lying-prefix shape).
fn bare_prefix(len: u32) -> Vec<u8> {
    len.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Frame/allocation bounds
// ---------------------------------------------------------------------------

#[test]
fn oversize_length_prefix_refuses_naming_the_cap() {
    let mut cursor = std::io::Cursor::new(bare_prefix(MAX_FRAME_BYTES + 1));
    let err =
        read_frame(&mut cursor).expect_err("a length prefix past the cap is a protocol violation");
    let msg = err.to_string();
    assert!(
        msg.contains(&MAX_FRAME_BYTES.to_string()),
        "the refusal names the cap: {msg}"
    );
}

#[test]
fn lying_length_prefix_allocates_one_chunk_not_the_cap() {
    // The VAL-6 anchor: `vec![0u8; len]` ran BEFORE `read_exact`, so four
    // attacker bytes bought a 16 MiB allocation per connection. Body
    // memory must be committed only as bytes arrive.
    let (res, peak) = peak_alloc(|| {
        let mut cursor = std::io::Cursor::new(bare_prefix(MAX_FRAME_BYTES));
        read_frame(&mut cursor)
    });
    assert!(
        res.is_err(),
        "a body-less 16 MiB claim is EOF-truncated, not a frame"
    );
    assert!(
        peak <= FRAME_CHUNK_BYTES * 2,
        "body allocation must stay chunk-bounded: peak {peak} B for a \
         {MAX_FRAME_BYTES} B claim (chunk {FRAME_CHUNK_BYTES} B)"
    );
}

#[test]
fn partially_delivered_body_allocates_only_what_arrived() {
    // 8 MiB claimed, 1 KiB delivered: the peak stays chunk-bounded and
    // the read fails (truncated), never succeeds on a short body.
    let mut wire = bare_prefix(8 * 1024 * 1024);
    wire.extend(std::iter::repeat_n(b'x', 1024));
    let (res, peak) = peak_alloc(|| {
        let mut cursor = std::io::Cursor::new(wire);
        read_frame(&mut cursor)
    });
    assert!(res.is_err(), "a truncated body must not decode");
    assert!(
        peak <= FRAME_CHUNK_BYTES * 2,
        "truncated body peak {peak} B must stay chunk-bounded"
    );
}

#[test]
fn valid_frames_still_round_trip_through_the_bounded_reader() {
    let frame = WireFrame::Heartbeat {
        worker_id: "w-round-trip".into(),
    };
    let mut buf: Vec<u8> = Vec::new();
    write_frame(&mut buf, &frame).expect("encode");
    let mut cursor = std::io::Cursor::new(buf);
    let back = read_frame(&mut cursor).expect("decode").expect("one frame");
    match back {
        WireFrame::Heartbeat { worker_id } => assert_eq!(worker_id, "w-round-trip"),
        other => panic!("round-trip changed the frame: {other:?}"),
    }
    // Clean EOF at a frame boundary is still `Ok(None)`.
    let mut empty = std::io::Cursor::new(Vec::new());
    assert!(read_frame(&mut empty).expect("clean eof").is_none());
}

#[test]
fn pre_enrollment_class_cap_is_far_below_the_frame_cap() {
    // Post-enrollment frames (ShardAssign/ResultSubmit) may be large;
    // a hello never is. The class cap is what an unauthenticated peer
    // gets to spend.
    const {
        assert!(
            MAX_HELLO_FRAME_BYTES < MAX_FRAME_BYTES / 64,
            "the pre-enrollment class must be far below the post-enrollment cap"
        )
    };
    let mut cursor = std::io::Cursor::new(bare_prefix(MAX_HELLO_FRAME_BYTES + 1));
    let err = read_frame_limited(
        &mut cursor,
        MAX_HELLO_FRAME_BYTES,
        Some(Duration::from_millis(50)),
    )
    .expect_err("a hello-class frame past its class cap refuses");
    let msg = err.to_string();
    assert!(
        msg.contains(&MAX_HELLO_FRAME_BYTES.to_string()),
        "the class refusal names its own cap, not the global one: {msg}"
    );
}

#[test]
fn stalled_frame_body_hits_the_per_class_deadline() {
    // Slowloris: a valid prefix then silence. Before VAL-6 only the HELLO
    // frame had any deadline, and the body read had none at all.
    //
    // Deadlines ride the SOCKET since the rip-tokio conversion: each
    // blocking read is bounded by the caller-set `set_read_timeout`, and
    // the framer's `body_timeout` bounds the WHOLE body between chunks
    // (a dribbling peer is an error, not a parked thread). Socket-timeout
    // expiry surfaces as `WouldBlock` on Linux (`TimedOut` elsewhere) —
    // the product's own `io_timed_out` predicate reads both as a timeout,
    // so this asserts the same class.
    let (mut client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    server
        .set_read_timeout(Some(Duration::from_millis(150)))
        .expect("socket read timeout");
    // 4 KiB claimed; 8 bytes delivered; then the peer just holds.
    let mut head = bare_prefix(4096);
    head.extend_from_slice(b"12345678");
    client.write_all(&head).expect("prefix + dribble");

    let started = std::time::Instant::now();
    let err = read_frame_limited(
        &mut server,
        MAX_HELLO_FRAME_BYTES,
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
        "the deadline fired, not the old unbounded park"
    );
    drop(client);
}

#[test]
fn write_frame_still_refuses_to_emit_past_the_cap() {
    // The encode side's cap is unchanged (kept in the same pass so the
    // two directions cannot drift).
    let huge = WireFrame::Heartbeat {
        worker_id: "x".repeat(MAX_FRAME_BYTES as usize + 64),
    };
    let mut buf: Vec<u8> = Vec::new();
    let err = write_frame(&mut buf, &huge).expect_err("an oversize frame is never emitted");
    assert!(
        err.to_string().contains("MAX_FRAME_BYTES"),
        "the encode refusal names the cap: {err}"
    );
}

// ---------------------------------------------------------------------------
// Accept-error backoff ladder
// ---------------------------------------------------------------------------

#[test]
fn accept_backoff_ladder_doubles_and_caps() {
    // The old arm was a bare `continue`: a persistent accept error
    // (EMFILE/ENFILE) spun the accept loop at 100 % of a core.
    let first = next_accept_backoff(None);
    assert_eq!(first, ACCEPT_BACKOFF_START, "the ladder starts small");
    let mut d = first;
    let mut steps = 0;
    while d < ACCEPT_BACKOFF_MAX {
        let next = next_accept_backoff(Some(d));
        assert!(next > d, "the ladder is monotone: {next:?} after {d:?}");
        assert!(next <= ACCEPT_BACKOFF_MAX, "the ladder is capped: {next:?}");
        d = next;
        steps += 1;
        assert!(steps < 64, "the ladder must reach its cap in a few steps");
    }
    assert_eq!(
        next_accept_backoff(Some(ACCEPT_BACKOFF_MAX)),
        ACCEPT_BACKOFF_MAX,
        "at the cap the ladder is idempotent"
    );
    assert!(
        ACCEPT_BACKOFF_MAX <= Duration::from_secs(5),
        "the cap must stay small enough that a transient EMFILE recovers promptly"
    );
}

// ---------------------------------------------------------------------------
// The configuration surface (VAL-6: `security` was unreachable)
// ---------------------------------------------------------------------------

#[test]
fn from_env_defaults_reproduce_todays_posture_and_bound_the_listener() {
    for k in [
        "SQUEEZEFS_JOB_WIRE_BIND",
        "SQUEEZEFS_JOB_WIRE_CA_CERT",
        "SQUEEZEFS_JOB_WIRE_CA_KEY",
        "SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE",
        "SQUEEZEFS_JOB_WIRE_MAX_CONNS",
        "SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS",
    ] {
        std::env::remove_var(k);
    }
    let cfg = JobWireConfig::from_env(Vec::new()).expect("unset env is today's posture");
    assert!(cfg.enabled, "D2: the listener stays on by default");
    assert_eq!(
        cfg.bind_addr.ip(),
        std::net::IpAddr::from([0, 0, 0, 0]),
        "D2: default bind stays 0.0.0.0 (peers are auto-discovered)"
    );
    assert_eq!(
        cfg.bind_addr.port(),
        0,
        "ephemeral port, published via the \
         mount registration"
    );
    assert!(
        cfg.security.is_none(),
        "unset ⇒ today's plaintext behavior, unchanged"
    );
    assert!(
        cfg.max_connections >= 32,
        "the derived connection cap must admit a real worker population: {}",
        cfg.max_connections
    );
    assert!(
        cfg.max_connections <= 4096,
        "…and must still be a bound: {}",
        cfg.max_connections
    );
    assert!(cfg.enroll_freshness > Duration::ZERO);
    assert!(cfg.handshake_timeout > Duration::ZERO);
    assert!(cfg.frame_body_timeout > Duration::ZERO);
}

#[test]
fn from_env_expresses_bind_caps_and_a_ca_pinned_channel() {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};

    let dir = tempfile::tempdir().expect("tempdir");
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SqueezeFS Cluster CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

    let cert_path = dir.path().join("ca.der");
    let key_path = dir.path().join("ca-key.pem");
    std::fs::write(&cert_path, ca_cert.der()).expect("write ca der");
    std::fs::write(&key_path, ca_key.serialize_pem()).expect("write ca key pem");

    std::env::set_var("SQUEEZEFS_JOB_WIRE_BIND", "127.0.0.1:0");
    std::env::set_var("SQUEEZEFS_JOB_WIRE_CA_CERT", &cert_path);
    std::env::set_var("SQUEEZEFS_JOB_WIRE_CA_KEY", &key_path);
    std::env::set_var("SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE", "250");
    std::env::set_var("SQUEEZEFS_JOB_WIRE_MAX_CONNS", "64");
    std::env::set_var("SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS", "5000");

    let cfg = JobWireConfig::from_env(Vec::new()).expect("a CA pair is expressible");
    assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:0");
    assert_eq!(cfg.max_connections, 64, "absolute override wins verbatim");
    assert_eq!(cfg.verify_sample_permille, 250);
    assert_eq!(cfg.enroll_freshness, Duration::from_millis(5000));
    let sec = cfg
        .security
        .as_ref()
        .expect("CA pair populates the security config");
    assert!(sec.ca_cert.is_some() && sec.ca_key.is_some());
    assert!(
        squeezefs::job_wire::channel_authenticated(sec),
        "a CA-pinned pair IS the authenticated channel the ladder keys on"
    );
    // PEM and DER are both accepted (the key above was PEM, the cert DER).
    assert_eq!(
        sec.ca_cert.as_deref(),
        Some(ca_cert.der().as_ref()),
        "a DER CA file lands verbatim"
    );

    // A CA cert with no key is REFUSED loud — the cluster machinery
    // cannot sign a node cert without it (today it `unwrap()`s).
    std::env::remove_var("SQUEEZEFS_JOB_WIRE_CA_KEY");
    let err = JobWireConfig::from_env(Vec::new())
        .expect_err("cert-without-key must refuse, never panic downstream");
    assert!(
        format!("{err}").contains("SQUEEZEFS_JOB_WIRE_CA_KEY"),
        "the refusal names the missing knob: {err}"
    );

    // Listener disable is expressible (VAL-6: there was no way).
    std::env::set_var("SQUEEZEFS_JOB_WIRE_CA_KEY", &key_path);
    std::env::set_var("SQUEEZEFS_JOB_WIRE_BIND", "off");
    let cfg = JobWireConfig::from_env(Vec::new()).expect("off parses");
    assert!(!cfg.enabled, "`off` disables the listener");

    // A garbage bind refuses loud rather than silently falling back to
    // the wide-open default.
    std::env::set_var("SQUEEZEFS_JOB_WIRE_BIND", "not-an-address");
    let err = JobWireConfig::from_env(Vec::new()).expect_err("garbage bind refuses");
    assert!(
        format!("{err}").contains("SQUEEZEFS_JOB_WIRE_BIND"),
        "the refusal names the knob: {err}"
    );

    for k in [
        "SQUEEZEFS_JOB_WIRE_BIND",
        "SQUEEZEFS_JOB_WIRE_CA_CERT",
        "SQUEEZEFS_JOB_WIRE_CA_KEY",
        "SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE",
        "SQUEEZEFS_JOB_WIRE_MAX_CONNS",
        "SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS",
    ] {
        std::env::remove_var(k);
    }
}

#[test]
fn wire_schema_advanced_for_the_challenge_handshake() {
    // The coordinator-issued challenge is a protocol change: a v1 worker
    // (worker-chosen nonce, no registry) must refuse loud, not silently
    // enroll on a replayable proof.
    const {
        assert!(
            WIRE_SCHEMA >= 2,
            "the challenge handshake carries its own schema version"
        )
    };
}
