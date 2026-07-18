//! `SpdkStack` contract tests (`docs/design-nvmeof-target-management.md`
//! §6.4/§6.5, PR 4/N4) — SPDK share/unshare/restore with pinned
//! nsid + ns UUID + `ptpl_file`, driven through the `TargetStack` trait
//! against an **in-process stateful fake spdk_tgt on a real
//! `UnixListener`** (the §6.8 zero-mock seam: real socket, real JSON-RPC
//! framing, no env behavior forks). Real-target semantics are proven by
//! the N4 root-tier gate; this tier pins:
//!
//! * **Intent ordering (§6.4 law 6)**: the `pending` ledger record is
//!   durable BEFORE the first RPC mutation reaches the target — asserted
//!   via the fake server's per-call ledger snapshots, not trust;
//! * **Identity pinning (§6.5)**: `nvmf_subsystem_add_ns` carries
//!   `nsid` / `uuid` / `ptpl_file` verbatim (`<state>/spdk/ptpl/
//!   <uuid>.json`), `bdev_aio_create` keeps block_size 4096, and restore
//!   re-presents the recorded identity (same uuid, nsid, serial);
//! * **SPDK persistence law (§6.4)**: `share` and `unshare` end with
//!   `save_config`; `restore` saves **whenever reconciliation changed
//!   anything** (re-adds, finalized `pending` intents, resumed
//!   `removing` teardowns — the resurrection law) and skips it on a
//!   pure verified-no-op pass;
//! * **Duplicate guard, triple source**: ledger membership (both
//!   stacks), the live `bdev_get_bdevs` filename scan, and the
//!   cross-stack walk (`cross_stack_duplicate_guard`) — each refusal is
//!   the runbook (holder + classification + removal steps), and refusals
//!   fire with ZERO mutating RPCs sent;
//! * **Vanished objects are verified no-ops** on unshare/restore;
//! * **Live-consumer refusal** (design R7): unshare refuses while the
//!   subsystem has live controllers unless `--force`, naming the NQN.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use squeezefs::nvmeof::cross_stack_duplicate_guard;
use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::spdk::{SpdkPaths, SpdkStack};
use squeezefs::nvmeof::stack::{
    Listener, LiveShare, NvmeofError, PreflightOp, RestoreOutcome, ShareRecord, ShareRequest,
    ShareState, TargetStack,
};
use squeezefs::nvmeof::StackKind;

const UUID_A: &str = "e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1";
const UUID_B: &str = "0f0e0d0c-0b0a-4a09-8807-060504030201";
const NQN_A: &str = "nqn.2026-07.io.squeezefs:share-aaaaaaaa";
const NQN_B: &str = "nqn.2026-07.io.squeezefs:share-bbbbbbbb";

/// Every RPC method that mutates target state (the §6.4 law-6 boundary:
/// the pending intent must be durable before ANY of these is sent).
const MUTATING_METHODS: &[&str] = &[
    "nvmf_create_transport",
    "bdev_aio_create",
    "bdev_aio_delete",
    "nvmf_create_subsystem",
    "nvmf_delete_subsystem",
    "nvmf_subsystem_add_host",
    "nvmf_subsystem_add_ns",
    "nvmf_subsystem_add_listener",
    "nvmf_subsystem_remove_listener",
];

// ---------------------------------------------------------------------------
// stateful fake spdk_tgt (real UnixListener; per-call ledger snapshots)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct FakeNamespace {
    nsid: u64,
    bdev_name: String,
    uuid: String,
    ptpl_file: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct FakeSubsystem {
    nqn: String,
    allow_any_host: bool,
    serial_number: String,
    hosts: Vec<String>,
    namespaces: Vec<FakeNamespace>,
    /// (traddr, trsvcid)
    listeners: Vec<(String, String)>,
}

#[derive(Debug, Default)]
struct TargetState {
    transports: Vec<String>,
    /// name -> aio filename (None = non-aio bdev, e.g. a foreign Malloc)
    bdevs: Vec<(String, Option<String>)>,
    subsystems: Vec<FakeSubsystem>,
    /// nqn -> live controller count (consumer-refusal probe)
    controllers: HashMap<String, usize>,
    /// methods scripted to fail with an RPC error (crash/failure inject)
    fail_methods: Vec<String>,
}

/// One observed request, with the share ledger's state AT ARRIVAL — the
/// §6.4 law-6 ordering witness (subnqn -> intent state string).
#[derive(Debug, Clone)]
struct Call {
    method: String,
    params: Option<Value>,
    ledger: Vec<(String, String)>,
}

struct FakeTgt {
    state: Arc<Mutex<TargetState>>,
    calls: Arc<Mutex<Vec<Call>>>,
    _dir: (),
}

fn snapshot_ledger(state_dir: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = fs::read(state_dir.join("shares.json")) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    v.get("shares")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some((
                        r.get("subnqn")?.as_str()?.to_string(),
                        r.get("state")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl FakeTgt {
    fn start(socket: &Path, version: &'static str, state_dir: PathBuf) -> FakeTgt {
        let listener = UnixListener::bind(socket).expect("bind fake spdk_tgt socket");
        let state: Arc<Mutex<TargetState>> = Arc::new(Mutex::new(TargetState::default()));
        let calls: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
        let (state_t, calls_t) = (Arc::clone(&state), Arc::clone(&calls));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                serve_one(stream, &state_t, &calls_t, version, &state_dir);
            }
        });
        FakeTgt {
            state,
            calls,
            _dir: (),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.method).collect()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TargetState> {
        self.state.lock().unwrap()
    }

    /// Seed one served subsystem + its aio bdev (live-state fixtures).
    #[allow(clippy::too_many_arguments)]
    fn seed_share(
        &self,
        nqn: &str,
        bdev: &str,
        filename: &str,
        nsid: u64,
        uuid: &str,
        ptpl: Option<&str>,
        listeners: &[(&str, u16)],
    ) {
        let mut st = self.state();
        st.bdevs
            .push((bdev.to_string(), Some(filename.to_string())));
        st.subsystems.push(FakeSubsystem {
            nqn: nqn.to_string(),
            allow_any_host: true,
            serial_number: "FAKESERIAL".to_string(),
            hosts: Vec::new(),
            namespaces: vec![FakeNamespace {
                nsid,
                bdev_name: bdev.to_string(),
                uuid: uuid.to_string(),
                ptpl_file: ptpl.map(str::to_string),
            }],
            listeners: listeners
                .iter()
                .map(|(ip, port)| (ip.to_string(), port.to_string()))
                .collect(),
        });
    }
}

fn rpc_error(id: u64, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_result(id: u64, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn serve_one(
    mut stream: UnixStream,
    state: &Arc<Mutex<TargetState>>,
    calls: &Arc<Mutex<Vec<Call>>>,
    version: &str,
    state_dir: &Path,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
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
    let params = request.get("params").cloned();
    calls.lock().unwrap().push(Call {
        method: method.clone(),
        params: params.clone(),
        ledger: snapshot_ledger(state_dir),
    });

    let mut st = state.lock().unwrap();
    let response = if st.fail_methods.iter().any(|m| m == &method) {
        rpc_error(id, -32602, &format!("scripted failure of '{method}'"))
    } else {
        dispatch(&mut st, &method, params.as_ref(), id, version)
    };
    drop(st);
    let _ = stream.write_all(response.to_string().as_bytes());
    let _ = stream.flush();
}

fn p_str(params: Option<&Value>, key: &str) -> String {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn dispatch(
    st: &mut TargetState,
    method: &str,
    params: Option<&Value>,
    id: u64,
    version: &str,
) -> Value {
    match method {
        "spdk_get_version" => rpc_result(id, json!({"version": version})),
        "rpc_get_methods" => rpc_result(
            id,
            json!(MUTATING_METHODS
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()),
        ),
        "nvmf_get_transports" => rpc_result(
            id,
            json!(st
                .transports
                .iter()
                .map(|t| json!({"trtype": t}))
                .collect::<Vec<_>>()),
        ),
        "nvmf_create_transport" => {
            let trtype = p_str(params, "trtype");
            if st
                .transports
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&trtype))
            {
                return rpc_error(
                    id,
                    -32602,
                    &format!("Transport type '{trtype}' already exists"),
                );
            }
            st.transports.push(trtype);
            rpc_result(id, json!(true))
        }
        "bdev_get_bdevs" => {
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let rows: Vec<Value> = st
                .bdevs
                .iter()
                .filter(|(n, _)| name.as_deref().is_none_or(|want| want == n))
                .map(|(n, f)| match f {
                    Some(filename) => json!({
                        "name": n, "block_size": 4096,
                        "driver_specific": {"aio": {"filename": filename}}
                    }),
                    None => json!({"name": n, "block_size": 512, "driver_specific": {}}),
                })
                .collect();
            if name.is_some() && rows.is_empty() {
                return rpc_error(id, -19, "No such device");
            }
            rpc_result(id, json!(rows))
        }
        "bdev_aio_create" => {
            let name = p_str(params, "name");
            let filename = p_str(params, "filename");
            if st.bdevs.iter().any(|(n, _)| n == &name) {
                return rpc_error(id, -32602, &format!("bdev '{name}' already exists"));
            }
            st.bdevs.push((name.clone(), Some(filename)));
            rpc_result(id, json!(name))
        }
        "bdev_aio_delete" => {
            let name = p_str(params, "name");
            let before = st.bdevs.len();
            st.bdevs.retain(|(n, _)| n != &name);
            if st.bdevs.len() == before {
                return rpc_error(id, -19, &format!("No such device: {name}"));
            }
            rpc_result(id, json!(true))
        }
        "nvmf_create_subsystem" => {
            let nqn = p_str(params, "nqn");
            if st.subsystems.iter().any(|s| s.nqn == nqn) {
                return rpc_error(id, -32602, &format!("Subsystem {nqn} already exists"));
            }
            st.subsystems.push(FakeSubsystem {
                nqn,
                allow_any_host: params
                    .and_then(|p| p.get("allow_any_host"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                serial_number: p_str(params, "serial_number"),
                ..FakeSubsystem::default()
            });
            rpc_result(id, json!(true))
        }
        "nvmf_delete_subsystem" => {
            let nqn = p_str(params, "nqn");
            let before = st.subsystems.len();
            st.subsystems.retain(|s| s.nqn != nqn);
            if st.subsystems.len() == before {
                return rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}"));
            }
            rpc_result(id, json!(true))
        }
        "nvmf_subsystem_add_host" => {
            let nqn = p_str(params, "nqn");
            let host = p_str(params, "host");
            match st.subsystems.iter_mut().find(|s| s.nqn == nqn) {
                Some(sub) => {
                    sub.hosts.push(host);
                    rpc_result(id, json!(true))
                }
                None => rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}")),
            }
        }
        "nvmf_subsystem_add_ns" => {
            let nqn = p_str(params, "nqn");
            let ns = params.and_then(|p| p.get("namespace"));
            match st.subsystems.iter_mut().find(|s| s.nqn == nqn) {
                Some(sub) => {
                    sub.namespaces.push(FakeNamespace {
                        nsid: ns
                            .and_then(|n| n.get("nsid"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        bdev_name: ns
                            .and_then(|n| n.get("bdev_name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        uuid: ns
                            .and_then(|n| n.get("uuid"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        ptpl_file: ns
                            .and_then(|n| n.get("ptpl_file"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                    rpc_result(id, json!(1))
                }
                None => rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}")),
            }
        }
        "nvmf_subsystem_add_listener" | "nvmf_subsystem_remove_listener" => {
            let nqn = p_str(params, "nqn");
            let la = params.and_then(|p| p.get("listen_address"));
            let traddr = la
                .and_then(|l| l.get("traddr"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let trsvcid = la
                .and_then(|l| l.get("trsvcid"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match st.subsystems.iter_mut().find(|s| s.nqn == nqn) {
                Some(sub) => {
                    if method == "nvmf_subsystem_add_listener" {
                        sub.listeners.push((traddr, trsvcid));
                    } else {
                        let before = sub.listeners.len();
                        sub.listeners
                            .retain(|(a, s)| !(a == &traddr && s == &trsvcid));
                        if sub.listeners.len() == before {
                            return rpc_error(id, -32602, "Listener not found");
                        }
                    }
                    rpc_result(id, json!(true))
                }
                None => rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}")),
            }
        }
        "nvmf_get_subsystems" => {
            let mut rows = vec![json!({
                "nqn": "nqn.2014-08.org.nvmexpress.discovery",
                "subtype": "Discovery", "listen_addresses": [],
            })];
            for sub in &st.subsystems {
                rows.push(json!({
                    "nqn": sub.nqn, "subtype": "NVMe",
                    "allow_any_host": sub.allow_any_host,
                    "serial_number": sub.serial_number,
                    "hosts": sub.hosts.iter().map(|h| json!({"nqn": h})).collect::<Vec<_>>(),
                    "listen_addresses": sub.listeners.iter().map(|(a, s)| json!({
                        "trtype": "TCP", "adrfam": "IPv4", "traddr": a, "trsvcid": s,
                    })).collect::<Vec<_>>(),
                    "namespaces": sub.namespaces.iter().map(|n| json!({
                        "nsid": n.nsid, "bdev_name": n.bdev_name, "name": n.bdev_name,
                        "uuid": n.uuid,
                    })).collect::<Vec<_>>(),
                }));
            }
            rpc_result(id, json!(rows))
        }
        "nvmf_subsystem_get_controllers" => {
            let nqn = p_str(params, "nqn");
            if !st.subsystems.iter().any(|s| s.nqn == nqn) {
                return rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}"));
            }
            let count = st.controllers.get(&nqn).copied().unwrap_or(0);
            rpc_result(
                id,
                json!((0..count)
                    .map(|i| json!({"cntlid": i + 1}))
                    .collect::<Vec<_>>()),
            )
        }
        "framework_get_subsystems" => rpc_result(
            id,
            json!([
                {"subsystem": "bdev", "depends_on": []},
                {"subsystem": "nvmf", "depends_on": ["bdev"]},
            ]),
        ),
        "framework_get_config" => {
            let name = p_str(params, "name");
            let cfg: Vec<Value> = match name.as_str() {
                "bdev" => st
                    .bdevs
                    .iter()
                    .filter_map(|(n, f)| {
                        f.as_ref().map(|filename| {
                            json!({"method": "bdev_aio_create",
                                   "params": {"name": n, "filename": filename, "block_size": 4096}})
                        })
                    })
                    .collect(),
                "nvmf" => {
                    let mut out: Vec<Value> = st
                        .transports
                        .iter()
                        .map(
                            |t| json!({"method": "nvmf_create_transport", "params": {"trtype": t}}),
                        )
                        .collect();
                    for sub in &st.subsystems {
                        out.push(json!({"method": "nvmf_create_subsystem",
                            "params": {"nqn": sub.nqn, "allow_any_host": sub.allow_any_host,
                                       "serial_number": sub.serial_number}}));
                        for ns in &sub.namespaces {
                            out.push(json!({"method": "nvmf_subsystem_add_ns",
                                "params": {"nqn": sub.nqn, "namespace": {
                                    "bdev_name": ns.bdev_name, "nsid": ns.nsid,
                                    "uuid": ns.uuid, "ptpl_file": ns.ptpl_file}}}));
                        }
                        for (a, s) in &sub.listeners {
                            out.push(json!({"method": "nvmf_subsystem_add_listener",
                                "params": {"nqn": sub.nqn, "listen_address": {
                                    "trtype": "TCP", "adrfam": "IPv4",
                                    "traddr": a, "trsvcid": s}}}));
                        }
                    }
                    out
                }
                _ => Vec::new(),
            };
            rpc_result(id, json!(cfg))
        }
        other => rpc_error(id, -32601, &format!("Method not found: {other}")),
    }
}

// ---------------------------------------------------------------------------
// rig
// ---------------------------------------------------------------------------

struct Rig {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    server: FakeTgt,
    stack: SpdkStack,
    ledger: Ledger,
}

impl Rig {
    fn paths(&self) -> SpdkPaths {
        SpdkPaths::with_roots(
            self._dir.path().join("install"),
            self._dir.path().join("run"),
            self.state_dir.clone(),
        )
    }

    /// Same rig, different verb-layer options (drift acceptance / force).
    fn stack_with(&self, accept_version_drift: bool, unshare_force: bool) -> SpdkStack {
        SpdkStack::new(
            self.paths(),
            Ledger::new(&self.state_dir),
            accept_version_drift,
            unshare_force,
        )
    }

    fn tgt_config(&self) -> PathBuf {
        self.state_dir.join("spdk").join("tgt-config.json")
    }
}

fn rig_with_version(version: &'static str) -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = dir.path().join("install");
    let run = dir.path().join("run");
    let state_dir = dir.path().join("state");
    fs::create_dir_all(install.join("build/bin")).expect("install tree");
    fs::create_dir_all(&run).expect("run dir");
    fs::create_dir_all(&state_dir).expect("state dir");
    // Preflight rung 1: a present, executable pinned binary.
    let bin = install.join("build/bin/spdk_tgt");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").expect("dummy spdk_tgt");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod");
    // Preflight rung 2: a pidfile naming a LIVE pid (our own).
    fs::write(
        run.join("spdk_tgt.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("pidfile");
    let server = FakeTgt::start(&run.join("spdk.sock"), version, state_dir.clone());
    let paths = SpdkPaths::with_roots(&install, &run, &state_dir);
    let stack = SpdkStack::new(paths, Ledger::new(&state_dir), false, false);
    Rig {
        _dir: dir,
        state_dir: state_dir.clone(),
        server,
        stack,
        ledger: Ledger::new(&state_dir),
    }
}

fn rig() -> Rig {
    rig_with_version("SPDK v26.05")
}

fn req(subnqn: &str, backing: &str, uuid: &str, listeners: &[(&str, u16)]) -> ShareRequest {
    ShareRequest {
        subnqn: subnqn.to_string(),
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
        nsid: None,
        ns_uuid: uuid.to_string(),
        listeners: listeners
            .iter()
            .map(|(ip, port)| Listener {
                ip: ip.to_string(),
                port: *port,
                nvmet_port_id: None,
            })
            .collect(),
        allow_hosts: Vec::new(),
    }
}

fn find_call<'a>(calls: &'a [Call], method: &str) -> Option<&'a Call> {
    calls.iter().find(|c| c.method == method)
}

fn method_index(methods: &[String], want: &str) -> Option<usize> {
    methods.iter().position(|m| m == want)
}

// ---------------------------------------------------------------------------
// share: intent ordering + identity pinning + persistence law
// ---------------------------------------------------------------------------

/// §6.4 law 6: the `pending` intent record is durable BEFORE the first
/// RPC mutation — witnessed by the fake server's ledger snapshot at every
/// mutating call, never by trusting the implementation's ordering.
#[test]
fn test_spdk_share_records_pending_intent_before_first_rpc_mutation() {
    let r = rig();
    let record = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect("share succeeds");
    assert_eq!(record.state, ShareState::Active);

    let calls = r.server.calls();
    let mutating: Vec<&Call> = calls
        .iter()
        .filter(|c| MUTATING_METHODS.contains(&c.method.as_str()))
        .collect();
    assert!(
        !mutating.is_empty(),
        "share must have sent mutating RPCs: {:?}",
        r.server.methods()
    );
    for call in &mutating {
        assert_eq!(
            call.ledger
                .iter()
                .find(|(nqn, _)| nqn == NQN_A)
                .map(|(_, state)| state.as_str()),
            Some("pending"),
            "law 6 violated: mutating RPC '{}' arrived while the ledger did NOT carry the \
             pending intent (snapshot: {:?})",
            call.method,
            call.ledger
        );
    }
    // The first mutating call is the bdev/transport rung — never a
    // subsystem mutation before its bdev exists.
    assert!(
        matches!(
            mutating[0].method.as_str(),
            "nvmf_create_transport" | "bdev_aio_create"
        ),
        "unexpected first mutation: {}",
        mutating[0].method
    );
}

/// §6.5 identity pinning, asserted VERBATIM off the wire: `add_ns`
/// carries `-n <nsid>` / `-u <uuid>` / `--ptpl-file <state>/spdk/ptpl/
/// <uuid>.json`, and `bdev_aio_create` keeps block_size 4096. The
/// record carries the §6.4 SPDK presence shape (nsid, ns_uuid, relative
/// ptpl_file, bdev_name; loop_device / nvmet_port_id null).
#[test]
fn test_spdk_share_pins_nsid_uuid_ptpl_and_block_size_verbatim() {
    let r = rig();
    let mut request = req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]);
    request.nsid = Some(7);
    let record = r.stack.share(&request).expect("share succeeds");

    let calls = r.server.calls();
    let create = find_call(&calls, "bdev_aio_create").expect("bdev_aio_create sent");
    let p = create.params.as_ref().expect("params");
    assert_eq!(p["filename"], "/dev/null");
    assert_eq!(p["block_size"], 4096, "block_size 4096 is pinned (§6.5)");
    let bdev_name = p["name"].as_str().expect("bdev name").to_string();

    let add_ns = find_call(&calls, "nvmf_subsystem_add_ns").expect("add_ns sent");
    let ns = &add_ns.params.as_ref().expect("params")["namespace"];
    assert_eq!(ns["nsid"], 7, "explicit --nsid pinned via -n");
    assert_eq!(ns["uuid"], UUID_A, "ns UUID pinned via -u");
    assert_eq!(ns["bdev_name"], bdev_name.as_str());
    let expected_ptpl = r
        .state_dir
        .join("spdk")
        .join("ptpl")
        .join(format!("{UUID_A}.json"));
    assert_eq!(
        ns["ptpl_file"],
        expected_ptpl.display().to_string().as_str(),
        "--ptpl-file must be the absolute <state>/spdk/ptpl/<uuid>.json"
    );
    assert!(
        expected_ptpl.parent().unwrap().is_dir(),
        "the ptpl dir must exist before add_ns (SPDK only creates the file)"
    );

    // Record presence shape (§6.4).
    assert_eq!(record.stack, StackKind::Spdk);
    assert_eq!(record.nsid, Some(7));
    assert_eq!(record.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(record.bdev_name.as_deref(), Some(bdev_name.as_str()));
    assert_eq!(
        record.ptpl_file.as_deref(),
        Some(format!("spdk/ptpl/{UUID_A}.json").as_str()),
        "the ledger records the state-dir-relative ptpl path (§6.4 schema)"
    );
    assert_eq!(record.loop_device, None, "loop_device is nvmet-only");
    assert!(
        record.listeners.iter().all(|l| l.nvmet_port_id.is_none()),
        "nvmet_port_id is nvmet-only"
    );

    // allow-any default: allow_any_host=true, zero add_host calls.
    let create_sub = find_call(&calls, "nvmf_create_subsystem").expect("create_subsystem");
    assert_eq!(create_sub.params.as_ref().unwrap()["allow_any_host"], true);
    assert!(find_call(&calls, "nvmf_subsystem_add_host").is_none());
}

/// Default nsid is 1; one `add_listener` per (ip, port) with the address
/// carried verbatim; `--allow-host` wires `allow_any_host=false` +
/// one `nvmf_subsystem_add_host` per host (the nvmet-precedent wiring).
#[test]
fn test_spdk_share_defaults_nsid_1_wires_listeners_and_allow_hosts() {
    let r = rig();
    let mut request = req(
        NQN_A,
        "/dev/null",
        UUID_A,
        &[("127.0.0.1", 4420), ("10.0.0.9", 4421)],
    );
    request.allow_hosts = vec![
        "nqn.2014-08.org.nvmexpress:uuid:h1".to_string(),
        "nqn.2014-08.org.nvmexpress:uuid:h2".to_string(),
    ];
    let record = r.stack.share(&request).expect("share succeeds");
    assert_eq!(record.nsid, Some(1), "default --nsid is 1 (§6.2)");

    let calls = r.server.calls();
    assert_eq!(
        find_call(&calls, "nvmf_subsystem_add_ns")
            .unwrap()
            .params
            .as_ref()
            .unwrap()["namespace"]["nsid"],
        1
    );
    let listeners: Vec<(String, String)> = calls
        .iter()
        .filter(|c| c.method == "nvmf_subsystem_add_listener")
        .map(|c| {
            let la = &c.params.as_ref().unwrap()["listen_address"];
            (
                la["traddr"].as_str().unwrap().to_string(),
                la["trsvcid"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        listeners,
        vec![
            ("127.0.0.1".to_string(), "4420".to_string()),
            ("10.0.0.9".to_string(), "4421".to_string())
        ],
        "one listener per (ip, port), verbatim"
    );

    let create_sub = find_call(&calls, "nvmf_create_subsystem").unwrap();
    assert_eq!(
        create_sub.params.as_ref().unwrap()["allow_any_host"],
        false,
        "an allowlist share must NOT be allow-any"
    );
    let hosts: Vec<String> = calls
        .iter()
        .filter(|c| c.method == "nvmf_subsystem_add_host")
        .map(|c| {
            c.params.as_ref().unwrap()["host"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        hosts,
        vec![
            "nqn.2014-08.org.nvmexpress:uuid:h1".to_string(),
            "nqn.2014-08.org.nvmexpress:uuid:h2".to_string()
        ]
    );
    assert_eq!(record.allow_hosts.len(), 2, "allowlist recorded (§6.4)");
}

/// §6.4 SPDK persistence law on share: the LAST mutation is
/// `save_config` (the composition runs after every target mutation),
/// and the ledger flips `pending -> active` only after it — witnessed
/// by the ledger snapshot AT the save composition's first call.
#[test]
fn test_spdk_share_saves_config_then_finalizes_active() {
    let r = rig();
    let record = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect("share succeeds");
    assert_eq!(record.state, ShareState::Active);

    let methods = r.server.methods();
    let save_at = method_index(&methods, "framework_get_subsystems")
        .expect("share must end with the save_config composition");
    let last_mutation = methods
        .iter()
        .enumerate()
        .filter(|(_, m)| MUTATING_METHODS.contains(&m.as_str()))
        .map(|(i, _)| i)
        .max()
        .expect("mutations exist");
    assert!(
        last_mutation < save_at,
        "save_config must run AFTER the last target mutation: {methods:?}"
    );

    // At the save composition the record is still pending (§6.4 law 6:
    // active only after the last mutation = save_config succeeds).
    let calls = r.server.calls();
    let save_call = find_call(&calls, "framework_get_subsystems").unwrap();
    assert_eq!(
        save_call
            .ledger
            .iter()
            .find(|(nqn, _)| nqn == NQN_A)
            .map(|(_, s)| s.as_str()),
        Some("pending"),
        "finalize must happen only after save_config"
    );

    // The SPDK source of truth now carries the share.
    let cfg = fs::read_to_string(r.tgt_config()).expect("tgt-config.json written");
    assert!(
        cfg.contains(NQN_A),
        "tgt-config.json must capture the subsystem: {cfg}"
    );
    assert_eq!(
        r.ledger.find(NQN_A).unwrap().unwrap().state,
        ShareState::Active
    );
}

/// A mid-flight RPC failure leaves the pending intent CLAIMING the
/// partial objects (§6.4 law 6) — and `restore` reconciles it: the
/// interrupted share never completed, so the intent is garbage-collected
/// loudly and the partial residue swept.
#[test]
fn test_spdk_share_midflight_failure_leaves_reconcilable_pending_intent() {
    let r = rig();
    r.server
        .state()
        .fail_methods
        .push("nvmf_subsystem_add_ns".to_string());
    let err = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect_err("scripted add_ns failure must fail the share");
    assert!(
        err.to_string().contains("scripted failure"),
        "the RPC error surfaces: {err}"
    );
    let rec = r.ledger.find(NQN_A).unwrap().expect("intent record kept");
    assert_eq!(
        rec.state,
        ShareState::Pending,
        "the crash-window record still claims the partial objects"
    );
    {
        let st = r.server.state();
        assert_eq!(st.subsystems.len(), 1, "partial subsystem exists");
        assert_eq!(st.bdevs.len(), 1, "partial bdev exists");
    }

    // Reconcile: restore GCs the pending intent and sweeps the residue.
    r.server.state().fail_methods.clear();
    let report = r.stack.restore(&[rec]).expect("restore runs");
    assert_eq!(report.entries.len(), 1);
    assert_eq!(
        report.entries[0].outcome,
        RestoreOutcome::GarbageCollectedPending,
        "an interrupted share that never returned success is GC'd, never finalized"
    );
    assert!(r.ledger.find(NQN_A).unwrap().is_none(), "record gone");
    let st = r.server.state();
    assert!(st.subsystems.is_empty(), "partial subsystem swept");
    assert!(st.bdevs.is_empty(), "partial bdev swept");
}

// ---------------------------------------------------------------------------
// duplicate guard — the triple source (§6.4)
// ---------------------------------------------------------------------------

/// Ledger source: a recorded share of the same backing (ANY stack)
/// refuses before a single mutating RPC is sent.
#[test]
fn test_spdk_share_duplicate_guard_ledger_source_zero_mutations() {
    let r = rig();
    // Seed an nvmet-stack record claiming the backing (cross-stack half
    // of the ledger guard).
    let holder = ShareRecord {
        subnqn: NQN_B.to_string(),
        stack: StackKind::Nvmet,
        state: ShareState::Pending,
        backing_path: "/dev/null".to_string(),
        backing_canonical: "/dev/null".to_string(),
        nsid: None,
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: Some(53000),
        }],
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: "2026-07-17T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    r.ledger.begin_share(&holder).expect("seed holder");

    let err = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4421)]))
        .expect_err("same backing must refuse");
    let text = err.to_string();
    assert!(
        text.contains(NQN_B) && text.contains("unshare"),
        "refusal names the ledgered holder + the unshare exit: {text}"
    );
    assert!(
        !r.server
            .methods()
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "a refused share must send ZERO mutating RPCs: {:?}",
        r.server.methods()
    );
}

/// Live `bdev_get_bdevs` filename-scan source (the one duplicate check
/// the old module got right — kept): an UNLEDGERED aio bdev already
/// opening the backing refuses loud, naming the bdev + the rpc.py
/// removal step, with zero mutations.
#[test]
fn test_spdk_share_duplicate_guard_live_bdev_filename_scan() {
    let r = rig();
    {
        let mut st = r.server.state();
        st.bdevs
            .push(("foreign_aio".to_string(), Some("/dev/null".to_string())));
        // A non-aio bdev must never trip the filename scan.
        st.bdevs.push(("Malloc0".to_string(), None));
    }
    let err = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect_err("live foreign bdev must refuse");
    let text = err.to_string();
    assert!(
        text.contains("foreign_aio"),
        "refusal names the holding bdev: {text}"
    );
    assert!(
        text.contains("bdev_aio_delete") && text.contains("rpc.py"),
        "the refusal IS the runbook — manual rpc.py removal steps: {text}"
    );
    assert!(
        !r.server
            .methods()
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "zero mutations on refusal"
    );
}

/// Live subsystem source: a foreign live SPDK subsystem serving the
/// backing refuses naming NQN + classification + removal steps.
#[test]
fn test_spdk_share_duplicate_guard_live_subsystem_backing() {
    let r = rig();
    r.server.seed_share(
        "nqn.2026-06.io.foreign:sub1",
        "foreign_bdev",
        "/dev/null",
        1,
        UUID_B,
        None,
        &[("127.0.0.1", 4409)],
    );
    let err = r
        .stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect_err("live foreign subsystem must refuse");
    let text = err.to_string();
    assert!(
        text.contains("nqn.2026-06.io.foreign:sub1"),
        "names the holder NQN: {text}"
    );
    assert!(
        text.contains("foreign"),
        "names the classification (unledgered = foreign): {text}"
    );
    assert!(
        text.contains("nvmf_delete_subsystem"),
        "manual removal steps present: {text}"
    );
}

/// The cross-stack walk (`cross_stack_duplicate_guard`, wired by the
/// verb layer per the G3 module-graph rule): an nvmet-live holder
/// refuses an SPDK share of the same backing and vice versa; the
/// refusal names holder + stack + classification + per-stack steps.
#[test]
fn test_cross_stack_duplicate_guard_names_holder_and_steps() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path());
    let nvmet_holder = LiveShare {
        subnqn: "nqn.2026-06.io.foreign:nvmet-holder".to_string(),
        device_path: "/dev/zram9".to_string(),
        backing_canonical: "/dev/zram9".to_string(),
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![],
        enabled: true,
    };

    // SPDK share (other stack = nvmet) of the nvmet-served backing.
    let request = req(NQN_A, "/dev/zram9", UUID_A, &[("127.0.0.1", 4420)]);
    let err = cross_stack_duplicate_guard(
        &request,
        StackKind::Nvmet,
        std::slice::from_ref(&nvmet_holder),
        &ledger,
    )
    .expect_err("nvmet-live backing must refuse the spdk share");
    let text = err.to_string();
    assert!(
        text.contains("nqn.2026-06.io.foreign:nvmet-holder") && text.contains("nvmet"),
        "names holder + stack: {text}"
    );
    assert!(text.contains("foreign"), "classification: {text}");
    assert!(
        text.contains("rmdir") && text.contains("subsystems"),
        "nvmet holders get the configfs removal steps: {text}"
    );
    assert!(
        text.contains("never be double-served"),
        "states the law: {text}"
    );

    // nvmet share (other stack = spdk) of an SPDK-served backing.
    let spdk_holder = LiveShare {
        subnqn: "nqn.2026-06.io.foreign:spdk-holder".to_string(),
        device_path: "/dev/zram8".to_string(),
        backing_canonical: "/dev/zram8".to_string(),
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![],
        enabled: true,
    };
    let request = req(NQN_A, "/dev/zram8", UUID_A, &[("127.0.0.1", 4420)]);
    let err = cross_stack_duplicate_guard(&request, StackKind::Spdk, &[spdk_holder], &ledger)
        .expect_err("spdk-live backing must refuse the nvmet share");
    let text = err.to_string();
    assert!(
        text.contains("nqn.2026-06.io.foreign:spdk-holder") && text.contains("spdk"),
        "names holder + stack: {text}"
    );
    assert!(
        text.contains("rpc.py") && text.contains("nvmf_delete_subsystem"),
        "spdk holders get the rpc.py removal steps: {text}"
    );

    // A ledgered holder classifies as managed and exits via unshare.
    let managed = ShareRecord {
        subnqn: "nqn.2026-07.io.squeezefs:share-managed".to_string(),
        stack: StackKind::Nvmet,
        state: ShareState::Pending,
        backing_path: "/dev/zram7".to_string(),
        backing_canonical: "/dev/zram7".to_string(),
        nsid: None,
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: Some(53001),
        }],
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: "2026-07-17T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    ledger.begin_share(&managed).expect("seed managed");
    let managed_live = LiveShare {
        subnqn: managed.subnqn.clone(),
        device_path: "/dev/zram7".to_string(),
        backing_canonical: "/dev/zram7".to_string(),
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![],
        enabled: true,
    };
    let request = req(NQN_A, "/dev/zram7", UUID_A, &[("127.0.0.1", 4420)]);
    let err = cross_stack_duplicate_guard(&request, StackKind::Nvmet, &[managed_live], &ledger)
        .expect_err("must refuse");
    let text = err.to_string();
    assert!(
        text.contains("pending") && text.contains("unshare"),
        "ledgered holders classify by intent state and exit via unshare: {text}"
    );

    // Different backing, different NQN: passes.
    let request = req(NQN_A, "/dev/zram1", UUID_A, &[("127.0.0.1", 4420)]);
    cross_stack_duplicate_guard(&request, StackKind::Nvmet, &[nvmet_holder], &ledger)
        .expect("unrelated backing passes the guard");
}

// ---------------------------------------------------------------------------
// unshare
// ---------------------------------------------------------------------------

fn share_one(r: &Rig) -> ShareRecord {
    r.stack
        .share(&req(NQN_A, "/dev/null", UUID_A, &[("127.0.0.1", 4420)]))
        .expect("share fixture")
}

/// Unshare rides the §6.4 law-6 order: `removing` BEFORE the first
/// teardown RPC (witnessed by the fake's ledger snapshots), teardown
/// listener -> subsystem -> bdev, `save_config`, record delete LAST.
#[test]
fn test_spdk_unshare_removing_intent_teardown_save_config_delete() {
    let r = rig();
    let record = share_one(&r);
    let calls_before = r.server.calls().len();

    r.stack.unshare(&record).expect("unshare succeeds");

    let calls: Vec<Call> = r.server.calls().split_off(calls_before);
    for call in calls
        .iter()
        .filter(|c| MUTATING_METHODS.contains(&c.method.as_str()))
    {
        assert_eq!(
            call.ledger
                .iter()
                .find(|(nqn, _)| nqn == NQN_A)
                .map(|(_, s)| s.as_str()),
            Some("removing"),
            "law 6: teardown RPC '{}' arrived without the removing intent (snapshot {:?})",
            call.method,
            call.ledger
        );
    }
    let methods: Vec<String> = calls.iter().map(|c| c.method.clone()).collect();
    let del_sub = method_index(&methods, "nvmf_delete_subsystem").expect("subsystem deleted");
    let del_bdev = method_index(&methods, "bdev_aio_delete").expect("bdev deleted");
    let save = method_index(&methods, "framework_get_subsystems").expect("save_config runs");
    assert!(
        del_sub < del_bdev && del_bdev < save,
        "teardown order subsystem -> bdev -> save_config: {methods:?}"
    );
    if let Some(rm_listener) = method_index(&methods, "nvmf_subsystem_remove_listener") {
        assert!(
            rm_listener < del_sub,
            "listeners drain before the subsystem"
        );
    }

    assert!(r.ledger.find(NQN_A).unwrap().is_none(), "record deleted");
    let st = r.server.state();
    assert!(
        st.subsystems.is_empty() && st.bdevs.is_empty(),
        "zero live residue"
    );
    drop(st);
    let cfg = fs::read_to_string(r.tgt_config()).expect("config saved");
    assert!(
        !cfg.contains(NQN_A),
        "tgt-config.json no longer carries the subsystem (resurrection law): {cfg}"
    );
}

/// Vanished live objects are verified no-ops — the record is still
/// deleted, and `save_config` STILL runs (a stale tgt-config.json entry
/// would otherwise resurrect the share on the next load_config).
#[test]
fn test_spdk_unshare_vanished_objects_verified_noop_still_saves_config() {
    let r = rig();
    let record = share_one(&r);
    // The operator tore the objects down by hand (rpc.py) — vanished.
    {
        let mut st = r.server.state();
        st.subsystems.clear();
        st.bdevs.clear();
    }
    let calls_before = r.server.calls().len();
    r.stack
        .unshare(&record)
        .expect("unshare of vanished objects is a verified no-op");
    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        !methods.iter().any(|m| m == "nvmf_delete_subsystem"
            || m == "bdev_aio_delete"
            || m == "nvmf_subsystem_remove_listener"),
        "vanished objects must not be deleted again: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "framework_get_subsystems"),
        "save_config still runs — the config must stop describing the share: {methods:?}"
    );
    assert!(r.ledger.find(NQN_A).unwrap().is_none(), "record deleted");
    let cfg = fs::read_to_string(r.tgt_config()).expect("config saved");
    assert!(!cfg.contains(NQN_A));
}

/// Design R7: unshare refuses while the subsystem has live initiator
/// connections — naming the NQN and the `--force` override — and the
/// record must stay ACTIVE (a refused unshare never leaves `removing`).
#[test]
fn test_spdk_unshare_live_consumer_refusal_names_nqn_and_force() {
    let r = rig();
    let record = share_one(&r);
    r.server.state().controllers.insert(NQN_A.to_string(), 2);

    let err = r
        .stack
        .unshare(&record)
        .expect_err("live consumers must refuse");
    let text = err.to_string();
    assert!(text.contains(NQN_A), "names the NQN: {text}");
    assert!(text.contains("--force"), "names the override: {text}");
    assert!(
        text.contains("disconnect"),
        "the sequence names the client-side exit: {text}"
    );
    assert_eq!(
        r.ledger.find(NQN_A).unwrap().unwrap().state,
        ShareState::Active,
        "a refused unshare must not leave a removing intent"
    );

    // --force proceeds.
    let forced = r.stack_with(false, true);
    forced
        .unshare(&record)
        .expect("--force overrides the refusal");
    assert!(r.ledger.find(NQN_A).unwrap().is_none());
    assert!(r.server.state().subsystems.is_empty());
}

// ---------------------------------------------------------------------------
// restore — reconciliation + the save_config-after-change law
// ---------------------------------------------------------------------------

/// active + gone => re-shared re-presenting the RECORDED identity (same
/// uuid / nsid / ptpl_file / bdev name / serial — initiators reattach
/// without revalidation trips), ending with save_config.
#[test]
fn test_spdk_restore_reshares_gone_record_with_recorded_identity() {
    let r = rig();
    let record = share_one(&r);
    let calls_share = r.server.calls();
    let serial_at_share = find_call(&calls_share, "nvmf_create_subsystem")
        .unwrap()
        .params
        .as_ref()
        .unwrap()["serial_number"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !serial_at_share.is_empty() && serial_at_share.len() <= 20,
        "serial must fit the NVMe 20-char field: '{serial_at_share}'"
    );

    // Target restarted empty (no tgt-config: the load_config half is
    // exercised separately) — the share is GONE.
    {
        let mut st = r.server.state();
        st.subsystems.clear();
        st.bdevs.clear();
        st.transports.clear();
    }
    fs::remove_file(r.tgt_config()).expect("drop config for the reshare path");
    let calls_before = r.server.calls().len();

    let report = r
        .stack
        .restore(std::slice::from_ref(&record))
        .expect("restore runs");
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].outcome, RestoreOutcome::Restored);

    let calls: Vec<Call> = r.server.calls().split_off(calls_before);
    let add_ns = find_call(&calls, "nvmf_subsystem_add_ns").expect("re-share adds the ns");
    let ns = &add_ns.params.as_ref().unwrap()["namespace"];
    assert_eq!(ns["uuid"], UUID_A, "recorded identity re-presented");
    assert_eq!(ns["nsid"], 1);
    assert_eq!(
        ns["ptpl_file"],
        r.state_dir
            .join("spdk/ptpl")
            .join(format!("{UUID_A}.json"))
            .display()
            .to_string()
            .as_str(),
        "the recorded ptpl_file re-binds PTPL state across the re-create"
    );
    assert_eq!(
        ns["bdev_name"].as_str().unwrap(),
        record.bdev_name.as_deref().unwrap(),
        "recorded bdev name re-presented"
    );
    let serial_at_restore = find_call(&calls, "nvmf_create_subsystem")
        .unwrap()
        .params
        .as_ref()
        .unwrap()["serial_number"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        serial_at_restore, serial_at_share,
        "serial is derived from the recorded identity — stable across restores"
    );
    let methods: Vec<String> = calls.iter().map(|c| c.method.clone()).collect();
    let save = method_index(&methods, "framework_get_subsystems")
        .expect("a restore that changed anything ends with save_config");
    let last_mutation = methods
        .iter()
        .enumerate()
        .filter(|(_, m)| MUTATING_METHODS.contains(&m.as_str()))
        .map(|(i, _)| i)
        .max()
        .unwrap();
    assert!(
        last_mutation < save,
        "save_config after the re-adds: {methods:?}"
    );
    assert!(fs::read_to_string(r.tgt_config()).unwrap().contains(NQN_A));
}

/// A restore whose every record verifies live-and-matching is a no-op
/// pass — and per §6.4 it SKIPS save_config.
#[test]
fn test_spdk_restore_verified_noop_skips_save_config() {
    let r = rig();
    let record = share_one(&r);
    let calls_before = r.server.calls().len();

    let report = r.stack.restore(&[record]).expect("restore runs");
    assert_eq!(report.entries[0].outcome, RestoreOutcome::VerifiedNoop);

    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        !methods.iter().any(|m| m == "framework_get_subsystems"),
        "a no-op restore must skip save_config (§6.4): {methods:?}"
    );
    assert!(
        !methods
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "a no-op restore mutates nothing: {methods:?}"
    );
}

/// pending + live-and-matching => finalized active; and because the
/// share's own save_config may never have run (that is the crash
/// window), the finalization SAVES — otherwise the next load_config
/// drops the share (the share-flap failure mode, rev-2 issue 20).
#[test]
fn test_spdk_restore_finalizes_pending_with_live_match_and_saves() {
    let r = rig();
    let record = share_one(&r);
    // Rewind the ledger to the crash shape: pending again, config stale.
    r.ledger.delete(NQN_A).unwrap();
    let mut pending = record.clone();
    pending.state = ShareState::Pending;
    r.ledger.begin_share(&pending).unwrap();
    fs::remove_file(r.tgt_config()).unwrap();
    let calls_before = r.server.calls().len();

    let report = r.stack.restore(&[pending]).expect("restore runs");
    assert_eq!(report.entries[0].outcome, RestoreOutcome::FinalizedPending);
    assert_eq!(
        r.ledger.find(NQN_A).unwrap().unwrap().state,
        ShareState::Active
    );
    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        methods.iter().any(|m| m == "framework_get_subsystems"),
        "finalization must save (the crash was pre-save_config): {methods:?}"
    );
    assert!(
        fs::read_to_string(r.tgt_config()).unwrap().contains(NQN_A),
        "tgt-config.json heals to describe the finalized share"
    );
}

/// pending + NO live objects => the interrupted share never completed:
/// garbage-collected loudly, record gone.
#[test]
fn test_spdk_restore_gcs_pending_without_live_objects() {
    let r = rig();
    let pending = ShareRecord {
        subnqn: NQN_A.to_string(),
        stack: StackKind::Spdk,
        state: ShareState::Pending,
        backing_path: "/dev/null".to_string(),
        backing_canonical: "/dev/null".to_string(),
        nsid: Some(1),
        ns_uuid: Some(UUID_A.to_string()),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: None,
        }],
        bdev_name: Some("sqz_aio_dead".to_string()),
        ptpl_file: Some(format!("spdk/ptpl/{UUID_A}.json")),
        loop_device: None,
        created_utc: "2026-07-17T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    r.ledger.begin_share(&pending).unwrap();
    let report = r.stack.restore(&[pending]).expect("restore runs");
    assert_eq!(
        report.entries[0].outcome,
        RestoreOutcome::GarbageCollectedPending
    );
    assert!(r.ledger.find(NQN_A).unwrap().is_none(), "record GC'd");
}

/// removing intent => teardown RESUMED and — the §6.4 resurrection law —
/// `save_config` runs: a resumed teardown that is not saved gets its
/// deleted subsystem resurrected by the next load_config.
#[test]
fn test_spdk_restore_resumes_removing_teardown_and_saves_config() {
    let r = rig();
    let record = share_one(&r);
    r.ledger.mark_removing(NQN_A).unwrap();
    let mut removing = record.clone();
    removing.state = ShareState::Removing;
    let calls_before = r.server.calls().len();

    let report = r.stack.restore(&[removing]).expect("restore runs");
    assert_eq!(report.entries[0].outcome, RestoreOutcome::TeardownResumed);
    assert!(r.ledger.find(NQN_A).unwrap().is_none(), "record deleted");
    assert!(r.server.state().subsystems.is_empty(), "teardown resumed");

    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    let save = method_index(&methods, "framework_get_subsystems")
        .expect("resumed teardown MUST save_config (resurrection law)");
    let del = method_index(&methods, "nvmf_delete_subsystem").unwrap();
    assert!(del < save);
    assert!(
        !fs::read_to_string(r.tgt_config()).unwrap().contains(NQN_A),
        "the saved config no longer describes the torn-down share"
    );
}

/// active + live-but-MISMATCHED (a different ns UUID is exactly what
/// initiator revalidation trips on) => loud Failed, never clobbered.
#[test]
fn test_spdk_restore_mismatched_live_fails_loud_never_clobbered() {
    let r = rig();
    let record = share_one(&r);
    // Mutate the live identity out from under the record.
    r.server.state().subsystems[0].namespaces[0].uuid = UUID_B.to_string();
    let calls_before = r.server.calls().len();

    let report = r.stack.restore(&[record]).expect("restore runs");
    match &report.entries[0].outcome {
        RestoreOutcome::Failed(why) => {
            assert!(
                why.contains(UUID_B) || why.contains("mismatch") || why.contains("MISMATCH"),
                "failure names the identity mismatch: {why}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        !methods
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "never clobbered: {methods:?}"
    );
    assert!(
        r.ledger.find(NQN_A).unwrap().is_some(),
        "record kept for manual resolve"
    );
}

/// The systemd ExecStartPost half (§6.5): restore against an EMPTY
/// target with a tgt-config.json present replays it via load_config
/// (rpc_get_methods-gated), then reconciles the records against the
/// now-live state as verified no-ops.
#[test]
fn test_spdk_restore_load_config_replays_onto_empty_target() {
    let r = rig();
    let record = share_one(&r);
    // Simulate the target bounce: state gone, config intact.
    {
        let mut st = r.server.state();
        st.subsystems.clear();
        st.bdevs.clear();
        st.transports.clear();
    }
    let calls_before = r.server.calls().len();

    let report = r.stack.restore(&[record]).expect("restore runs");
    assert_eq!(
        report.entries[0].outcome,
        RestoreOutcome::VerifiedNoop,
        "after load_config the record's objects are live again"
    );
    let methods: Vec<String> = r
        .server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        methods.iter().any(|m| m == "rpc_get_methods"),
        "load_config replay is rpc_get_methods-gated: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "bdev_aio_create")
            && methods.iter().any(|m| m == "nvmf_create_subsystem"),
        "the saved config replayed: {methods:?}"
    );
    let st = r.server.state();
    assert_eq!(st.subsystems.len(), 1, "share live again");
    assert_eq!(st.subsystems[0].nqn, NQN_A);
}

// ---------------------------------------------------------------------------
// preflight ladder (§6.2) + tolerant list gather
// ---------------------------------------------------------------------------

/// Rung 4 on the mutating verbs: version drift refuses without
/// `--accept-version-drift` and proceeds (loudly) with it.
#[test]
fn test_spdk_preflight_drift_gate_on_mutating_verbs() {
    let r = rig_with_version("SPDK v27.01");
    let err = r
        .stack
        .preflight(PreflightOp::Share)
        .expect_err("drift must refuse the mutating preflight");
    let text = err.to_string();
    assert!(
        text.contains("v26.05") && text.contains("SPDK v27.01"),
        "names pin + reported: {text}"
    );
    assert!(
        text.contains("--accept-version-drift"),
        "names the flag: {text}"
    );

    let accepting = r.stack_with(true, false);
    accepting
        .preflight(PreflightOp::Share)
        .expect("--accept-version-drift proceeds");
}

/// Rung 2: no running target (no pidfile, no socket-serving process)
/// refuses with the designed check/start runbook.
#[test]
fn test_spdk_preflight_requires_running_target() {
    let r = rig();
    fs::remove_file(r._dir.path().join("run").join("spdk_tgt.pid")).unwrap();
    let err = r
        .stack
        .preflight(PreflightOp::Share)
        .expect_err("no running target must refuse");
    let text = err.to_string();
    assert!(
        text.contains("target start") && text.contains("target status"),
        "remediation names the lifecycle verbs: {text}"
    );
    assert!(
        text.contains("never falls back between target stacks"),
        "the no-fallback law: {text}"
    );
}

/// The tolerant gather for `list`/cross-stack-guard: a dead RPC socket
/// degrades to empty WITH the note — never a hard failure, never a
/// silent hole.
#[test]
fn test_spdk_live_shares_tolerant_dead_rpc_is_empty_with_note() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let paths = SpdkPaths::with_roots(
        dir.path().join("install"),
        dir.path().join("run"), // no socket bound here
        &state_dir,
    );
    let stack = SpdkStack::new(paths, Ledger::new(&state_dir), false, false);
    let (live, note) = stack.live_shares_tolerant();
    assert!(live.is_empty());
    let note = note.expect("a dead target must be noted loudly");
    assert!(
        note.contains("not answering") || note.contains("not running"),
        "the note names the condition: {note}"
    );

    // …and the strict trait walk refuses instead.
    let err = stack.live_shares().expect_err("strict walk fails loud");
    assert!(matches!(err, NvmeofError::Refused(_) | NvmeofError::Io(_)));
}

/// The live walk itself: Discovery filtered, listeners + uuid + backing
/// carried, non-aio bdevs yield no backing path.
#[test]
fn test_spdk_live_shares_walk_shape() {
    let r = rig();
    r.server.seed_share(
        NQN_B,
        "some_aio",
        "/dev/null",
        1,
        UUID_B,
        Some("/x/ptpl.json"),
        &[("10.0.0.1", 4420)],
    );
    let live = r.stack.live_shares().expect("walk");
    assert_eq!(live.len(), 1, "Discovery subsystem filtered out");
    let l = &live[0];
    assert_eq!(l.subnqn, NQN_B);
    assert_eq!(l.device_path, "/dev/null");
    assert_eq!(l.ns_uuid.as_deref(), Some(UUID_B));
    assert!(l.enabled);
    assert_eq!(l.listeners.len(), 1);
    assert_eq!(l.listeners[0].ip, "10.0.0.1");
    assert_eq!(l.listeners[0].port, 4420);
    assert_eq!(l.listeners[0].nvmet_port_id, None);
}
