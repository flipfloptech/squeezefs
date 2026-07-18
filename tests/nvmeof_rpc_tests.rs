//! SPDK JSON-RPC client v2 contract tests
//! (`docs/design-nvmeof-target-management.md` §6.5/§6.9, PR 3/N3).
//!
//! Zero-mock seam policy (§6.8): the client speaks to an **in-process
//! fake RPC server on a real `UnixListener`** — framing, timeouts, id
//! discipline and error mapping are exercised over a real socket; no
//! env-var behavior forks anywhere.
//!
//! Pinned here:
//! * framing: one JSON value per response, assembled across split writes;
//! * request ids increase monotonically and the echo is verified
//!   (mismatch = protocol violation);
//! * per-call timeouts, with the §6.5 slow-verb budget class
//!   (`save_config`/`load_config`/`bdev_aio_create`);
//! * typed error mapping (`SpdkRpcError::{Connect,Timeout,Rpc,Protocol}`);
//! * the version handshake + drift classification and the §6.2 gating
//!   policies (`--accept-version-drift` mutating-only; stop
//!   warns-and-proceeds; status reports);
//! * the client-side `save_config`/`load_config` composition (§6.4 SPDK
//!   persistence law mechanics), incl. the loud skipped-method report.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use squeezefs::nvmeof::spdk::lifecycle::{
    gate_version_drift, load_config, save_config, DriftPolicy, SPDK_PINNED_TAG,
};
use squeezefs::nvmeof::spdk::rpc::{
    pinned_major_minor, SpdkRpcClient, SpdkRpcError, SpdkVersion, VersionDrift, RPC_SLOW_METHODS,
};

// ---------------------------------------------------------------------------
// fake server (real UnixListener; scripted per-method behavior)
// ---------------------------------------------------------------------------

/// What the fake does with one received request.
#[derive(Clone)]
enum Script {
    /// Reply `{"jsonrpc":"2.0","id":<echo>,"result":<v>}`.
    Result(Value),
    /// Reply an error member.
    Error { code: i64, message: String },
    /// Sleep, then reply the result (timeout probes).
    SleepThenResult(Duration, Value),
    /// Reply the result in N byte-chunks with small delays (framing).
    ChunkedResult(Value, usize),
    /// Echo a WRONG id back (protocol-violation probe).
    WrongId(Value),
    /// Accept, read the request, never answer.
    Silence,
    /// `rpc_get_methods` with real `current` semantics (FIND-N3-A): the
    /// runtime-callable set only when `params.current == true`, the full
    /// method list (incl. STARTUP-only entries) otherwise.
    CurrentAwareMethods {
        current: Vec<String>,
        all: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    id: u64,
    params: Option<Value>,
}

struct FakeRpcServer {
    path: PathBuf,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    _dir: tempfile::TempDir,
}

impl FakeRpcServer {
    /// Start a fake server whose behavior is `script(method) -> Script`.
    fn start(script: impl Fn(&str) -> Script + Send + Sync + 'static) -> FakeRpcServer {
        let dir = tempfile::tempdir().expect("socket dir");
        let path = dir.path().join("spdk.sock");
        let listener = UnixListener::bind(&path).expect("bind fake rpc socket");
        let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let script = Arc::new(script);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let seen = Arc::clone(&seen_thread);
                let script = Arc::clone(&script);
                thread::spawn(move || serve_one(stream, &seen, &*script));
            }
        });
        FakeRpcServer {
            path,
            seen,
            _dir: dir,
        }
    }

    fn socket(&self) -> &PathBuf {
        &self.path
    }

    fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

/// Read exactly one JSON value off the stream (the SPDK wire format has
/// no framing header), then act the script out.
fn serve_one(
    mut stream: UnixStream,
    seen: &Arc<Mutex<Vec<SeenRequest>>>,
    script: &(dyn Fn(&str) -> Script + Send + Sync),
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let request: Value = loop {
        match serde_json::Deserializer::from_slice(&buf)
            .into_iter::<Value>()
            .next()
        {
            Some(Ok(v)) => break v,
            Some(Err(e)) if e.is_eof() => {}
            None => {}
            Some(Err(_)) => return,
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = request.get("id").and_then(Value::as_u64).unwrap_or(0);
    seen.lock().unwrap().push(SeenRequest {
        method: method.clone(),
        id,
        params: request.get("params").cloned(),
    });

    let respond = |stream: &mut UnixStream, body: Value| {
        let _ = stream.write_all(body.to_string().as_bytes());
        let _ = stream.flush();
    };
    match script(&method) {
        Script::Result(v) => respond(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": id, "result": v}),
        ),
        Script::Error { code, message } => respond(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        ),
        Script::SleepThenResult(nap, v) => {
            thread::sleep(nap);
            respond(
                &mut stream,
                json!({"jsonrpc": "2.0", "id": id, "result": v}),
            );
        }
        Script::ChunkedResult(v, chunks) => {
            let body = json!({"jsonrpc": "2.0", "id": id, "result": v}).to_string();
            let bytes = body.as_bytes();
            let step = bytes.len().div_ceil(chunks);
            for piece in bytes.chunks(step.max(1)) {
                let _ = stream.write_all(piece);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(20));
            }
        }
        Script::WrongId(v) => respond(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 999_999, "result": v}),
        ),
        Script::Silence => {
            // Hold the connection open, never answer, until the client
            // gives up and closes.
            let mut sink = [0u8; 64];
            while let Ok(n) = stream.read(&mut sink) {
                if n == 0 {
                    break;
                }
            }
        }
        Script::CurrentAwareMethods { current, all } => {
            let wants_current = request
                .get("params")
                .and_then(|p| p.get("current"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let list = if wants_current { current } else { all };
            respond(
                &mut stream,
                json!({"jsonrpc": "2.0", "id": id, "result": list}),
            );
        }
    }
}

fn fast_client(server: &FakeRpcServer) -> SpdkRpcClient {
    SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_millis(500),
        Duration::from_millis(300),
        Duration::from_secs(2),
    )
}

// ---------------------------------------------------------------------------
// framing + id discipline
// ---------------------------------------------------------------------------

#[test]
fn test_rpc_round_trip_and_id_monotonicity() {
    let server = FakeRpcServer::start(|_| Script::Result(json!({"ok": true})));
    let client = fast_client(&server);
    for _ in 0..3 {
        let out = client.call("bdev_get_bdevs", None).expect("call succeeds");
        assert_eq!(out, json!({"ok": true}));
    }
    let seen = server.seen();
    assert_eq!(seen.len(), 3, "three requests must reach the server");
    assert!(
        seen.windows(2).all(|w| w[1].id > w[0].id),
        "request ids must increase monotonically: {:?}",
        seen.iter().map(|s| s.id).collect::<Vec<_>>()
    );
    assert!(
        seen.iter().all(|s| s.method == "bdev_get_bdevs"),
        "method must be carried verbatim"
    );
}

#[test]
fn test_rpc_params_are_carried_verbatim() {
    let server = FakeRpcServer::start(|_| Script::Result(json!(null)));
    let client = fast_client(&server);
    client
        .call(
            "nvmf_subsystem_get_controllers",
            Some(json!({"nqn": "nqn.2026-07.io.squeezefs:share-x"})),
        )
        .expect("call succeeds");
    let seen = server.seen();
    assert_eq!(
        seen[0].params,
        Some(json!({"nqn": "nqn.2026-07.io.squeezefs:share-x"})),
        "params must arrive verbatim"
    );
}

#[test]
fn test_rpc_response_split_across_writes_is_assembled() {
    let server = FakeRpcServer::start(|_| {
        Script::ChunkedResult(
            json!({"version": "SPDK v26.05", "big": "x".repeat(2048)}),
            5,
        )
    });
    let client = SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_secs(1),
        Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let out = client.call("spdk_get_version", None).expect("assembled");
    assert_eq!(out["version"], "SPDK v26.05");
    assert_eq!(out["big"].as_str().unwrap().len(), 2048);
}

#[test]
fn test_rpc_id_mismatch_is_a_protocol_error() {
    let server = FakeRpcServer::start(|_| Script::WrongId(json!({"ok": true})));
    let client = fast_client(&server);
    let err = client
        .call("bdev_get_bdevs", None)
        .expect_err("must refuse");
    match &err {
        SpdkRpcError::Protocol { method, detail } => {
            assert_eq!(method, "bdev_get_bdevs");
            assert!(
                detail.contains("id"),
                "detail must name the id echo: {detail}"
            );
        }
        other => panic!("expected Protocol error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// typed errors: connect / rpc-error / timeout classes
// ---------------------------------------------------------------------------

#[test]
fn test_rpc_dead_socket_is_a_connect_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("spdk.sock");
    let client = SpdkRpcClient::with_timeouts(
        &missing,
        Duration::from_millis(300),
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    let err = client.call("spdk_get_version", None).expect_err("dead");
    match &err {
        SpdkRpcError::Connect { socket, .. } => assert_eq!(socket, &missing),
        other => panic!("expected Connect error, got {other:?}"),
    }
    assert!(
        err.to_string().contains("not answering"),
        "the dead-RPC class message names the condition: {err}"
    );
}

#[test]
fn test_rpc_error_member_maps_to_typed_error() {
    let server = FakeRpcServer::start(|_| Script::Error {
        code: -32601,
        message: "Method not found".to_string(),
    });
    let client = fast_client(&server);
    let err = client.call("bogus_method", None).expect_err("must fail");
    match &err {
        SpdkRpcError::Rpc {
            method,
            code,
            message,
        } => {
            assert_eq!(method, "bogus_method");
            assert_eq!(*code, -32601);
            assert_eq!(message, "Method not found");
        }
        other => panic!("expected Rpc error, got {other:?}"),
    }
}

#[test]
fn test_rpc_call_timeout_fires_within_budget() {
    let server = FakeRpcServer::start(|_| Script::Silence);
    let client = SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_millis(500),
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = client.call("bdev_get_bdevs", None).expect_err("timeout");
    let elapsed = started.elapsed();
    match &err {
        SpdkRpcError::Timeout { method, budget_ms } => {
            assert_eq!(method, "bdev_get_bdevs");
            assert_eq!(*budget_ms, 300);
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(280) && elapsed < Duration::from_secs(3),
        "timeout must fire near its budget, not hang: {elapsed:?}"
    );
}

#[test]
fn test_rpc_slow_methods_get_the_slow_budget() {
    // The §6.5 slow verbs carry the 60 s class; here scaled: normal 250 ms,
    // slow 2 s, server answers after 600 ms.
    for method in RPC_SLOW_METHODS {
        let server = FakeRpcServer::start(|_| {
            Script::SleepThenResult(Duration::from_millis(600), json!({"ok": true}))
        });
        let client = SpdkRpcClient::with_timeouts(
            server.socket(),
            Duration::from_millis(500),
            Duration::from_millis(250),
            Duration::from_secs(2),
        );
        client
            .call(method, Some(json!({})))
            .unwrap_or_else(|e| panic!("slow verb '{method}' must ride the slow budget: {e}"));
    }
    // …and a normal method with the same server latency times out.
    let server = FakeRpcServer::start(|_| {
        Script::SleepThenResult(Duration::from_millis(600), json!({"ok": true}))
    });
    let client = SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_millis(500),
        Duration::from_millis(250),
        Duration::from_secs(2),
    );
    let err = client
        .call("spdk_get_version", None)
        .expect_err("normal budget");
    assert!(matches!(err, SpdkRpcError::Timeout { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// version handshake + drift classification (§6.2/§6.5)
// ---------------------------------------------------------------------------

#[test]
fn test_pinned_major_minor_parses_the_pin() {
    assert!(SPDK_PINNED_TAG.starts_with('v'));
    assert_eq!(pinned_major_minor(), (26, 5), "pin is v26.05");
}

#[test]
fn test_version_parse_prefers_fields_and_falls_back_to_string() {
    let v = SpdkVersion::parse(&json!({
        "version": "SPDK v26.05",
        "fields": {"major": 26, "minor": 5, "patch": 0, "suffix": ""}
    }))
    .expect("parse");
    assert_eq!((v.major, v.minor), (26, 5));
    assert_eq!(v.raw, "SPDK v26.05");
    assert_eq!(v.drift(), None, "the pin itself is not drift");

    let v = SpdkVersion::parse(&json!({"version": "SPDK v26.09-pre"})).expect("string fallback");
    assert_eq!((v.major, v.minor), (26, 9));
    assert_eq!(v.drift(), Some(VersionDrift::Minor));

    let v = SpdkVersion::parse(&json!({"version": "SPDK v27.01"})).expect("major");
    assert_eq!(v.drift(), Some(VersionDrift::Major));

    SpdkVersion::parse(&json!({"nonsense": true})).expect_err("garbage refuses");
}

#[test]
fn test_version_handshake_against_the_fake_server() {
    let server = FakeRpcServer::start(|method| {
        assert_eq!(method, "spdk_get_version");
        Script::Result(json!({
            "version": "SPDK v26.05",
            "fields": {"major": 26, "minor": 5, "patch": 0, "suffix": ""}
        }))
    });
    let client = fast_client(&server);
    let v = client.version().expect("handshake");
    assert_eq!((v.major, v.minor), (26, 5));
    assert_eq!(v.raw, "SPDK v26.05");
}

/// §6.2 flag placement: mutating verbs refuse drift without
/// `--accept-version-drift` (the G3 wrong-version loud fail), and proceed
/// with a loud warning under the flag; `target stop` warns-and-proceeds;
/// `target status` reports only.
#[test]
fn test_drift_gate_policies() {
    let pinned = SpdkVersion::parse(&json!({"version": "SPDK v26.05"})).unwrap();
    let drifted = SpdkVersion::parse(&json!({"version": "SPDK v27.01"})).unwrap();

    // No drift: silent pass under every policy.
    for policy in [
        DriftPolicy::Mutating {
            accept_version_drift: false,
        },
        DriftPolicy::WarnAndProceed,
        DriftPolicy::ReportOnly,
    ] {
        assert_eq!(
            gate_version_drift(&pinned, policy).expect("no drift passes"),
            None
        );
    }

    // Mutating without the flag: the designed refusal.
    let err = gate_version_drift(
        &drifted,
        DriftPolicy::Mutating {
            accept_version_drift: false,
        },
    )
    .expect_err("mutating verbs refuse drift");
    let text = err.to_string();
    assert!(
        text.contains(SPDK_PINNED_TAG) && text.contains("SPDK v27.01"),
        "must name both the pin and the reported version: {text}"
    );
    assert!(
        text.contains("--accept-version-drift"),
        "must name the explicit override flag: {text}"
    );
    assert!(
        text.contains("target install"),
        "must name the pinned-install remediation: {text}"
    );

    // Mutating WITH the flag: proceeds with a loud warning line.
    let warn = gate_version_drift(
        &drifted,
        DriftPolicy::Mutating {
            accept_version_drift: true,
        },
    )
    .expect("flag accepts drift")
    .expect("but never silently");
    assert!(
        warn.contains("SPDK v27.01"),
        "warning names the version: {warn}"
    );

    // Stop: warn-and-proceed without any flag.
    let warn = gate_version_drift(&drifted, DriftPolicy::WarnAndProceed)
        .expect("stop proceeds")
        .expect("loudly");
    assert!(warn.contains("drift"), "warning names drift: {warn}");

    // Status: reports (the payload carries rpc.drift) — no gate output.
    assert_eq!(
        gate_version_drift(&drifted, DriftPolicy::ReportOnly).expect("status never refuses"),
        None
    );
}

// ---------------------------------------------------------------------------
// save_config / load_config composition (§6.4 persistence-law mechanics)
// ---------------------------------------------------------------------------

#[test]
fn test_save_config_composes_subsystem_configs_and_writes_atomically() {
    let server = FakeRpcServer::start(|method| match method {
        "framework_get_subsystems" => Script::Result(json!([
            {"subsystem": "bdev", "depends_on": []},
            {"subsystem": "nvmf", "depends_on": ["bdev"]}
        ])),
        "framework_get_config" => Script::Result(json!([
            {"method": "bdev_aio_create", "params": {"name": "sqz_aio_x"}}
        ])),
        other => panic!("unexpected method {other}"),
    });
    let client = SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("spdk").join("tgt-config.json");
    save_config(&client, &cfg_path).expect("save_config");

    let written: Value =
        serde_json::from_slice(&std::fs::read(&cfg_path).expect("config written")).unwrap();
    let subsystems = written["subsystems"].as_array().expect("subsystems array");
    assert_eq!(subsystems.len(), 2);
    assert_eq!(subsystems[0]["subsystem"], "bdev");
    assert_eq!(
        subsystems[0]["config"][0]["method"], "bdev_aio_create",
        "per-subsystem config captured: {written}"
    );
    // Atomic-replace discipline: no tmp residue.
    let residue: Vec<_> = std::fs::read_dir(cfg_path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(residue.is_empty(), "no tmp residue after save: {residue:?}");
    // The composition asked the target, per subsystem.
    let methods: Vec<String> = server.seen().iter().map(|s| s.method.clone()).collect();
    assert_eq!(
        methods,
        vec![
            "framework_get_subsystems".to_string(),
            "framework_get_config".to_string(),
            "framework_get_config".to_string(),
        ]
    );
}

/// The FIND-N3-A regression pin (caught by the root-tier gate, run 1):
/// a RUNTIME target's `rpc_get_methods` lists ALL methods unless
/// `{"current": true}` is passed — without it, load_config replayed the
/// STARTUP-only `sock_set_default_impl` a real save_config captures and
/// the target refused ("Method may only be called before framework is
/// initialized"). The fake mirrors the real semantics: the full method
/// list without `current: true`, the runtime-callable set with it — so a
/// client that drops the param sends the startup-only method and panics
/// the fake.
#[test]
fn test_load_config_replays_runtime_methods_and_reports_skipped() {
    let server = FakeRpcServer::start(move |method| match method {
        "rpc_get_methods" => Script::CurrentAwareMethods {
            current: vec![
                "bdev_aio_create".into(),
                "nvmf_create_transport".into(),
                "nvmf_create_subsystem".into(),
            ],
            all: vec![
                "framework_set_scheduler".into(),
                "bdev_aio_create".into(),
                "nvmf_create_transport".into(),
                "nvmf_create_subsystem".into(),
            ],
        },
        "bdev_aio_create" | "nvmf_create_transport" | "nvmf_create_subsystem" => {
            Script::Result(json!(true))
        }
        other => panic!(
            "startup-only method must never be sent to a runtime target: {other} (the client \
             must gate on rpc_get_methods current:true — FIND-N3-A)"
        ),
    });
    let client = SpdkRpcClient::with_timeouts(
        server.socket(),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let config = json!({
        "subsystems": [
            {"subsystem": "scheduler", "config": [
                {"method": "framework_set_scheduler", "params": {"name": "static"}}
            ]},
            {"subsystem": "bdev", "config": [
                {"method": "bdev_aio_create", "params": {"name": "sqz_aio_x"}}
            ]},
            {"subsystem": "nvmf", "config": [
                {"method": "nvmf_create_transport", "params": {"trtype": "TCP"}},
                {"method": "nvmf_create_subsystem", "params": {"nqn": "nqn.x"}}
            ]}
        ]
    });
    let report = load_config(&client, &config).expect("load_config");
    assert_eq!(report.applied, 3, "the three runtime methods replay");
    assert_eq!(
        report.skipped,
        vec!["framework_set_scheduler".to_string()],
        "STARTUP-only entries are reported loud, never silently dropped"
    );
    let seen = server.seen();
    let replayed: Vec<&str> = seen
        .iter()
        .map(|s| s.method.as_str())
        .filter(|m| *m != "rpc_get_methods")
        .collect();
    assert_eq!(
        replayed,
        vec![
            "bdev_aio_create",
            "nvmf_create_transport",
            "nvmf_create_subsystem"
        ],
        "replay preserves config order within the pass"
    );
}
