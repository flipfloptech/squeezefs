//! `nvmeof adopt <subnqn>` contract tests
//! (`docs/design-nvmeof-target-management.md` §6.10, PR 4b/N4b) —
//! foreign-share absorption into the ledger, driven through the same
//! §6.8 zero-mock seams as the stack suites: an **injected configfs
//! root** for the nvmet arm and an **in-process stateful fake spdk_tgt
//! on a real `UnixListener`** for the SPDK arm (real socket, real
//! JSON-RPC framing, no env behavior forks). Pinned here:
//!
//! * **classification + the six named refusal classes** against
//!   injected live-state snapshots (`adopt_not_live` /
//!   `adopt_ambiguous` — `--target-stack` disambiguates /
//!   `adopt_already_ledgered` (NQN **or** backing, any intent state) /
//!   `adopt_backing_duplicated` (the §6.4 duplicate-guard laws applied
//!   verbatim, incl. the SPDK bare-bdev filename scan) /
//!   `adopt_harness_owned` (devsub-/fideli-/spdkscope prefixes + port
//!   ids 52026/52470/52471 — never absorb the test fabric) /
//!   `adopt_shape_unsupported` (nvmet ns index ≠ 1 / multi-ns, SPDK
//!   multi-ns / non-`bdev_aio`, unmaterialized shells));
//! * **candidate shape**: the live object read into a `pending` record
//!   with `adopted_from` provenance (class heuristic: product `share-`
//!   prefix unledgered ⇒ ledger-loss, pre-rebuild default prefixes ⇒
//!   pre-rebuild, else foreign), identity recorded with **loud nulls**
//!   (missing `device_uuid` / `ptpl_file` ⇒ null + re-share note),
//!   out-of-range nvmet port ids **recorded as-is** under the
//!   link-free teardown law, allow-hosts captured so a restored
//!   adopted share never widens to allow-any;
//! * **absorption via the intent protocol** (§6.4 law 6): `pending`
//!   before anything else, TOCTOU re-verify against a fresh live probe
//!   (drift = loud abort + the pending intent garbage-collected), SPDK
//!   truth-capture `save_config` BEFORE finalize (the ledger snapshot
//!   at the save composition is the witness), finalize `active`;
//! * **zero target mutation**: the fake server's call log carries no
//!   mutating RPC across an adopt, and the injected configfs tree
//!   snapshots byte-identical (shape + content + symlink targets);
//! * **adopted shares are fully managed**: `restore` verifies them as
//!   no-ops, `unshare` tears them down cleanly — including the
//!   out-of-range port-id removal (link-free only);
//! * the §6.4 duplicate-guard refusal messages now name `adopt` beside
//!   the manual removal steps (foreign holders on both stacks + the
//!   cross-stack walk).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::nvmet::{NvmetStack, NVMET_PORT_ID_BASE_DEFAULT};
use squeezefs::nvmeof::spdk::{SpdkPaths, SpdkStack};
use squeezefs::nvmeof::stack::{
    AdoptClass, Listener, LiveShare, ShareRecord, ShareRequest, ShareState, TargetStack,
};
use squeezefs::nvmeof::{
    adopt_candidate, adopt_class_of, adopt_over, adopt_verify_unchanged,
    cross_stack_duplicate_guard, AdoptProbe, StackKind, HARNESS_NQN_MARKERS,
    HARNESS_NVMET_PORT_IDS,
};

const UUID_A: &str = "e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1";
const UUID_B: &str = "0f0e0d0c-0b0a-4a09-8807-060504030201";
const NQN_FOREIGN: &str = "nqn.2026-06.io.foreign:handbuilt-1";
const NQN_PRE_REBUILD: &str = "nqn.2026-06.io.squeezefs:subsystem-0af1";
const NQN_LEDGER_LOSS: &str = "nqn.2026-07.io.squeezefs:share-lost01";

/// Every RPC method that mutates target state (the zero-mutation
/// boundary: an adopt must send NONE of these — the same list the SPDK
/// stack suite pins for the law-6 ordering).
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
// compact stateful fake spdk_tgt (real UnixListener; per-call ledger
// snapshots; read-heavy — adopt must never call the mutating half, but
// it exists so unshare-of-adopted can be proven end-to-end)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct FakeNamespace {
    nsid: u64,
    bdev_name: String,
    uuid: String,
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
    controllers: HashMap<String, usize>,
    /// TOCTOU drift hook: once `nvmf_get_subsystems` has been served
    /// `after` times, the first namespace's uuid flips to the string —
    /// the state change lands exactly between adopt's classification
    /// probe and its re-verify probe.
    flip_uuid_after_gets: Option<(u32, String)>,
    gets_served: u32,
}

#[derive(Debug, Clone)]
struct Call {
    method: String,
    /// The share ledger AT ARRIVAL (subnqn -> intent state) — the law-6
    /// ordering witness.
    ledger: Vec<(String, String)>,
}

struct FakeTgt {
    state: Arc<Mutex<TargetState>>,
    calls: Arc<Mutex<Vec<Call>>>,
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
    fn start(socket: &Path, state_dir: PathBuf) -> FakeTgt {
        let listener = UnixListener::bind(socket).expect("bind fake spdk_tgt socket");
        let state: Arc<Mutex<TargetState>> = Arc::new(Mutex::new(TargetState::default()));
        let calls: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
        let (state_t, calls_t) = (Arc::clone(&state), Arc::clone(&calls));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                serve_one(stream, &state_t, &calls_t, &state_dir);
            }
        });
        FakeTgt { state, calls }
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

    #[allow(clippy::too_many_arguments)]
    fn seed_share(
        &self,
        nqn: &str,
        bdev: &str,
        filename: &str,
        nsid: u64,
        uuid: &str,
        listeners: &[(&str, u16)],
        hosts: &[&str],
    ) {
        let mut st = self.state();
        st.bdevs
            .push((bdev.to_string(), Some(filename.to_string())));
        st.subsystems.push(FakeSubsystem {
            nqn: nqn.to_string(),
            allow_any_host: hosts.is_empty(),
            serial_number: "FAKESERIAL".to_string(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            namespaces: vec![FakeNamespace {
                nsid,
                bdev_name: bdev.to_string(),
                uuid: uuid.to_string(),
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
        ledger: snapshot_ledger(state_dir),
    });
    let mut st = state.lock().unwrap();
    let response = dispatch(&mut st, &method, params.as_ref(), id);
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

fn dispatch(st: &mut TargetState, method: &str, params: Option<&Value>, id: u64) -> Value {
    match method {
        "spdk_get_version" => rpc_result(id, json!({"version": "SPDK v26.05"})),
        "nvmf_get_transports" => rpc_result(
            id,
            json!(st
                .transports
                .iter()
                .map(|t| json!({"trtype": t}))
                .collect::<Vec<_>>()),
        ),
        "bdev_get_bdevs" => {
            let rows: Vec<Value> = st
                .bdevs
                .iter()
                .map(|(n, f)| match f {
                    Some(filename) => json!({
                        "name": n, "block_size": 4096,
                        "driver_specific": {"aio": {"filename": filename}}
                    }),
                    None => json!({"name": n, "block_size": 512, "driver_specific": {}}),
                })
                .collect();
            rpc_result(id, json!(rows))
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
        "nvmf_delete_subsystem" => {
            let nqn = p_str(params, "nqn");
            let before = st.subsystems.len();
            st.subsystems.retain(|s| s.nqn != nqn);
            if st.subsystems.len() == before {
                return rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}"));
            }
            rpc_result(id, json!(true))
        }
        "nvmf_subsystem_remove_listener" => {
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
                    sub.listeners
                        .retain(|(a, s)| !(a == &traddr && s == &trsvcid));
                    rpc_result(id, json!(true))
                }
                None => rpc_error(id, -32602, &format!("Unable to find subsystem {nqn}")),
            }
        }
        "nvmf_get_subsystems" => {
            st.gets_served += 1;
            if let Some((after, new_uuid)) = st.flip_uuid_after_gets.clone() {
                if st.gets_served > after {
                    if let Some(sub) = st.subsystems.first_mut() {
                        if let Some(ns) = sub.namespaces.first_mut() {
                            ns.uuid = new_uuid;
                        }
                    }
                }
            }
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
                                    "uuid": ns.uuid}}}));
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
// rig: injected configfs root + relocated state dir + (optional) fake tgt
// ---------------------------------------------------------------------------

struct Rig {
    _dir: tempfile::TempDir,
    nvmet_root: PathBuf,
    state_dir: PathBuf,
    nvmet: NvmetStack,
    spdk: SpdkStack,
    server: Option<FakeTgt>,
    ledger: Ledger,
}

fn rig_build(with_fake_tgt: bool) -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let nvmet_root = dir.path().join("nvmet");
    fs::create_dir_all(nvmet_root.join("subsystems")).expect("mkdir subsystems");
    fs::create_dir_all(nvmet_root.join("ports")).expect("mkdir ports");
    fs::create_dir_all(nvmet_root.join("hosts")).expect("mkdir hosts");
    let run = dir.path().join("run");
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&run).expect("run dir");
    fs::create_dir_all(&state_dir).expect("state dir");
    let server = with_fake_tgt.then(|| FakeTgt::start(&run.join("spdk.sock"), state_dir.clone()));
    let nvmet = NvmetStack::new(
        nvmet_root.clone(),
        Ledger::new(&state_dir),
        NVMET_PORT_ID_BASE_DEFAULT,
    );
    let spdk = SpdkStack::new(
        SpdkPaths::with_roots(dir.path().join("install"), &run, &state_dir),
        Ledger::new(&state_dir),
        false,
        false,
    );
    Rig {
        nvmet_root,
        state_dir: state_dir.clone(),
        nvmet,
        spdk,
        server,
        ledger: Ledger::new(&state_dir),
        _dir: dir,
    }
}

fn rig() -> Rig {
    rig_build(true)
}

fn rig_dead_spdk() -> Rig {
    rig_build(false)
}

/// Hand-build a live nvmet subsystem in the injected root — the shape a
/// pre-rebuild binary or another tenant leaves behind (namespace index 1
/// unless overridden, real attr files, port object + subsystem link).
#[allow(clippy::too_many_arguments)]
fn plant_nvmet_subsystem(
    root: &Path,
    nqn: &str,
    device: &str,
    uuid: Option<&str>,
    ns_indexes: &[u32],
    port_id: Option<u32>,
    listener: (&str, u16),
    allow_hosts: &[&str],
) {
    let sub = root.join("subsystems").join(nqn);
    for idx in ns_indexes {
        let ns = sub.join("namespaces").join(idx.to_string());
        fs::create_dir_all(&ns).expect("plant ns");
        fs::write(ns.join("device_path"), device).expect("plant device_path");
        if let Some(u) = uuid {
            fs::write(ns.join("device_uuid"), u).expect("plant device_uuid");
        }
        fs::write(ns.join("enable"), "1").expect("plant enable");
    }
    if ns_indexes.is_empty() {
        fs::create_dir_all(&sub).expect("plant bare subsystem");
    }
    fs::write(
        sub.join("attr_allow_any_host"),
        if allow_hosts.is_empty() { "1" } else { "0" },
    )
    .expect("plant allow_any");
    for host in allow_hosts {
        let host_obj = root.join("hosts").join(host);
        fs::create_dir_all(&host_obj).expect("plant host");
        let links = sub.join("allowed_hosts");
        fs::create_dir_all(&links).expect("plant allowed_hosts");
        std::os::unix::fs::symlink(&host_obj, links.join(host)).expect("plant host link");
    }
    if let Some(id) = port_id {
        let p = root.join("ports").join(id.to_string());
        fs::create_dir_all(p.join("subsystems")).expect("plant port");
        fs::write(p.join("addr_trtype"), "tcp").expect("trtype");
        fs::write(p.join("addr_adrfam"), "ipv4").expect("adrfam");
        fs::write(p.join("addr_traddr"), listener.0).expect("traddr");
        fs::write(p.join("addr_trsvcid"), listener.1.to_string()).expect("trsvcid");
        std::os::unix::fs::symlink(&sub, p.join("subsystems").join(nqn)).expect("port link");
    }
}

/// Full-tree snapshot (relative path, kind, content / symlink target) —
/// the unit-tier zero-mutation witness for the injected configfs root.
fn tree_snapshot(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path)
                    .expect("readlink")
                    .display()
                    .to_string();
                out.push(format!("L {rel} -> {target}"));
            } else if meta.is_dir() {
                out.push(format!("D {rel}"));
                walk(base, &path, out);
            } else {
                let content = fs::read_to_string(&path).unwrap_or_default();
                out.push(format!("F {rel} = {content}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn live(
    subnqn: &str,
    device: &str,
    uuid: Option<&str>,
    listeners: &[(&str, u16, Option<u32>)],
) -> LiveShare {
    LiveShare {
        subnqn: subnqn.to_string(),
        device_path: device.to_string(),
        backing_canonical: device.to_string(),
        ns_uuid: uuid.map(str::to_string),
        listeners: listeners
            .iter()
            .map(|(ip, port, id)| Listener {
                ip: ip.to_string(),
                port: *port,
                nvmet_port_id: *id,
            })
            .collect(),
        enabled: true,
        nsids: vec![1],
        bdev_name: None,
        allow_hosts: Vec::new(),
    }
}

fn probes(nvmet_live: Vec<LiveShare>, spdk_live: Vec<LiveShare>) -> Vec<AdoptProbe> {
    vec![
        AdoptProbe {
            kind: StackKind::Nvmet,
            live: nvmet_live,
            aio_bdevs: Vec::new(),
            note: None,
        },
        AdoptProbe {
            kind: StackKind::Spdk,
            live: spdk_live,
            aio_bdevs: Vec::new(),
            note: None,
        },
    ]
}

fn tmp_ledger() -> (tempfile::TempDir, Ledger) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path());
    (dir, ledger)
}

fn seed_record(ledger: &Ledger, subnqn: &str, backing: &str, state: ShareState) {
    let record = ShareRecord {
        subnqn: subnqn.to_string(),
        stack: StackKind::Nvmet,
        state: ShareState::Pending,
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
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
        created_utc: "2026-07-18T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    ledger.begin_share(&record).expect("seed record");
    match state {
        ShareState::Pending => {}
        ShareState::Active => ledger.finalize_share(subnqn).expect("finalize"),
        ShareState::Removing => ledger.mark_removing(subnqn).expect("mark removing"),
    }
}

// ---------------------------------------------------------------------------
// provenance-class heuristic
// ---------------------------------------------------------------------------

/// §6.10 pt 3: pre-rebuild default prefixes ⇒ pre-rebuild; the product's
/// own N2+ ownership prefix unledgered ⇒ ledger-loss; anything else ⇒
/// foreign.
#[test]
fn test_adopt_class_heuristic_pre_rebuild_ledger_loss_foreign() {
    assert_eq!(adopt_class_of(NQN_PRE_REBUILD), AdoptClass::PreRebuild);
    assert_eq!(
        adopt_class_of("nqn.2026-06.io.squeezefs:spdk-subsystem-9d"),
        AdoptClass::PreRebuild
    );
    assert_eq!(adopt_class_of(NQN_LEDGER_LOSS), AdoptClass::LedgerLoss);
    assert_eq!(adopt_class_of(NQN_FOREIGN), AdoptClass::Foreign);
    assert_eq!(
        adopt_class_of("nqn.2014-08.org.nvmexpress:some-operator-thing"),
        AdoptClass::Foreign
    );
}

// ---------------------------------------------------------------------------
// the six named refusal classes (§6.10 pt 2)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_not_live_refusal_names_class_and_list_guidance() {
    let (_dir, ledger) = tmp_ledger();
    let err = adopt_candidate(NQN_FOREIGN, None, &probes(vec![], vec![]), &ledger)
        .expect_err("nothing live must refuse");
    let text = err.to_string();
    assert!(text.contains("adopt_not_live"), "names the class: {text}");
    assert!(text.contains(NQN_FOREIGN), "names the NQN: {text}");
    assert!(
        text.contains("nvmeof list"),
        "points at the reconciliation view: {text}"
    );

    // A dead-SPDK-target probe note is woven in — never a silent hole.
    let mut ps = probes(vec![], vec![]);
    ps[1].note = Some("SPDK target RPC socket not answering".to_string());
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("still nothing live");
    let text = err.to_string();
    assert!(
        text.contains("not answering"),
        "the tolerant-gather note surfaces in the refusal: {text}"
    );
}

#[test]
fn test_adopt_ambiguous_refusal_names_both_holders_and_disambiguator() {
    let (_dir, ledger) = tmp_ledger();
    let ps = probes(
        vec![live(
            NQN_FOREIGN,
            "/dev/zram11",
            Some(UUID_A),
            &[("127.0.0.1", 4420, Some(4))],
        )],
        vec![live(
            NQN_FOREIGN,
            "/dev/zram12",
            Some(UUID_B),
            &[("127.0.0.1", 4421, None)],
        )],
    );
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger)
        .expect_err("both stacks live must fail closed");
    let text = err.to_string();
    assert!(text.contains("adopt_ambiguous"), "names the class: {text}");
    assert!(
        text.contains("nvmet") && text.contains("spdk"),
        "names both holders' stacks: {text}"
    );
    assert!(
        text.contains("/dev/zram11") && text.contains("/dev/zram12"),
        "names both holders' backings: {text}"
    );
    assert!(
        text.contains("--target-stack"),
        "names the disambiguator: {text}"
    );

    // The flag disambiguates: only the named stack participates in
    // locate, so the nvmet-side holder is adopted.
    let (candidate, _notes) = adopt_candidate(NQN_FOREIGN, Some(StackKind::Nvmet), &ps, &ledger)
        .expect("--target-stack nvmet disambiguates");
    assert_eq!(candidate.stack, StackKind::Nvmet);
    assert_eq!(candidate.backing_canonical, "/dev/zram11");
    assert_eq!(candidate.state, ShareState::Pending);
}

#[test]
fn test_adopt_not_live_respects_target_stack_filter() {
    let (_dir, ledger) = tmp_ledger();
    // Live on spdk only; the operator filters to nvmet — not live THERE.
    let ps = probes(
        vec![],
        vec![live(NQN_FOREIGN, "/dev/zram12", Some(UUID_B), &[])],
    );
    let err = adopt_candidate(NQN_FOREIGN, Some(StackKind::Nvmet), &ps, &ledger)
        .expect_err("flag filters locate");
    let text = err.to_string();
    assert!(text.contains("adopt_not_live"), "names the class: {text}");
    assert!(
        text.contains("--target-stack"),
        "explains the filter (drop the flag to auto-detect): {text}"
    );
}

#[test]
fn test_adopt_already_ledgered_refusal_any_state_nqn_or_backing() {
    // (a) the NQN itself is ledgered (active).
    let (_dir, ledger) = tmp_ledger();
    seed_record(&ledger, NQN_FOREIGN, "/dev/zram13", ShareState::Active);
    let ps = probes(
        vec![live(NQN_FOREIGN, "/dev/zram13", Some(UUID_A), &[])],
        vec![],
    );
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger)
        .expect_err("a ledgered NQN is never adopt territory");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered"),
        "names the class: {text}"
    );
    assert!(text.contains("active"), "names the record state: {text}");

    // (b) a pending intent record (crash window) — restore territory.
    let (_dir2, ledger2) = tmp_ledger();
    seed_record(&ledger2, NQN_FOREIGN, "/dev/zram13", ShareState::Pending);
    let ps = probes(
        vec![live(NQN_FOREIGN, "/dev/zram13", Some(UUID_A), &[])],
        vec![],
    );
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger2)
        .expect_err("pending intents belong to restore");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered") && text.contains("pending"),
        "names class + intent state: {text}"
    );
    assert!(
        text.contains("restore"),
        "the crash-window remediation is restore: {text}"
    );

    // (c) the BACKING is ledgered under another NQN.
    let (_dir3, ledger3) = tmp_ledger();
    seed_record(
        &ledger3,
        "nqn.2026-07.io.squeezefs:share-other",
        "/dev/zram14",
        ShareState::Active,
    );
    let ps = probes(
        vec![live(NQN_FOREIGN, "/dev/zram14", Some(UUID_A), &[])],
        vec![],
    );
    let err =
        adopt_candidate(NQN_FOREIGN, None, &ps, &ledger3).expect_err("a ledgered backing refuses");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered"),
        "names the class: {text}"
    );
    assert!(
        text.contains("nqn.2026-07.io.squeezefs:share-other"),
        "names the holding record: {text}"
    );
}

#[test]
fn test_adopt_backing_duplicated_refusal_live_other_holder_and_bdev_scan() {
    // (a) another live object (the OTHER stack) serves the same backing.
    let (_dir, ledger) = tmp_ledger();
    let ps = probes(
        vec![live(NQN_FOREIGN, "/dev/zram15", Some(UUID_A), &[])],
        vec![live(
            "nqn.2026-06.io.foreign:spdk-twin",
            "/dev/zram15",
            Some(UUID_B),
            &[],
        )],
    );
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger)
        .expect_err("a double-served backing must refuse");
    let text = err.to_string();
    assert!(
        text.contains("adopt_backing_duplicated"),
        "names the class: {text}"
    );
    assert!(
        text.contains("nqn.2026-06.io.foreign:spdk-twin") && text.contains("spdk"),
        "names the other live holder + stack: {text}"
    );

    // (b) the SPDK bare-bdev filename scan (§6.4 verbatim): an
    // unattached aio bdev already opens the backing.
    let (_dir2, ledger2) = tmp_ledger();
    let mut ps = probes(
        vec![live(NQN_FOREIGN, "/dev/zram16", Some(UUID_A), &[])],
        vec![],
    );
    ps[1].aio_bdevs = vec![("orphan_aio".to_string(), "/dev/zram16".to_string())];
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger2)
        .expect_err("a bare bdev opening the backing must refuse");
    let text = err.to_string();
    assert!(
        text.contains("adopt_backing_duplicated") && text.contains("orphan_aio"),
        "names class + bdev: {text}"
    );

    // (c) the SPDK candidate's OWN serving bdev never self-trips.
    let (_dir3, ledger3) = tmp_ledger();
    let mut own = live(
        NQN_FOREIGN,
        "/dev/zram17",
        Some(UUID_A),
        &[("10.0.0.1", 4420, None)],
    );
    own.bdev_name = Some("its_own_aio".to_string());
    let mut ps = probes(vec![], vec![own]);
    ps[1].aio_bdevs = vec![("its_own_aio".to_string(), "/dev/zram17".to_string())];
    adopt_candidate(NQN_FOREIGN, None, &ps, &ledger3)
        .expect("the candidate's own bdev is not a duplicate");
}

#[test]
fn test_adopt_harness_owned_refusal_prefixes_and_port_ids() {
    let (_dir, ledger) = tmp_ledger();
    // Every known harness NQN marker refuses by name.
    for (marker, nqn) in [
        (":devsub-", "nqn.2026-07.io.squeezefs:devsub-meta"),
        (":fideli-", "nqn.2026-07.io.squeezefs:fideli-a1"),
        ("spdkscope", "nqn.2026-07.io.spdkscope:bench-spdk"),
    ] {
        assert!(
            HARNESS_NQN_MARKERS.contains(&marker),
            "marker {marker} must be a named constant"
        );
        let ps = probes(vec![live(nqn, "/dev/zram18", Some(UUID_A), &[])], vec![]);
        let err = adopt_candidate(nqn, None, &ps, &ledger)
            .expect_err("harness NQNs must never be absorbed");
        let text = err.to_string();
        assert!(
            text.contains("adopt_harness_owned"),
            "names the class for {nqn}: {text}"
        );
        assert!(
            text.contains(marker),
            "names the matched marker for {nqn}: {text}"
        );
        assert!(
            text.contains("teardown"),
            "points at the harness's own teardown: {text}"
        );
    }
    // Harness-reserved nvmet port ids refuse by number, even under a
    // non-harness NQN (a share serving through the test fabric's port
    // objects is the test fabric's).
    for port_id in HARNESS_NVMET_PORT_IDS {
        let ps = probes(
            vec![live(
                NQN_FOREIGN,
                "/dev/zram19",
                Some(UUID_A),
                &[("127.0.0.1", 4420, Some(port_id))],
            )],
            vec![],
        );
        let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger)
            .expect_err("harness port ids must never be absorbed");
        let text = err.to_string();
        assert!(
            text.contains("adopt_harness_owned") && text.contains(&port_id.to_string()),
            "names class + port id {port_id}: {text}"
        );
    }
}

#[test]
fn test_adopt_shape_unsupported_nvmet_index_and_multi_ns() {
    let (_dir, ledger) = tmp_ledger();
    // Namespace index != 1 (the §6.6 structural convention).
    let mut wrong_index = live(NQN_FOREIGN, "/dev/zram20", Some(UUID_A), &[]);
    wrong_index.nsids = vec![2];
    let ps = probes(vec![wrong_index], vec![]);
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("index 2 unsupported");
    let text = err.to_string();
    assert!(
        text.contains("adopt_shape_unsupported"),
        "names the class: {text}"
    );
    assert!(text.contains('2'), "names the observed index: {text}");
    assert!(
        text.contains("re-share"),
        "remediation is removal-first + re-share: {text}"
    );

    // Multi-namespace subsystem.
    let mut multi = live(NQN_FOREIGN, "/dev/zram20", Some(UUID_A), &[]);
    multi.nsids = vec![1, 2];
    let ps = probes(vec![multi], vec![]);
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("multi-ns unsupported");
    assert!(err.to_string().contains("adopt_shape_unsupported"));

    // An unmaterialized shell serves nothing absorbable.
    let mut shell = live(NQN_FOREIGN, "", None, &[]);
    shell.nsids = vec![];
    shell.enabled = false;
    let ps = probes(vec![shell], vec![]);
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("shell unsupported");
    assert!(err.to_string().contains("adopt_shape_unsupported"));
}

#[test]
fn test_adopt_shape_unsupported_spdk_multi_ns_and_non_aio() {
    let (_dir, ledger) = tmp_ledger();
    // Multi-namespace SPDK subsystem.
    let mut multi = live(NQN_FOREIGN, "/dev/zram21", Some(UUID_A), &[]);
    multi.nsids = vec![1, 2];
    multi.bdev_name = Some("aio1".to_string());
    let ps = probes(vec![], vec![multi]);
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("multi-ns unsupported");
    assert!(err.to_string().contains("adopt_shape_unsupported"));

    // Non-bdev_aio namespace: a namespace exists but resolves to no aio
    // filename (e.g. Malloc/NVMe bdev) — nothing recordable as backing.
    let mut non_aio = live(NQN_FOREIGN, "", None, &[]);
    non_aio.nsids = vec![1];
    non_aio.bdev_name = Some("Malloc0".to_string());
    non_aio.enabled = true;
    let ps = probes(vec![], vec![non_aio]);
    let err = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect_err("non-aio unsupported");
    let text = err.to_string();
    assert!(
        text.contains("adopt_shape_unsupported") && text.contains("bdev_aio"),
        "names class + the aio-only law: {text}"
    );
}

// ---------------------------------------------------------------------------
// candidate shape (§6.10 pt 1: identity read live, loud nulls,
// out-of-range port ids recorded as-is)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_candidate_nvmet_shape_records_identity_listeners_loop() {
    let (_dir, ledger) = tmp_ledger();
    // A pre-rebuild-style holder: small-int (out-of-range) port id.
    let mut holder = live(
        NQN_PRE_REBUILD,
        "/dev/zram22",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(4))],
    );
    holder.allow_hosts = vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()];
    let ps = probes(vec![holder], vec![]);
    let (candidate, notes) =
        adopt_candidate(NQN_PRE_REBUILD, None, &ps, &ledger).expect("candidate builds");

    assert_eq!(candidate.stack, StackKind::Nvmet);
    assert_eq!(candidate.state, ShareState::Pending, "intent protocol");
    assert_eq!(candidate.backing_path, "/dev/zram22");
    assert_eq!(candidate.backing_canonical, "/dev/zram22");
    assert_eq!(candidate.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(candidate.nsid, None, "nsid never recorded on nvmet");
    assert_eq!(candidate.bdev_name, None);
    assert_eq!(candidate.ptpl_file, None);
    assert_eq!(candidate.loop_device, None);
    assert_eq!(
        candidate.listeners,
        vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: Some(4),
        }],
        "the out-of-range port id is recorded AS-IS"
    );
    assert_eq!(
        candidate.allow_hosts,
        vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()],
        "the live allowlist is captured — a restored adopted share never widens to allow-any"
    );
    let adopted = candidate.adopted_from.as_ref().expect("provenance");
    assert_eq!(adopted.class, AdoptClass::PreRebuild);
    assert!(
        adopted.utc.contains('T') && adopted.utc.ends_with('Z'),
        "RFC3339 provenance timestamp: {}",
        adopted.utc
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("outside the reserved range") && n.contains("link-free")),
        "the out-of-range port id gets a loud recorded-as-is note: {notes:?}"
    );
    candidate.validate().expect("presence rules hold");

    // Loop-served file backing: device_path is the loop node, the
    // canonical resolves to the file — backing_path records the file and
    // loop_device the node (§6.4 law 5: teardown learns it from the
    // ledger).
    let mut loop_holder = live(
        NQN_FOREIGN,
        "/dev/loop7",
        Some(UUID_A),
        &[("127.0.0.1", 4421, Some(53002))],
    );
    loop_holder.backing_canonical = "/srv/backing.img".to_string();
    let ps = probes(vec![loop_holder], vec![]);
    let (candidate, _) = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect("loop candidate");
    assert_eq!(candidate.backing_path, "/srv/backing.img");
    assert_eq!(candidate.loop_device.as_deref(), Some("/dev/loop7"));

    // A live object with NO listener is not representable (the §6.4
    // schema requires >= 1) — shape-unsupported, never a validate panic.
    let bare_no_listener = live(
        "nqn.2026-06.io.foreign:no-listener",
        "/dev/zram23",
        None,
        &[],
    );
    let ps = probes(vec![bare_no_listener], vec![]);
    let err = adopt_candidate("nqn.2026-06.io.foreign:no-listener", None, &ps, &ledger)
        .expect_err("no listener = unsupported shape");
    let text = err.to_string();
    assert!(
        text.contains("adopt_shape_unsupported") && text.contains("listener"),
        "no-listener objects refuse as shape-unsupported: {text}"
    );

    // Missing device_uuid: recorded null with the loud re-share note.
    let bare = live(
        "nqn.2026-06.io.foreign:no-uuid",
        "/dev/zram23",
        None,
        &[("127.0.0.1", 4422, Some(53003))],
    );
    let ps = probes(vec![bare], vec![]);
    let (candidate, notes) = adopt_candidate("nqn.2026-06.io.foreign:no-uuid", None, &ps, &ledger)
        .expect("null identity is recordable");
    assert_eq!(candidate.ns_uuid, None);
    assert!(
        notes
            .iter()
            .any(|n| n.contains("re-share") && (n.contains("identity") || n.contains("uuid"))),
        "null identity gets the loud restart-stability note: {notes:?}"
    );
}

#[test]
fn test_adopt_candidate_spdk_shape_records_identity_and_ptpl_probe() {
    let (dir, ledger) = tmp_ledger();
    let mut holder = live(
        NQN_LEDGER_LOSS,
        "/dev/zram24",
        Some(UUID_A),
        &[("10.0.0.9", 4421, None)],
    );
    holder.nsids = vec![3];
    holder.bdev_name = Some("sqz_aio_e2b1c9a452d1".to_string());
    let ps = probes(vec![], vec![holder.clone()]);

    // No ptpl file on disk: recorded null + the loud upgrade note.
    let (candidate, notes) =
        adopt_candidate(NQN_LEDGER_LOSS, None, &ps, &ledger).expect("candidate builds");
    assert_eq!(candidate.stack, StackKind::Spdk);
    assert_eq!(candidate.nsid, Some(3), "the LIVE nsid is recorded");
    assert_eq!(
        candidate.bdev_name.as_deref(),
        Some("sqz_aio_e2b1c9a452d1"),
        "the live serving bdev is recorded (teardown drives the recorded name)"
    );
    assert_eq!(candidate.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(candidate.ptpl_file, None);
    assert!(
        notes
            .iter()
            .any(|n| n.contains("PTPL") && n.contains("re-share")),
        "the null ptpl_file gets the loud upgrade note: {notes:?}"
    );
    assert_eq!(
        candidate.adopted_from.as_ref().unwrap().class,
        AdoptClass::LedgerLoss,
        "an unledgered product-prefix NQN is the ledger-loss shape"
    );

    // The ledger-loss funnel: the product's own ptpl file still exists
    // under the state dir — adopt re-binds it instead of nulling.
    let ptpl_dir = dir.path().join("spdk").join("ptpl");
    fs::create_dir_all(&ptpl_dir).expect("ptpl dir");
    fs::write(ptpl_dir.join(format!("{UUID_A}.json")), "{}").expect("ptpl file");
    let (candidate, notes) =
        adopt_candidate(NQN_LEDGER_LOSS, None, &ps, &ledger).expect("candidate builds");
    assert_eq!(
        candidate.ptpl_file.as_deref(),
        Some(format!("spdk/ptpl/{UUID_A}.json").as_str()),
        "an existing state-dir ptpl file is re-bound to the adopted record"
    );
    assert!(
        notes.iter().any(|n| n.contains("re-bound")),
        "the re-bind is loud: {notes:?}"
    );
}

// ---------------------------------------------------------------------------
// TOCTOU verify (§6.10 pt 3)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_verify_unchanged_detects_drift() {
    let (_dir, ledger) = tmp_ledger();
    let holder = live(
        NQN_FOREIGN,
        "/dev/zram25",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(4))],
    );
    let ps = probes(vec![holder.clone()], vec![]);
    let (candidate, _) = adopt_candidate(NQN_FOREIGN, None, &ps, &ledger).expect("candidate");

    // Unchanged live state verifies.
    adopt_verify_unchanged(&candidate, std::slice::from_ref(&holder))
        .expect("identical live state must verify");

    // Vanished.
    let err = adopt_verify_unchanged(&candidate, &[]).expect_err("vanished = drift");
    assert!(err.contains("vanished"), "names the drift: {err}");

    // Identity drift.
    let mut drifted = holder.clone();
    drifted.ns_uuid = Some(UUID_B.to_string());
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("uuid drift");
    assert!(err.contains("ns_uuid") || err.contains("uuid"), "{err}");

    // Backing drift.
    let mut drifted = holder.clone();
    drifted.device_path = "/dev/zram26".to_string();
    drifted.backing_canonical = "/dev/zram26".to_string();
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("backing drift");
    assert!(err.contains("backing"), "{err}");

    // Listener drift (a port id moved).
    let mut drifted = holder.clone();
    drifted.listeners[0].nvmet_port_id = Some(5);
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("listener drift");
    assert!(err.contains("listener"), "{err}");

    // Allowlist drift.
    let mut drifted = holder.clone();
    drifted.allow_hosts = vec!["nqn.x:new-host".to_string()];
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("allowlist drift");
    assert!(err.contains("allow"), "{err}");

    // Namespace-shape drift (a second namespace appeared).
    let mut drifted = holder.clone();
    drifted.nsids = vec![1, 2];
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("shape drift");
    assert!(err.contains("namespace"), "{err}");
}

// ---------------------------------------------------------------------------
// walker probes feed adopt (the §6.10 classification rides the SAME
// live walkers list/guards use — extended shape pinned here)
// ---------------------------------------------------------------------------

#[test]
fn test_nvmet_walker_reports_nsids_and_allow_hosts() {
    let r = rig_dead_spdk();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4),
        ("127.0.0.1", 4420),
        &["nqn.2014-08.org.nvmexpress:uuid:h1"],
    );
    let live = r.nvmet.live_shares().expect("walk");
    assert_eq!(live.len(), 1);
    let l = &live[0];
    assert_eq!(l.nsids, vec![1], "namespace indexes enumerated");
    assert_eq!(l.bdev_name, None, "bdev_name is SPDK-only");
    assert_eq!(
        l.allow_hosts,
        vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()],
        "allowed_hosts links surface"
    );
    assert_eq!(l.listeners.len(), 1);
    assert_eq!(l.listeners[0].nvmet_port_id, Some(4));

    // Multi-ns shape surfaces for the shape check.
    plant_nvmet_subsystem(
        &r.nvmet_root,
        "nqn.2026-06.io.foreign:multi",
        "/dev/null2",
        None,
        &[1, 2],
        None,
        ("127.0.0.1", 4421),
        &[],
    );
    let live = r.nvmet.live_shares().expect("walk");
    let multi = live
        .iter()
        .find(|l| l.subnqn == "nqn.2026-06.io.foreign:multi")
        .expect("present");
    assert_eq!(multi.nsids, vec![1, 2]);
}

#[test]
fn test_spdk_walker_reports_nsids_bdev_and_hosts() {
    let r = rig();
    r.server.as_ref().unwrap().seed_share(
        NQN_FOREIGN,
        "foreign_aio",
        "/dev/null",
        3,
        UUID_A,
        &[("10.0.0.1", 4420)],
        &["nqn.2014-08.org.nvmexpress:uuid:h9"],
    );
    let live = r.spdk.live_shares().expect("walk");
    assert_eq!(live.len(), 1);
    let l = &live[0];
    assert_eq!(l.nsids, vec![3], "live namespace ids surface");
    assert_eq!(l.bdev_name.as_deref(), Some("foreign_aio"));
    assert_eq!(
        l.allow_hosts,
        vec!["nqn.2014-08.org.nvmexpress:uuid:h9".to_string()]
    );
}

// ---------------------------------------------------------------------------
// absorption end-to-end (adopt_over): both stacks, zero mutation,
// intent ordering, provenance, managed lifecycle afterwards
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_over_nvmet_absorbs_planted_subsystem_zero_mutation() {
    let r = rig_dead_spdk();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_PRE_REBUILD,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4), // the pre-rebuild small-int port id
        ("127.0.0.1", 4420),
        &[],
    );
    let before = tree_snapshot(&r.nvmet_root);

    let record =
        adopt_over(NQN_PRE_REBUILD, None, &r.ledger, &r.nvmet, &r.spdk).expect("adopt succeeds");

    assert_eq!(record.state, ShareState::Active, "finalized active");
    assert_eq!(record.stack, StackKind::Nvmet);
    assert_eq!(record.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(record.listeners.len(), 1);
    assert_eq!(
        record.listeners[0].nvmet_port_id,
        Some(4),
        "the out-of-range port id rides the record as-is"
    );
    assert_eq!(
        record.adopted_from.as_ref().map(|a| a.class),
        Some(AdoptClass::PreRebuild)
    );

    // Ledger round-trip.
    let loaded = r.ledger.find(NQN_PRE_REBUILD).unwrap().expect("recorded");
    assert_eq!(loaded, record);

    // ZERO target mutation: the configfs tree is byte-identical.
    let after = tree_snapshot(&r.nvmet_root);
    assert_eq!(
        before, after,
        "adopt mutates no target state — the configfs tree must be untouched"
    );
}

#[test]
fn test_adopt_over_nvmet_adopted_share_restore_noop_then_unshare_clean() {
    let r = rig_dead_spdk();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4),
        ("127.0.0.1", 4420),
        &[],
    );
    let record = adopt_over(NQN_FOREIGN, None, &r.ledger, &r.nvmet, &r.spdk).expect("adopt");

    // Fully managed: restore verifies the adopted share as a no-op…
    let report = r
        .nvmet
        .restore(std::slice::from_ref(&record))
        .expect("restore runs");
    assert_eq!(
        report.entries[0].outcome,
        squeezefs::nvmeof::stack::RestoreOutcome::VerifiedNoop,
        "an adopted share is restore-reconcilable like any other"
    );

    // …and unshare tears it down cleanly, INCLUDING the out-of-range
    // port id (recorded ⇒ removed when link-free — the §6.6 law).
    r.nvmet.unshare(&record).expect("unshare adopted share");
    assert!(
        !r.nvmet_root.join("subsystems").join(NQN_FOREIGN).exists(),
        "subsystem gone"
    );
    assert!(
        !r.nvmet_root.join("ports").join("4").exists(),
        "the adopted out-of-range port object is removed once link-free"
    );
    assert!(r.ledger.find(NQN_FOREIGN).unwrap().is_none(), "record gone");
}

#[test]
fn test_adopt_over_spdk_absorbs_foreign_share_zero_mutating_rpcs_saves_config() {
    let r = rig();
    let server = r.server.as_ref().unwrap();
    server.seed_share(
        NQN_FOREIGN,
        "rpcpy_aio",
        "/dev/null",
        1,
        UUID_A,
        &[("127.0.0.1", 4409)],
        &[],
    );

    let record = adopt_over(NQN_FOREIGN, None, &r.ledger, &r.nvmet, &r.spdk).expect("adopt");
    assert_eq!(record.state, ShareState::Active);
    assert_eq!(record.stack, StackKind::Spdk);
    assert_eq!(record.nsid, Some(1));
    assert_eq!(record.bdev_name.as_deref(), Some("rpcpy_aio"));
    assert_eq!(record.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(record.ptpl_file, None, "no state-dir ptpl file ⇒ null");
    assert_eq!(
        record.adopted_from.as_ref().map(|a| a.class),
        Some(AdoptClass::Foreign)
    );

    // ZERO mutating RPCs across the whole adopt.
    let methods = server.methods();
    assert!(
        !methods
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "adopt must never mutate target state: {methods:?}"
    );

    // §6.10 pt 4 truth capture: the save composition ran and
    // tgt-config.json now describes the adopted share.
    assert!(
        methods.iter().any(|m| m == "framework_get_subsystems"),
        "adopt on the SPDK stack ends with save_config: {methods:?}"
    );
    let cfg = fs::read_to_string(r.state_dir.join("spdk").join("tgt-config.json"))
        .expect("tgt-config.json written");
    assert!(
        cfg.contains(NQN_FOREIGN),
        "the SPDK source of truth captures the adopted subsystem: {cfg}"
    );

    // Intent ordering witness: at the save composition's first call the
    // record is still PENDING — active only after the truth capture
    // (the law-6 pattern: finalize after the last verb step).
    let calls = server.calls();
    let save_call = calls
        .iter()
        .find(|c| c.method == "framework_get_subsystems")
        .unwrap();
    assert_eq!(
        save_call
            .ledger
            .iter()
            .find(|(nqn, _)| nqn == NQN_FOREIGN)
            .map(|(_, s)| s.as_str()),
        Some("pending"),
        "finalize must happen only after the truth capture"
    );

    // Fully managed afterwards: restore = verified no-op with NO save
    // (nothing changed), unshare tears down for real.
    let calls_before = server.calls().len();
    let report = r
        .spdk
        .restore(std::slice::from_ref(&record))
        .expect("restore");
    assert_eq!(
        report.entries[0].outcome,
        squeezefs::nvmeof::stack::RestoreOutcome::VerifiedNoop
    );
    let methods: Vec<String> = server
        .calls()
        .split_off(calls_before)
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert!(
        !methods.iter().any(|m| m == "framework_get_subsystems"),
        "a verified-no-op restore of an adopted share skips save_config: {methods:?}"
    );

    r.spdk.unshare(&record).expect("unshare adopted share");
    let st = server.state();
    assert!(st.subsystems.is_empty(), "subsystem torn down");
    assert!(st.bdevs.is_empty(), "bdev torn down (recorded name)");
    drop(st);
    assert!(r.ledger.find(NQN_FOREIGN).unwrap().is_none());
    let cfg = fs::read_to_string(r.state_dir.join("spdk").join("tgt-config.json")).unwrap();
    assert!(
        !cfg.contains(NQN_FOREIGN),
        "tgt-config.json no longer describes the unshared adopted share"
    );
}

#[test]
fn test_adopt_over_spdk_toctou_drift_aborts_loud_and_gcs_pending() {
    let r = rig();
    let server = r.server.as_ref().unwrap();
    server.seed_share(
        NQN_FOREIGN,
        "rpcpy_aio",
        "/dev/null",
        1,
        UUID_A,
        &[("127.0.0.1", 4409)],
        &[],
    );
    // The live identity flips right after the classification probe —
    // the TOCTOU window made real.
    server.state().flip_uuid_after_gets = Some((1, UUID_B.to_string()));

    let err = adopt_over(NQN_FOREIGN, None, &r.ledger, &r.nvmet, &r.spdk)
        .expect_err("drift must abort the adopt");
    let text = err.to_string();
    assert!(
        text.contains(NQN_FOREIGN) && (text.contains("drift") || text.contains("changed")),
        "the abort is loud and names the drift: {text}"
    );
    assert!(
        r.ledger.find(NQN_FOREIGN).unwrap().is_none(),
        "the pending intent is garbage-collected on abort — nothing strands"
    );
    let methods = server.methods();
    assert!(
        !methods
            .iter()
            .any(|m| MUTATING_METHODS.contains(&m.as_str())),
        "an aborted adopt still mutates nothing: {methods:?}"
    );
}

#[test]
fn test_adopt_over_not_live_when_spdk_target_dead() {
    // No fake server bound: the SPDK probe degrades tolerantly (a dead
    // target serves nothing) and the refusal is adopt_not_live with the
    // note surfaced — never a hard failure, never a silent hole.
    let r = rig_dead_spdk();
    let err = adopt_over(NQN_FOREIGN, None, &r.ledger, &r.nvmet, &r.spdk)
        .expect_err("nothing live anywhere");
    let text = err.to_string();
    assert!(text.contains("adopt_not_live"), "names the class: {text}");
    assert!(
        text.contains("not answering") || text.contains("not running"),
        "the dead-target note surfaces: {text}"
    );
}

// ---------------------------------------------------------------------------
// adopted-record schema (§6.4 presence rules + provenance)
// ---------------------------------------------------------------------------

#[test]
fn test_adopted_record_schema_roundtrips_with_provenance() {
    let r = rig_dead_spdk();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_LEDGER_LOSS,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(53017),
        ("127.0.0.1", 4420),
        &[],
    );
    let record = adopt_over(NQN_LEDGER_LOSS, None, &r.ledger, &r.nvmet, &r.spdk).expect("adopt");
    record.validate().expect("presence rules hold");
    let adopted = record.adopted_from.as_ref().expect("provenance present");
    assert_eq!(adopted.class, AdoptClass::LedgerLoss);
    assert!(adopted.utc.contains('T') && adopted.utc.ends_with('Z'));

    // Round-trips byte-faithfully through the ledger (deny_unknown_fields
    // schema — the provenance object is part of format 1).
    let loaded = r.ledger.find(NQN_LEDGER_LOSS).unwrap().expect("loaded");
    assert_eq!(loaded, record);
    let raw: Value =
        serde_json::from_slice(&fs::read(r.ledger.ledger_path()).unwrap()).expect("json");
    let rec = &raw["shares"][0];
    assert_eq!(rec["adopted_from"]["class"], "ledger-loss");
    assert!(rec["adopted_from"]["utc"].is_string());
}

// ---------------------------------------------------------------------------
// the §6.4 duplicate-guard refusals now name adopt beside the manual
// steps (§6.10: "the duplicate-guard refusal message adds adopt")
// ---------------------------------------------------------------------------

#[test]
fn test_duplicate_guard_foreign_refusals_name_adopt() {
    // Cross-stack walk (mod.rs).
    let (_dir, ledger) = tmp_ledger();
    let holder = live(NQN_FOREIGN, "/dev/zram30", Some(UUID_A), &[]);
    let request = ShareRequest {
        subnqn: "nqn.2026-07.io.squeezefs:share-new".to_string(),
        backing_path: "/dev/zram30".to_string(),
        backing_canonical: "/dev/zram30".to_string(),
        nsid: None,
        ns_uuid: UUID_B.to_string(),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: None,
        }],
        allow_hosts: Vec::new(),
    };
    let err = cross_stack_duplicate_guard(&request, StackKind::Nvmet, &[holder], &ledger)
        .expect_err("refuses");
    let text = err.to_string();
    assert!(
        text.contains("nvmeof adopt") && text.contains(NQN_FOREIGN),
        "the cross-stack refusal names adopt beside the manual steps: {text}"
    );

    // nvmet stack guard (foreign live holder on the same stack).
    let r = rig_dead_spdk();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/zram31",
        Some(UUID_A),
        &[1],
        None,
        ("127.0.0.1", 4420),
        &[],
    );
    let err = r
        .nvmet
        .share(&ShareRequest {
            subnqn: "nqn.2026-07.io.squeezefs:share-x1".to_string(),
            backing_path: "/dev/zram31".to_string(),
            backing_canonical: "/dev/zram31".to_string(),
            nsid: None,
            ns_uuid: UUID_B.to_string(),
            listeners: vec![Listener {
                ip: "127.0.0.1".to_string(),
                port: 4430,
                nvmet_port_id: None,
            }],
            allow_hosts: Vec::new(),
        })
        .expect_err("same backing refused");
    let text = err.to_string();
    assert!(
        text.contains("nvmeof adopt"),
        "the nvmet guard refusal names adopt: {text}"
    );

    // spdk stack guard (foreign live subsystem on the same stack).
    let r = rig();
    r.server.as_ref().unwrap().seed_share(
        NQN_FOREIGN,
        "foreign_aio",
        "/dev/zram32",
        1,
        UUID_A,
        &[("127.0.0.1", 4409)],
        &[],
    );
    let err = r
        .spdk
        .share(&ShareRequest {
            subnqn: "nqn.2026-07.io.squeezefs:share-x2".to_string(),
            backing_path: "/dev/zram32".to_string(),
            backing_canonical: "/dev/zram32".to_string(),
            nsid: None,
            ns_uuid: UUID_B.to_string(),
            listeners: vec![Listener {
                ip: "127.0.0.1".to_string(),
                port: 4431,
                nvmet_port_id: None,
            }],
            allow_hosts: Vec::new(),
        })
        .expect_err("same backing refused");
    let text = err.to_string();
    assert!(
        text.contains("nvmeof adopt"),
        "the spdk guard refusal names adopt: {text}"
    );
}
