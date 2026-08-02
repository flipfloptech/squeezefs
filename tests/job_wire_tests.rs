//! PR VL2b — the §5.1.6 remote-worker job-shard execution wire, red-first
//! (docs/design-volume-lifecycle.md §5.1.6, KD-15, gate G-VL-7 remote legs):
//!
//! - **Enrollment security (storage-trust)**: the coordinator writes a
//!   random session secret into the `job:enroll` meta-KV record (behind
//!   the VL2 reserved-xattr screen); a worker proves storage membership
//!   with `HMAC-SHA256(secret, worker_id ‖ endpoint_nonce ‖ "hello")`.
//!   Good HMAC admitted, bad HMAC refused (`job_remote_enroll_refused`),
//!   wrong `wire_schema` refused.
//! - **Transport-coupled verification (Issue-30 law)**: plaintext ⇒
//!   mandatory-100 % verify-reads before any mutating publish (the
//!   configured sample is overridden); TLS (tokio-rustls over the
//!   `ClusterSecurityConfig` cert/CA/verifier machinery) keeps sampling.
//! - **End-to-end Noop shard**: enroll → ShardAssign → heartbeats →
//!   ResultSubmit → ResultAck; the job completes through the fabric;
//!   `job_remote_shards`/`job_remote_submissions` account for it.
//! - **Lease expiry + fencing**: a worker that stops heartbeating past
//!   the TTL loses the lease — fencing bumps, the shard requeues to the
//!   local pool, and the old holder's late ResultSubmit is refused
//!   (`job_remote_refused_stale`).
//! - **Fresh-destination law (KD-15)**: a reassigned shard's
//!   destinations are freshly allocated; the expired lease's tuples
//!   enter the do-not-publish quarantine set
//!   (`job_remote_quarantined_destinations`).
//! - **WERO fence (rung 2)**: `acquire_write_exclusive_registrants_only`
//!   (rtype 2) beside D0's rtype-1 acquire — registrants write,
//!   unregistered hosts are blocked, a preempted key's host is
//!   device-rejected while other registrants proceed; the coordinator
//!   holds WERO from first enrollment to last departure and preempts an
//!   expired worker host's registration (`job_remote_pr_preempts`).
//!
//! VAL-6 (P0, pre-RC) adds the host-side hardening legs: the
//! coordinator-issued enrollment challenge (single-use nonce +
//! freshness window — the worker used to pick its own nonce with no
//! registry), the concurrent-connection cap, connection-handle pruning
//! (RES-5), the pre-enrollment frame class cap + handshake deadline,
//! and the verification-strength ladder keyed on an AUTHENTICATED
//! channel (CA-pinned mTLS) instead of `transport == "tls"`. The
//! decode-side bounds live in `tests/job_wire_bounds_tests.rs`.
//!
//! NOT here (G-VL-7 rig rows, VL3+ — the 2-node devsub mount rig; do not
//! fake them in cargo): remote-worker **kill-9 mid-shard ⇒ TTL ⇒ reclaim
//! ⇒ converge ×10**, the **SIGSTOP-past-TTL / reassign / SIGCONT
//! live-zombie soak** (within-job and post-job-end-on-PR variants), and
//! offline-coordinator remote serving (§5.8).

use squeezefs::fuse_client::METRICS;
use squeezefs::job_wire::{
    read_enroll_secret, read_frame, write_frame, FakeShardDevice, JobWireConfig, JobWireHost,
    JobWireWorker, WireFrame, WorkerOptions, JOB_ENROLL_XATTR, WIRE_SCHEMA,
};
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType};
use squeezefs::meta_backend::reservation::{
    clear_override, install_override, FakeNvmeNamespace, FakeReservationClient, ReservationClient,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

const ROOT: u64 = 1;

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(b"{\"name\":\"jobwire\"}".to_vec()),
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

/// Meta-only fixture: the wire needs the fabric + meta backend, not the
/// full FUSE engine.
async fn meta_fixture() -> (Arc<RoutedMetaBackend>, NamedTempFile) {
    let meta_file = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(meta_file.path(), 256 * 1024 * 1024).await;
    (Arc::new(RoutedMetaBackend::new(vec![kv])), meta_file)
}

/// A fabric with `workers` local pool tasks (0 = no local pool — the
/// remote-only test posture).
async fn fabric(meta: &Arc<RoutedMetaBackend>, workers: usize) -> Arc<JobFabric> {
    JobFabric::start(meta.clone(), workers, 100, None)
        .await
        .expect("fabric start")
}

/// Plaintext wire config on an ephemeral localhost port with
/// test-friendly lease/heartbeat cadence.
fn wire_cfg(ttl_ms: u64, hb_ms: u64) -> JobWireConfig {
    JobWireConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        lease_ttl: Duration::from_millis(ttl_ms),
        heartbeat_interval: Duration::from_millis(hb_ms),
        ..JobWireConfig::default()
    }
}

fn noop_spec(tasks: u64, task_ms: u64) -> JobSpec {
    JobSpec {
        job_type: JobType::Noop { tasks, task_ms },
        throttle_pct: 100,
    }
}

/// Read the coordinator-issued enrollment challenge off a freshly
/// accepted connection (VAL-6: the coordinator speaks first now).
async fn challenge_nonce<S>(stream: &mut S) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match read_frame(stream)
        .await
        .expect("challenge readable")
        .expect("the coordinator issues the challenge first")
    {
        WireFrame::Challenge {
            wire_schema,
            server_nonce,
            freshness_ms,
        } => {
            assert_eq!(wire_schema, WIRE_SCHEMA, "challenge carries the schema");
            assert!(freshness_ms > 0, "challenge advertises its own window");
            assert!(!server_nonce.is_empty(), "challenge carries a nonce");
            server_nonce
        }
        other => panic!("expected Challenge first, got {other:?}"),
    }
}

/// Poll `f` until true or panic at the deadline.
async fn poll_until(what: &str, deadline: Duration, mut f: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    loop {
        if f() {
            return;
        }
        assert!(
            start.elapsed() < deadline,
            "timed out after {deadline:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// WERO (rtype 2) — the reservation.rs extension at the fake seam
// ---------------------------------------------------------------------------

#[test]
fn wero_registrants_only_fake_semantics() {
    // KD-15 rung 2: Write Exclusive – Registrants Only. Registrants
    // write; unregistered hosts are blocked; preempting a key rejects
    // exactly that host's writes while other registrants proceed.
    let ns = FakeNvmeNamespace::new();
    let coord = FakeReservationClient::new(ns.clone(), "nqn-coord", "host-coord");
    let wa = FakeReservationClient::new(ns.clone(), "nqn-a", "host-a");
    let wb = FakeReservationClient::new(ns.clone(), "nqn-b", "host-b");

    coord.register(0xC0).expect("coordinator register");
    coord
        .acquire_write_exclusive_registrants_only(0xC0)
        .expect("WERO acquire");
    assert_eq!(ns.holder(), Some(0xC0));

    wa.register(0xA0).expect("worker A register");
    wb.register(0xB0).expect("worker B register");

    // Registrants (holder + workers) write; a non-registered host is
    // write-blocked while the reservation stands — the documented
    // operational side effect (design §5.1.6 consequence (a)).
    assert!(ns.write_allowed(b"host-coord"), "holder writes");
    assert!(
        ns.write_allowed(b"host-a"),
        "registrant A writes under WERO"
    );
    assert!(
        ns.write_allowed(b"host-b"),
        "registrant B writes under WERO"
    );
    assert!(
        !ns.write_allowed(b"host-stranger"),
        "unregistered host is write-blocked under WERO"
    );

    // Preempt A's registration: A's DMA is device-rejected, B keeps
    // writing, the reservation stands.
    coord
        .preempt_registrants_only(0xC0, 0xA0)
        .expect("preempt expired worker registration");
    assert!(!ns.is_registered(0xA0), "victim registration removed");
    assert!(
        !ns.write_allowed(b"host-a"),
        "preempted key's host writes are rejected"
    );
    assert!(
        ns.write_allowed(b"host-b"),
        "other registrants proceed after the preempt"
    );
    assert_eq!(ns.holder(), Some(0xC0), "WERO reservation stands");
    assert_eq!(ns.preempt_count(), 1);

    // Last departure: release drops the reservation (and the coordinator
    // registration — zero residue), opening writes again.
    coord
        .release_registrants_only(0xC0)
        .expect("WERO release at last departure");
    assert_eq!(ns.holder(), None);
    assert!(
        ns.write_allowed(b"host-stranger"),
        "no reservation → writes open"
    );

    // Contrast with D0's rtype 1 (untouched semantics): only the holder
    // host writes — a registered non-holder is still blocked.
    let ns2 = FakeNvmeNamespace::new();
    let c2 = FakeReservationClient::new(ns2.clone(), "nqn-c2", "host-c2");
    let w2 = FakeReservationClient::new(ns2.clone(), "nqn-w2", "host-w2");
    c2.register(0xC2).unwrap();
    w2.register(0xD2).unwrap();
    c2.acquire_write_exclusive(0xC2).unwrap();
    assert!(ns2.write_allowed(b"host-c2"), "WE holder writes");
    assert!(
        !ns2.write_allowed(b"host-w2"),
        "WE (rtype 1) blocks even registered non-holders"
    );
}

// ---------------------------------------------------------------------------
// Enrollment: HMAC gate, schema refusal, plaintext posture
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_hmac_gate_and_schema_refusal() {
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 10_000),
        FakeShardDevice::new(0, 0),
    )
    .await
    .expect("host start");

    // The coordinator wrote the session secret into the job:enroll
    // record (reserved namespace — the storage-membership credential).
    let secret = read_enroll_secret(&meta)
        .await
        .expect("job:enroll record present");
    assert!(!secret.is_empty());
    let raw = meta
        .getxattr(ROOT, JOB_ENROLL_XATTR)
        .await
        .expect("backend read")
        .expect("job:enroll persisted on ino 1");
    assert!(!raw.is_empty());

    let enrollments = METRICS.job_remote_enrollments.load(Ordering::Relaxed);
    let refused = METRICS.job_remote_enroll_refused.load(Ordering::Relaxed);

    // Good HMAC: admitted.
    let endpoint = host.endpoint().to_string();
    let w = JobWireWorker::connect(&endpoint, &secret, WorkerOptions::new("w-good"))
        .await
        .expect("good HMAC must enroll");
    assert_eq!(
        METRICS.job_remote_enrollments.load(Ordering::Relaxed),
        enrollments + 1
    );
    drop(w);

    // Bad HMAC (wrong secret): refused, counted.
    let err = JobWireWorker::connect(&endpoint, b"not-the-secret", WorkerOptions::new("w-bad"))
        .await
        .expect_err("bad HMAC must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("refused") || msg.contains("hmac"),
        "refusal names the cause: {msg}"
    );
    poll_until("enroll_refused counted", Duration::from_secs(5), || {
        METRICS.job_remote_enroll_refused.load(Ordering::Relaxed) == refused + 1
    })
    .await;

    // Wrong wire_schema: refused loudly even with a valid HMAC — the
    // frame layer is schema-versioned (the challenge handshake bumped it).
    let mut raw_conn = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("raw connect");
    let server_nonce = challenge_nonce(&mut raw_conn).await;
    let hmac = squeezefs::job_wire::enroll_hmac(&secret, "w-schema", &server_nonce, "nonce-1");
    write_frame(
        &mut raw_conn,
        &WireFrame::Enroll {
            wire_schema: WIRE_SCHEMA + 1,
            worker_id: "w-schema".into(),
            server_nonce,
            endpoint_nonce: "nonce-1".into(),
            hmac,
            pr_key: None,
        },
    )
    .await
    .expect("send future-schema enroll");
    let reply = read_frame(&mut raw_conn)
        .await
        .expect("read reply")
        .expect("reply frame");
    match reply {
        WireFrame::EnrollRefused { reason } => {
            assert!(
                reason.contains("wire_schema"),
                "schema refusal names the field: {reason}"
            );
        }
        other => panic!("future wire_schema must refuse, got {other:?}"),
    }

    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plaintext_transport_forces_mandatory_verify_reads() {
    // OQ-A default-permissive plaintext + the Issue-30 law: without
    // ClusterSecurityConfig the listener runs plaintext (ONE loud log
    // line at start) and verify-reads are mandatory-100 % — a configured
    // sample rate is overridden, never honored.
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.verify_sample_permille = 100; // ask for 10 % sampling…
    assert!(cfg.security.is_none(), "default config is plaintext");
    let host = JobWireHost::start(fab, cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");
    assert_eq!(host.transport_mode(), "plaintext");
    assert_eq!(
        host.verify_permille(),
        1000,
        "Issue-30: plaintext transport ⇒ mandatory-100 % verify-reads"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_transport_round_trip_keeps_sampling() {
    // TLS via tokio-rustls over the ClusterSecurityConfig cert/CA/
    // verifier machinery (`tiering::cluster_tls`): an mTLS-pinned worker
    // enrolls and executes a Noop shard end-to-end; the configured
    // verify sample is honored (sampling is sanctioned under TLS).
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SqueezeFS Cluster CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");
    let security = squeezefs::tiering::cluster_tls::ClusterSecurityConfig {
        ca_cert: Some(ca_cert.der().to_vec()),
        ca_key: Some(ca_key.serialize_der()),
    };

    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 500);
    cfg.security = Some(security.clone());
    cfg.verify_sample_permille = 250;
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");
    assert_eq!(host.transport_mode(), "mtls");
    assert!(
        host.channel_authenticated(),
        "a CA-pinned pair IS the authenticated channel"
    );
    assert_eq!(
        host.verify_permille(),
        250,
        "CA-pinned mTLS keeps the configured sample"
    );

    let secret = read_enroll_secret(&meta).await.expect("secret");
    let mut opts = WorkerOptions::new("w-tls");
    opts.security = Some(security);
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts)
        .await
        .expect("TLS enroll");
    let run = tokio::spawn(worker.run(FakeShardDevice::new(0, 0)));

    let job_id = fab.submit(noop_spec(8, 1)).await.expect("submit");
    let state = fab
        .wait_terminal(&job_id, Duration::from_secs(30))
        .await
        .expect("terminal over TLS");
    assert_eq!(state, JobState::Completed);

    host.shutdown().await;
    let report = run.await.expect("worker task").expect("worker run");
    assert_eq!(report.shards_completed, 1);
}

// ---------------------------------------------------------------------------
// End-to-end Noop shard over the wire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_worker_executes_noop_shard_end_to_end() {
    let (meta, _mf) = meta_fixture().await;
    // Zero local workers: the remote worker is the only executor.
    let fab = fabric(&meta, 0).await;
    let seam = FakeShardDevice::new(0, 0);
    let host = JobWireHost::start(fab.clone(), wire_cfg(30_000, 500), seam.clone())
        .await
        .expect("host start");

    let shards0 = METRICS.job_remote_shards.load(Ordering::Relaxed);
    let subs0 = METRICS.job_remote_submissions.load(Ordering::Relaxed);

    let secret = read_enroll_secret(&meta).await.expect("secret");
    let worker = JobWireWorker::connect(
        &host.endpoint().to_string(),
        &secret,
        WorkerOptions::new("w-e2e"),
    )
    .await
    .expect("enroll");
    let run = tokio::spawn(worker.run(seam));

    let job_id = fab.submit(noop_spec(16, 1)).await.expect("submit");
    let state = fab
        .wait_terminal(&job_id, Duration::from_secs(30))
        .await
        .expect("remote execution completes the job");
    assert_eq!(state, JobState::Completed);
    let status = fab.status(&job_id).await.unwrap().expect("known");
    assert_eq!(status.tasks_done, 16);

    // Engagement instrument (§10): the remote share is accounted.
    assert_eq!(
        METRICS.job_remote_shards.load(Ordering::Relaxed),
        shards0 + 1,
        "job_remote_shards accounts the shard"
    );
    assert_eq!(
        METRICS.job_remote_submissions.load(Ordering::Relaxed),
        subs0 + 1,
        "job_remote_submissions accounts the verified submission"
    );

    // The durable shard record (job:{id}:shard:0 — KD-2 shape) reached
    // its terminal state with the original fencing.
    let raw = meta
        .getxattr(ROOT, &format!("job:{job_id}:shard:0"))
        .await
        .expect("backend read")
        .expect("shard record persisted");
    let rec: serde_json::Value = serde_json::from_slice(&raw).expect("shard record is JSON");
    assert_eq!(rec["schema"], 1);
    assert_eq!(rec["state"], "completed", "shard record terminal: {rec}");
    assert_eq!(
        rec["shard_fencing"], 0,
        "no expiry ⇒ no fencing bump: {rec}"
    );

    host.shutdown().await;
    let report = run.await.expect("worker task").expect("worker run");
    assert_eq!(report.shards_completed, 1);
    assert_eq!(report.submissions_refused, 0);
}

// ---------------------------------------------------------------------------
// Lease expiry: fencing bump, reassignment to the local pool, stale refusal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_expiry_reassigns_to_local_pool_and_refuses_late_submit() {
    let (meta, _mf) = meta_fixture().await;
    // One local worker (kept busy so the remote deterministically gets
    // the shard) — after expiry it is the reassignment target.
    let fab = fabric(&meta, 1).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(1_000, 200),
        FakeShardDevice::new(0, 0),
    )
    .await
    .expect("host start");

    // Occupy the local pool.
    let busy = fab.submit(noop_spec(1_000_000, 5)).await.expect("busy job");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if fab.status(&busy).await.unwrap().unwrap().state == JobState::Running {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "local worker never claimed the busy job"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let expiries0 = METRICS.job_remote_lease_expiries.load(Ordering::Relaxed);
    let reassigns0 = METRICS.job_remote_reassignments.load(Ordering::Relaxed);
    let stale0 = METRICS.job_remote_refused_stale.load(Ordering::Relaxed);
    let shards0 = METRICS.job_remote_shards.load(Ordering::Relaxed);

    // The zombie model: enrolls, executes, then goes silent — no
    // heartbeats (partition/pause) and its submission parked (the
    // pause-before-submit window).
    let secret = read_enroll_secret(&meta).await.expect("secret");
    let opts = WorkerOptions::new("w-zombie");
    opts.heartbeats.store(false, Ordering::SeqCst);
    opts.hold_submission.store(true, Ordering::SeqCst);
    let hold = opts.hold_submission.clone();
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts)
        .await
        .expect("enroll");
    let run = tokio::spawn(worker.run(FakeShardDevice::new(0, 0)));

    // Tiny shard: executed well within the TTL, then parked pre-submit.
    let job_id = fab.submit(noop_spec(4, 1)).await.expect("submit");
    poll_until(
        "shard assigned to the remote",
        Duration::from_secs(10),
        || METRICS.job_remote_shards.load(Ordering::Relaxed) == shards0 + 1,
    )
    .await;

    // TTL 1 s, no heartbeats ⇒ the lease expires: fencing bumps and the
    // shard requeues.
    poll_until("lease expiry", Duration::from_secs(10), || {
        METRICS.job_remote_lease_expiries.load(Ordering::Relaxed) == expiries0 + 1
    })
    .await;
    assert_eq!(
        METRICS.job_remote_reassignments.load(Ordering::Relaxed),
        reassigns0 + 1,
        "expired shard reassigned"
    );

    // The durable shard record carries the bumped fencing.
    let raw = meta
        .getxattr(ROOT, &format!("job:{job_id}:shard:0"))
        .await
        .unwrap()
        .expect("shard record");
    let rec: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(
        rec["shard_fencing"].as_u64().expect("fencing") >= 1,
        "expiry bumps shard_fencing: {rec}"
    );

    // Free the local pool: it picks the requeued job up and completes it
    // (reassignment to the LOCAL population — one protocol, two
    // transports).
    fab.cancel(&busy).await.expect("cancel busy");
    let state = fab
        .wait_terminal(&job_id, Duration::from_secs(30))
        .await
        .expect("local pool completes the reassigned job");
    assert_eq!(state, JobState::Completed);

    // The zombie wakes and submits late: refused by the fencing check.
    hold.store(false, Ordering::SeqCst);
    poll_until("late submission refused", Duration::from_secs(10), || {
        METRICS.job_remote_refused_stale.load(Ordering::Relaxed) == stale0 + 1
    })
    .await;

    host.shutdown().await;
    let report = run.await.expect("worker task").expect("worker run");
    assert_eq!(report.shards_completed, 0, "the zombie's work never lands");
    assert_eq!(report.submissions_refused, 1, "late submit refused");
}

// ---------------------------------------------------------------------------
// Fresh-destination law + quarantine + plaintext 100 % verify-reads
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reassigned_shard_gets_fresh_destinations_and_expired_ones_quarantined() {
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    // Mutating vehicle at the fake-device seam: every shard "moves"
    // 2 blocks of 512 B (VL4 plugs the real movers into this seam).
    const BLOCKS: usize = 2;
    const BLOCK_LEN: usize = 512;
    let seam = FakeShardDevice::new(BLOCKS, BLOCK_LEN);
    let host = JobWireHost::start(fab.clone(), wire_cfg(800, 200), seam.clone())
        .await
        .expect("host start");
    assert_eq!(host.transport_mode(), "plaintext");

    let quarantined0 = METRICS
        .job_remote_quarantined_destinations
        .load(Ordering::Relaxed);
    let verify0 = METRICS.job_remote_verify_read_bytes.load(Ordering::Relaxed);
    let moved0 = METRICS.job_remote_bytes_moved.load(Ordering::Relaxed);
    let expiries0 = METRICS.job_remote_lease_expiries.load(Ordering::Relaxed);

    let secret = read_enroll_secret(&meta).await.expect("secret");

    // Worker 1: the live zombie (no heartbeats, submission parked).
    let opts1 = WorkerOptions::new("w-old");
    opts1.heartbeats.store(false, Ordering::SeqCst);
    opts1.hold_submission.store(true, Ordering::SeqCst);
    let w1 = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts1)
        .await
        .expect("enroll w1");
    let run1 = tokio::spawn(w1.run(seam.clone()));

    let job_id = fab.submit(noop_spec(2, 1)).await.expect("submit");

    // First assignment allocated destinations.
    poll_until("first allocation", Duration::from_secs(10), || {
        seam.allocations().len() == 1
    })
    .await;
    let first = seam.allocations()[0].clone();
    assert_eq!(first.len(), BLOCKS);

    // Expiry: the OLD destinations enter the do-not-publish quarantine.
    poll_until("lease expiry", Duration::from_secs(10), || {
        METRICS.job_remote_lease_expiries.load(Ordering::Relaxed) == expiries0 + 1
    })
    .await;
    let mut quarantined = host.quarantined_destinations();
    quarantined.sort();
    let mut expected = first.clone();
    expected.sort();
    assert_eq!(
        quarantined, expected,
        "expired lease's destinations are quarantined do-not-publish"
    );
    assert_eq!(
        METRICS
            .job_remote_quarantined_destinations
            .load(Ordering::Relaxed),
        quarantined0 + BLOCKS as u64,
        "quarantine gauge accounts the tuples"
    );

    // Worker 2 (healthy) gets the reassigned shard — with FRESHLY
    // allocated destinations, never the expired lease's.
    let w2 = JobWireWorker::connect(
        &host.endpoint().to_string(),
        &secret,
        WorkerOptions::new("w-new"),
    )
    .await
    .expect("enroll w2");
    let run2 = tokio::spawn(w2.run(seam.clone()));

    poll_until("fresh allocation", Duration::from_secs(10), || {
        seam.allocations().len() == 2
    })
    .await;
    let second = seam.allocations()[1].clone();
    assert_eq!(second.len(), BLOCKS);
    for d in &second {
        assert!(
            !first.contains(d),
            "fresh-destination law: reassigned shard must not reuse {d:?}"
        );
    }

    let state = fab
        .wait_terminal(&job_id, Duration::from_secs(30))
        .await
        .expect("reassigned shard completes");
    assert_eq!(state, JobState::Completed);

    // Issue-30 on plaintext: EVERY destination byte was verify-read
    // before publish (exactly one verified submission: worker 2's).
    assert_eq!(
        METRICS.job_remote_verify_read_bytes.load(Ordering::Relaxed),
        verify0 + (BLOCKS * BLOCK_LEN) as u64,
        "plaintext ⇒ 100 % verify-reads on the verified submission"
    );
    assert_eq!(
        METRICS.job_remote_bytes_moved.load(Ordering::Relaxed),
        moved0 + (BLOCKS * BLOCK_LEN) as u64
    );

    host.shutdown().await;
    run1.abort();
    let _ = run1.await;
    let report2 = run2.await.expect("worker 2 task").expect("worker 2 run");
    assert_eq!(report2.shards_completed, 1);
}

// ---------------------------------------------------------------------------
// WERO fence over the wire: first-enrollment acquire, expiry preempt,
// last-departure release
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wero_fence_acquires_preempts_and_releases_over_the_wire() {
    let data_path = std::path::PathBuf::from(format!(
        "/tmp/squeezefs-vl2b-fake-data-{}",
        std::process::id()
    ));
    let ns = FakeNvmeNamespace::new();
    let coord_client = FakeReservationClient::new(ns.clone(), "nqn-coord", "host-coord");
    install_override(&data_path, coord_client);

    // The worker HOST registered its per-host PR key on the shared data
    // namespace at enrollment time (its own association — the
    // coordinator can only preempt it, never register it).
    let worker_client = FakeReservationClient::new(ns.clone(), "nqn-w1", "host-w1");
    worker_client.register(0xA0).expect("worker host register");

    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(800, 200);
    cfg.data_device_paths = vec![data_path.clone()];
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");

    // No remote workers yet: no WERO hold.
    assert_eq!(ns.holder(), None, "WERO waits for the first enrollment");
    assert_eq!(host.fence_mode(), "deferred-reclaim");

    let preempts0 = METRICS.job_remote_pr_preempts.load(Ordering::Relaxed);
    let expiries0 = METRICS.job_remote_lease_expiries.load(Ordering::Relaxed);

    let secret = read_enroll_secret(&meta).await.expect("secret");
    let mut opts = WorkerOptions::new("w-fenced");
    opts.heartbeats.store(false, Ordering::SeqCst);
    opts.hold_submission.store(true, Ordering::SeqCst);
    opts.pr_key = Some(0xA0);
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts)
        .await
        .expect("enroll");
    let run = tokio::spawn(worker.run(FakeShardDevice::new(0, 0)));

    // First enrollment ⇒ the coordinator takes WERO on the data
    // namespace; guarantee class flips to "pr".
    poll_until("WERO acquired", Duration::from_secs(10), || {
        ns.holder().is_some()
    })
    .await;
    assert_eq!(host.fence_mode(), "pr");
    assert_eq!(METRICS.job_remote_fence_mode.load(Ordering::Relaxed), 1);
    assert!(
        ns.write_allowed(b"host-w1"),
        "registered worker host writes under WERO"
    );

    // Assign a shard, let the lease expire: the coordinator preempts the
    // expired worker HOST's registration — its resumed DMA is
    // device-rejected while the coordinator keeps writing.
    let _job_id = fab.submit(noop_spec(2, 1)).await.expect("submit");
    poll_until("lease expiry", Duration::from_secs(10), || {
        METRICS.job_remote_lease_expiries.load(Ordering::Relaxed) == expiries0 + 1
    })
    .await;
    poll_until(
        "PR preempt of the expired host",
        Duration::from_secs(10),
        || METRICS.job_remote_pr_preempts.load(Ordering::Relaxed) == preempts0 + 1,
    )
    .await;
    assert!(
        !ns.is_registered(0xA0),
        "expired host's registration preempted"
    );
    assert!(
        !ns.write_allowed(b"host-w1"),
        "the preempted host's DMA is device-rejected"
    );
    assert!(
        ns.write_allowed(b"host-coord"),
        "the coordinator keeps writing"
    );

    // Last departure (the worker's connection drops) ⇒ WERO released,
    // zero residue.
    run.abort();
    let _ = run.await;
    poll_until(
        "WERO released at last departure",
        Duration::from_secs(10),
        || ns.holder().is_none(),
    )
    .await;

    host.shutdown().await;
    clear_override(&data_path);
}

// ---------------------------------------------------------------------------
// VAL-6 — the verification-strength ladder keys on an AUTHENTICATED channel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthenticated_tls_is_plaintext_class_for_the_ladder() {
    // With `ca_cert: None` the client installs an accept-everything
    // certificate verifier via `.dangerous()` and the server uses
    // `with_no_client_auth()` — a TLS OBJECT, not an authenticated
    // channel. That configuration used to be permitted to sample
    // verify-reads below 100 % because the ladder keyed on
    // `transport == "tls"`. It is plaintext-class now.
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.security = Some(squeezefs::tiering::cluster_tls::ClusterSecurityConfig::default());
    cfg.verify_sample_permille = 100; // ask for 10 % sampling…
    let host = JobWireHost::start(fab, cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");
    assert_eq!(
        host.transport_mode(),
        "tls-unauthenticated",
        "a CA-less TLS object is named for what it is"
    );
    assert!(
        !host.channel_authenticated(),
        "no CA pin ⇒ no authenticated channel"
    );
    assert_eq!(
        host.verify_permille(),
        1000,
        "the ladder refuses to sample on an unauthenticated channel \
         (plaintext-class ⇒ mandatory-100 % verify-reads)"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ca_cert_without_key_refuses_the_listener() {
    // `rustls_{server,client}_config` `unwrap()` the CA key whenever a CA
    // cert is present: a half-configured security config used to be a
    // panic waiting for the first connection. Refuse loud at start.
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SqueezeFS Cluster CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.security = Some(squeezefs::tiering::cluster_tls::ClusterSecurityConfig {
        ca_cert: Some(ca_cert.der().to_vec()),
        ca_key: None,
    });
    let err = JobWireHost::start(fab, cfg, FakeShardDevice::new(0, 0))
        .await
        .expect_err("a CA cert with no key must refuse the listener");
    let msg = format!("{err}");
    assert!(
        msg.contains("ca_key") || msg.contains("CA key"),
        "the refusal names the missing half: {msg}"
    );
}

// ---------------------------------------------------------------------------
// VAL-6 — enrollment freshness: coordinator-issued nonce, single use
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_challenge_nonce_is_single_use() {
    // The proof used to be HMAC(secret, worker_id ‖ WORKER-CHOSEN nonce ‖
    // "hello") with no registry: one captured hello enrolled forever.
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 10_000),
        FakeShardDevice::new(0, 0),
    )
    .await
    .expect("host start");
    let secret = read_enroll_secret(&meta).await.expect("secret");

    // Capture a legitimate hello…
    let mut conn = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let nonce = challenge_nonce(&mut conn).await;
    let hello = WireFrame::Enroll {
        wire_schema: WIRE_SCHEMA,
        worker_id: "w-replay".into(),
        server_nonce: nonce.clone(),
        endpoint_nonce: "worker-entropy".into(),
        hmac: squeezefs::job_wire::enroll_hmac(&secret, "w-replay", &nonce, "worker-entropy"),
        pr_key: None,
    };
    write_frame(&mut conn, &hello).await.expect("send hello");
    match read_frame(&mut conn).await.expect("reply").expect("frame") {
        WireFrame::EnrollOk { .. } => {}
        other => panic!("a fresh challenge must admit: {other:?}"),
    }
    assert_eq!(
        host.outstanding_challenges(),
        0,
        "an answered challenge leaves no registry residue (single use)"
    );

    // …and replay it verbatim on a second connection.
    let refused0 = METRICS.job_remote_enroll_refused.load(Ordering::Relaxed);
    let mut replay = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let _fresh = challenge_nonce(&mut replay).await; // discarded on purpose
    write_frame(&mut replay, &hello)
        .await
        .expect("send the replayed hello");
    match read_frame(&mut replay)
        .await
        .expect("reply")
        .expect("frame")
    {
        WireFrame::EnrollRefused { reason } => {
            assert!(
                reason.contains("nonce"),
                "the replay refusal names the nonce: {reason}"
            );
        }
        other => panic!("a replayed hello must be refused, got {other:?}"),
    }
    poll_until(
        "replay counted as an enroll refusal",
        Duration::from_secs(5),
        || METRICS.job_remote_enroll_refused.load(Ordering::Relaxed) > refused0,
    )
    .await;

    drop(conn);
    drop(replay);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_enrollment_challenge_is_refused() {
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.enroll_freshness = Duration::from_millis(120);
    cfg.handshake_timeout = Duration::from_secs(5); // outlives the window
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");
    let secret = read_enroll_secret(&meta).await.expect("secret");

    let mut conn = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let nonce = challenge_nonce(&mut conn).await;
    tokio::time::sleep(Duration::from_millis(400)).await; // past the window
    write_frame(
        &mut conn,
        &WireFrame::Enroll {
            wire_schema: WIRE_SCHEMA,
            worker_id: "w-stale".into(),
            server_nonce: nonce.clone(),
            endpoint_nonce: "worker-entropy".into(),
            hmac: squeezefs::job_wire::enroll_hmac(&secret, "w-stale", &nonce, "worker-entropy"),
            pr_key: None,
        },
    )
    .await
    .expect("send the stale hello");
    match read_frame(&mut conn).await.expect("reply").expect("frame") {
        WireFrame::EnrollRefused { reason } => {
            assert!(
                reason.contains("expired") || reason.contains("stale"),
                "the freshness refusal says so: {reason}"
            );
        }
        other => panic!("a stale challenge must be refused, got {other:?}"),
    }
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// VAL-6 / RES-5 — connection bounds: cap, pre-enrollment deadline, pruning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connections_are_capped() {
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.max_connections = 2;
    cfg.handshake_timeout = Duration::from_secs(30); // the cap, not a timeout, is under test
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");

    // Two live connections fill the cap (each parks after its challenge).
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut c = tokio::net::TcpStream::connect(host.endpoint())
            .await
            .expect("connect");
        let _ = challenge_nonce(&mut c).await;
        held.push(c);
    }
    poll_until("both connections live", Duration::from_secs(5), || {
        host.live_connections() == 2
    })
    .await;

    // The third is closed immediately — no task, no challenge, no frame.
    let mut over = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect past the cap");
    assert!(
        read_frame(&mut over).await.expect("eof is clean").is_none(),
        "a connection past the cap is closed, not served"
    );
    poll_until("refusal counted", Duration::from_secs(5), || {
        host.connections_refused() >= 1
    })
    .await;
    assert_eq!(
        host.live_connections(),
        2,
        "the cap holds while the refused connection is dropped"
    );

    // Freeing a slot re-admits.
    held.pop();
    poll_until("slot freed", Duration::from_secs(5), || {
        host.live_connections() == 1
    })
    .await;
    let mut again = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("reconnect");
    let _ = challenge_nonce(&mut again).await;

    drop(again);
    drop(held);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_enrollment_stall_is_dropped_at_the_handshake_deadline() {
    // A peer that connects and says nothing used to hold its slot for
    // 10 s (hello) and, once a length prefix arrived, forever (the body
    // read had no deadline at all).
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.handshake_timeout = Duration::from_millis(250);
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");

    // (a) silence after the challenge.
    let mut silent = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let _ = challenge_nonce(&mut silent).await;

    // (b) a length prefix with a body that never comes (slowloris).
    let mut dribble = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let _ = challenge_nonce(&mut dribble).await;
    tokio::io::AsyncWriteExt::write_all(&mut dribble, &[0u8, 0, 4, 0, b'{'])
        .await
        .expect("prefix + one body byte");

    poll_until(
        "both stalled connections reaped at the handshake deadline",
        Duration::from_secs(10),
        || host.live_connections() == 0,
    )
    .await;

    // Both see EOF.
    let mut buf = [0u8; 1];
    assert_eq!(
        tokio::io::AsyncReadExt::read(&mut silent, &mut buf)
            .await
            .expect("read after reap"),
        0,
        "the silent peer was closed"
    );
    assert_eq!(
        tokio::io::AsyncReadExt::read(&mut dribble, &mut buf)
            .await
            .expect("read after reap"),
        0,
        "the dribbling peer was closed"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_enrollment_frames_ride_the_hello_class_cap() {
    // 16 MiB is the post-enrollment cap; an unauthenticated peer gets the
    // hello class, and its body is never committed up front.
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.handshake_timeout = Duration::from_secs(5);
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");

    let mut conn = tokio::net::TcpStream::connect(host.endpoint())
        .await
        .expect("connect");
    let _ = challenge_nonce(&mut conn).await;
    let oversize = squeezefs::job_wire::MAX_HELLO_FRAME_BYTES + 1;
    tokio::io::AsyncWriteExt::write_all(&mut conn, &oversize.to_be_bytes())
        .await
        .expect("send an over-class length prefix");
    poll_until(
        "over-class pre-enrollment frame closes the connection",
        Duration::from_secs(10),
        || host.live_connections() == 0,
    )
    .await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_connection_handles_are_pruned() {
    // RES-5: `handles` was push-only — one `JoinHandle` retained per
    // connection ever accepted, all reachable before authentication.
    let (meta, _mf) = meta_fixture().await;
    let fab = fabric(&meta, 0).await;
    let mut cfg = wire_cfg(30_000, 10_000);
    cfg.handshake_timeout = Duration::from_millis(200);
    let host = JobWireHost::start(fab.clone(), cfg, FakeShardDevice::new(0, 0))
        .await
        .expect("host start");

    let base = host.retained_task_handles();
    for _ in 0..24 {
        let c = tokio::net::TcpStream::connect(host.endpoint())
            .await
            .expect("connect");
        drop(c); // immediate departure — the serve task finishes at once
    }
    poll_until("finished handles pruned", Duration::from_secs(15), || {
        host.retained_task_handles() <= base + 2
    })
    .await;
    assert!(
        host.retained_task_handles() <= base + 2,
        "24 finished connections must not each retain a JoinHandle forever \
         (retained {} vs base {base})",
        host.retained_task_handles()
    );
    assert_eq!(
        host.accept_backoffs(),
        0,
        "a healthy accept loop never backs off"
    );
    host.shutdown().await;
}
