//! SPDK target-stack management (`docs/design-nvmeof-target-management.md`
//! §6.5/§6.9, PR 3/N3 lifecycle + PR 4/N4 sharing): pinned-release
//! lifecycle (`target install/setup/start/stop/status/systemd-unit`),
//! the JSON-RPC client v2, and — live as of N4 — the **share path**:
//! `SpdkStack` behind the `TargetStack` trait, `save_config`-backed
//! share/unshare/restore with every share pinning `nsid` + ns UUID +
//! `ptpl_file` (PTPL state binds to the namespace UUID, so restore
//! re-presents the recorded identity and reservations survive target
//! power cycles — the scoping §4 pt 1 fix).
//!
//! Module-graph law (program gate G3): this subtree carries **no**
//! `use`/path reference to `super::nvmet` items and vice versa — stack
//! dispatch lives in `super` (`src/nvmeof/mod.rs`) only, so zero
//! cross-stack fallback paths can exist by construction (pinned by
//! `tests/nvmeof_target_lifecycle_tests.rs`).
//!
//! io_uring note: everything here is one-shot admin control plane
//! (unix-socket JSON-RPC, sysfs writes, git/make shell-outs) — the
//! sanctioned `reservation.rs` precedent; no data path is touched.

pub mod hugepages;
pub mod lifecycle;
pub mod rpc;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::ledger::Ledger;
use super::stack::{
    Listener, LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreOutcome, RestoreReport,
    ShareRecord, ShareRequest, ShareState, TargetStack, TargetStatus,
};
use super::StackKind;
use rpc::{SpdkRpcClient, SpdkRpcError, SpdkVersion};

/// Env override substituting the pinned `spdk_tgt` binary for the six
/// target verbs' preflights and `target start` (rig/dev seam — always
/// with the loud unpinned warning; `target install` ignores it: install
/// always builds the pin). §6.2 flag/override placement.
pub const SPDK_TGT_BIN_ENV: &str = "SQUEEZEFS_SPDK_TGT_BIN";

/// Env relocating the runtime dir (§6.4 state homes; a §6.8 relocation
/// seam — the production path pointed at a different location).
pub const RUN_DIR_ENV: &str = "SQUEEZEFS_NVMEOF_RUN_DIR";

/// Production runtime dir: RPC socket + pidfile (tmpfs, reboot-cleared).
/// Never `/var/tmp/spdk.sock` — a world-writable-dir default is a
/// local-root-equivalent control surface (§Security).
pub const DEFAULT_RUN_DIR: &str = "/run/squeezefs/nvmeof";

/// Pinned SPDK builds live under `<root>/<tag>/` — never `/opt/spdk`,
/// never system-wide (§6.4).
pub const DEFAULT_INSTALL_ROOT: &str = "/opt/squeezefs/spdk";

/// RPC socket file name under the run dir (socket 0600, dir 0700).
pub const RPC_SOCK_FILENAME: &str = "spdk.sock";
/// Pidfile name under the run dir (pidfile direct mode, §6.5).
pub const PIDFILE_FILENAME: &str = "spdk_tgt.pid";
/// spdk_tgt output capture under the run dir (pidfile mode) — never
/// `Stdio::null()` (§6.9 Logging).
pub const TGT_LOG_FILENAME: &str = "spdk_tgt.log";

/// The resolved path set every SPDK verb operates on (§6.4 state homes).
/// `resolve()` is the production constructor (env-aware for the
/// sanctioned relocation seams); `with_roots` is the unit-tier injection
/// seam — every code path executed is the production path pointed at a
/// different location (§6.8; zero env behavior forks).
#[derive(Debug, Clone)]
pub struct SpdkPaths {
    /// `/opt/squeezefs/spdk/<tag>` — the pinned build prefix.
    pub install_prefix: PathBuf,
    /// `/run/squeezefs/nvmeof` (or `SQUEEZEFS_NVMEOF_RUN_DIR`).
    pub run_dir: PathBuf,
    /// `/var/lib/squeezefs/nvmeof` (or `SQUEEZEFS_NVMEOF_STATE_DIR`) —
    /// shared with the ledger; SPDK-native state lives under `spdk/`.
    pub state_dir: PathBuf,
    /// `SQUEEZEFS_SPDK_TGT_BIN` (loud unpinned warning when used).
    pub bin_override: Option<PathBuf>,
}

impl SpdkPaths {
    /// Production constructor (env-aware).
    pub fn resolve() -> Self {
        let env_dir = |key: &str, default: &str| -> PathBuf {
            match std::env::var(key) {
                Ok(v) if !v.is_empty() => PathBuf::from(v),
                _ => PathBuf::from(default),
            }
        };
        SpdkPaths {
            install_prefix: PathBuf::from(DEFAULT_INSTALL_ROOT).join(lifecycle::SPDK_PINNED_TAG),
            run_dir: env_dir(RUN_DIR_ENV, DEFAULT_RUN_DIR),
            state_dir: env_dir(
                super::ledger::STATE_DIR_ENV,
                super::ledger::DEFAULT_STATE_DIR,
            ),
            bin_override: match std::env::var(SPDK_TGT_BIN_ENV) {
                Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
                _ => None,
            },
        }
    }

    /// Injection seam for the unit tier.
    pub fn with_roots(
        install_prefix: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        SpdkPaths {
            install_prefix: install_prefix.into(),
            run_dir: run_dir.into(),
            state_dir: state_dir.into(),
            bin_override: None,
        }
    }

    /// The pinned `spdk_tgt` binary: `<prefix>/build/bin/spdk_tgt`
    /// (`install` links `<prefix>/build` → `<prefix>/src/build`, so the
    /// §6.5 systemd-unit path holds verbatim).
    pub fn pinned_bin(&self) -> PathBuf {
        self.install_prefix.join("build/bin/spdk_tgt")
    }

    /// The pinned source checkout: `<prefix>/src`.
    pub fn src_dir(&self) -> PathBuf {
        self.install_prefix.join("src")
    }

    /// Build provenance record (§6.5 — the scoping `build-info.txt` shape).
    pub fn build_info(&self) -> PathBuf {
        self.install_prefix.join("build-info.txt")
    }

    /// Full install/build log.
    pub fn build_log(&self) -> PathBuf {
        self.install_prefix.join("build.log")
    }

    pub fn rpc_sock(&self) -> PathBuf {
        self.run_dir.join(RPC_SOCK_FILENAME)
    }

    pub fn pidfile(&self) -> PathBuf {
        self.run_dir.join(PIDFILE_FILENAME)
    }

    pub fn tgt_log(&self) -> PathBuf {
        self.run_dir.join(TGT_LOG_FILENAME)
    }

    /// SPDK-native state home under the shared state dir (§6.4):
    /// `tgt-config.json`, `ptpl/*.json`, the hugepage prior record.
    pub fn spdk_state_dir(&self) -> PathBuf {
        self.state_dir.join("spdk")
    }

    /// The SPDK source of truth (`save_config` output; §6.4).
    pub fn tgt_config(&self) -> PathBuf {
        self.spdk_state_dir().join("tgt-config.json")
    }

    /// Reservation-persistence files (`ptpl_file`s), populated from N4.
    pub fn ptpl_dir(&self) -> PathBuf {
        self.spdk_state_dir().join("ptpl")
    }
}

// ---------------------------------------------------------------------------
// SpdkStack — the SPDK share path behind the TargetStack trait (§6.5, N4)
// ---------------------------------------------------------------------------

/// The manual removal runbook for a live foreign SPDK object (§6.4: the
/// refusal message IS the runbook). Steps are rpc.py against the
/// product's socket — the pinned tree ships the script.
pub(crate) fn manual_removal_steps(sock: &Path, nqn: &str, bdev: Option<&str>) -> String {
    let rpcpy = format!(
        "{}/{}/src/scripts/rpc.py -s {}",
        DEFAULT_INSTALL_ROOT,
        lifecycle::SPDK_PINNED_TAG,
        sock.display()
    );
    let bdev_step = match bdev {
        Some(name) => format!("    {rpcpy} bdev_aio_delete {name}"),
        None => format!("    {rpcpy} bdev_aio_delete <its bdev>   (bdev_get_bdevs lists it)"),
    };
    format!("    {rpcpy} nvmf_delete_subsystem {nqn}\n{bdev_step}")
}

fn rpc_refused(e: SpdkRpcError) -> NvmeofError {
    NvmeofError::Refused(format!("SPDK {e}"))
}

/// SPDK spells the address family `IPv4`/`IPv6` (nvmet configfs uses
/// lowercase — each stack keeps its own spelling per the G3 module-graph
/// rule).
fn adrfam_spdk(ip: &str) -> Result<&'static str, NvmeofError> {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => Ok("IPv4"),
        Ok(std::net::IpAddr::V6(_)) => Ok("IPv6"),
        Err(_) => Err(NvmeofError::Refused(format!(
            "listener address '{ip}' is not a valid IP address"
        ))),
    }
}

/// NVMe serial number derived from the recorded ns UUID (hex, ≤ 20
/// chars) — deterministic, so a restore re-create presents the same
/// controller serial and initiators see a stable device identity.
fn serial_of(ns_uuid: &str) -> String {
    ns_uuid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(20)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The derived aio bdev name (`sqz_aio_<uuid-hex-prefix>` — the §6.4
/// schema shape); recorded at share time, and teardown/restore always
/// use the RECORDED name, never a re-derivation.
fn bdev_name_of(ns_uuid: &str) -> String {
    let hex: String = ns_uuid.chars().filter(char::is_ascii_hexdigit).collect();
    format!("sqz_aio_{}", &hex[..hex.len().min(12)])
}

/// The SPDK target stack (§6.1/§6.5, PR 4/N4): `save_config`-backed
/// share/unshare/restore over the RPC client v2, every share pinning
/// `nsid` + ns UUID + `ptpl_file` (the scoping §4 pt 1 fix — PTPL state
/// binds to the namespace UUID, so restore re-presents the recorded
/// identity and reservations survive target power cycles), riding the
/// shared ledger intent protocol (§6.4 law 6) and the live-state
/// duplicate guard (`bdev_get_bdevs` filename scan — the one duplicate
/// check the old module got right, kept).
pub struct SpdkStack {
    paths: SpdkPaths,
    ledger: Ledger,
    /// §6.2 flag placement: gates preflight rung 4 on the mutating verbs.
    accept_version_drift: bool,
    /// `unshare --force`: overrides the live-consumer refusal (design R7
    /// posture, mirrors `target stop`).
    unshare_force: bool,
}

impl SpdkStack {
    /// Stack rooted at explicit paths + ledger (the sanctioned unit-tier
    /// injection seam: every code path executed is the production path
    /// pointed at a different location — §6.8).
    pub fn new(
        paths: SpdkPaths,
        ledger: Ledger,
        accept_version_drift: bool,
        unshare_force: bool,
    ) -> Self {
        SpdkStack {
            paths,
            ledger,
            accept_version_drift,
            unshare_force,
        }
    }

    /// Production constructor (env-aware paths, default ledger).
    pub fn open_default(accept_version_drift: bool, unshare_force: bool) -> Self {
        SpdkStack::new(
            SpdkPaths::resolve(),
            Ledger::open_default(),
            accept_version_drift,
            unshare_force,
        )
    }

    fn client(&self) -> SpdkRpcClient {
        SpdkRpcClient::new(self.paths.rpc_sock())
    }

    /// Preflight rung 2 (§6.2 share ladder): a running spdk_tgt per
    /// pidfile (direct mode) or socket-serving process (systemd mode).
    fn preflight_running(&self) -> Result<(), PreflightError> {
        if matches!(
            lifecycle::read_pidfile_state(&self.paths.pidfile()),
            Ok(lifecycle::PidfileState::Running(_))
        ) {
            return Ok(());
        }
        if lifecycle::find_target_pid_by_socket(&self.paths.rpc_sock()).is_some() {
            return Ok(());
        }
        Err(PreflightError {
            message: format!(
                "error: SPDK target stack unavailable: no running spdk_tgt serves {} (pidfile \
                 {} names no live pid; no process carries -r {})\n  the target is not \
                 running:\n    check:  sudo squeezefs nvmeof target status\n    start:  sudo \
                 squeezefs nvmeof target start\n  if you intended the kernel target stack, \
                 select it explicitly:\n    sudo squeezefs nvmeof <verb> … --target-stack \
                 nvmet\n{}",
                self.paths.rpc_sock().display(),
                self.paths.pidfile().display(),
                self.paths.rpc_sock().display(),
                lifecycle::NO_FALLBACK_NOTE,
            ),
        })
    }

    /// Rung 3 with the restore-verb wait (§6.5: the systemd
    /// `ExecStartPost` restore races spdk_tgt's RPC readiness — poll the
    /// proven start-loop budget before failing with the designed
    /// dead-RPC message).
    fn preflight_rpc_wait(&self) -> Result<SpdkVersion, PreflightError> {
        let deadline = std::time::Instant::now() + lifecycle::START_RPC_POLL_TIMEOUT;
        let client = self.client();
        loop {
            match client.version() {
                Ok(v) => return Ok(v),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(lifecycle::START_RPC_POLL_INTERVAL);
                }
                Err(_) => return lifecycle::preflight_rpc(&self.paths),
            }
        }
    }

    /// The §6.2 mutating preflight ladder: (1) pinned binary / loud
    /// override, (2) process alive, (3) RPC answers (with the restore
    /// wait when asked), (4) version-drift gate under
    /// `--accept-version-drift`.
    fn mutating_preflight(&self, wait_for_rpc: bool) -> Result<(), PreflightError> {
        let (_bin, warning) = lifecycle::preflight_binary(&self.paths)?;
        if let Some(w) = warning {
            eprintln!("{w}");
        }
        self.preflight_running()?;
        let version = if wait_for_rpc {
            self.preflight_rpc_wait()?
        } else {
            lifecycle::preflight_rpc(&self.paths)?
        };
        if let Some(warning) = lifecycle::gate_version_drift(
            &version,
            lifecycle::DriftPolicy::Mutating {
                accept_version_drift: self.accept_version_drift,
            },
        )? {
            eprintln!("{warning}");
        }
        Ok(())
    }

    /// All aio bdevs as `name -> filename` (the §6.4 duplicate-guard
    /// filename scan's source; non-aio bdevs have no backing path and
    /// never participate).
    fn aio_bdevs(&self, client: &SpdkRpcClient) -> Result<Vec<(String, String)>, NvmeofError> {
        let bdevs = client.call("bdev_get_bdevs", None).map_err(rpc_refused)?;
        Ok(bdevs
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        Some((
                            b.get("name")?.as_str()?.to_string(),
                            b.get("driver_specific")?
                                .get("aio")?
                                .get("filename")?
                                .as_str()?
                                .to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The live walk behind `live_shares` (`nvmf_get_subsystems` +
    /// `bdev_get_bdevs`): every non-Discovery subsystem, its first
    /// namespace's identity, its listeners, and the aio backing path
    /// resolved through the bdev map.
    fn walk_live(&self, client: &SpdkRpcClient) -> Result<Vec<LiveShare>, NvmeofError> {
        let filename_of: std::collections::HashMap<String, String> =
            self.aio_bdevs(client)?.into_iter().collect();
        let subs = client
            .call("nvmf_get_subsystems", None)
            .map_err(rpc_refused)?;
        let mut out = Vec::new();
        for sub in subs.as_array().cloned().unwrap_or_default() {
            if sub.get("subtype").and_then(serde_json::Value::as_str) == Some("Discovery") {
                continue;
            }
            let Some(nqn) = sub.get("nqn").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let namespaces = sub
                .get("namespaces")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let ns0 = namespaces.first();
            let bdev_name = ns0.and_then(|n| {
                n.get("bdev_name")
                    .or_else(|| n.get("name"))
                    .and_then(serde_json::Value::as_str)
            });
            let device_path = bdev_name
                .and_then(|b| filename_of.get(b))
                .cloned()
                .unwrap_or_default();
            let backing_canonical = if device_path.is_empty() {
                String::new()
            } else {
                super::canonical_or_raw(&device_path)
            };
            let listeners = sub
                .get("listen_addresses")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|l| Listener {
                            ip: l
                                .get("traddr")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            port: l
                                .get("trsvcid")
                                .and_then(serde_json::Value::as_str)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                            nvmet_port_id: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(LiveShare {
                subnqn: nqn.to_string(),
                device_path,
                backing_canonical,
                ns_uuid: ns0
                    .and_then(|n| n.get("uuid").and_then(serde_json::Value::as_str))
                    .map(str::to_string),
                listeners,
                enabled: !namespaces.is_empty(),
            });
        }
        Ok(out)
    }

    /// Tolerant live-state gather for `list` and the cross-stack
    /// duplicate guard's nvmet-share direction: a dead/unreachable
    /// target serves nothing (SPDK state is process-resident), so the
    /// walk degrades to empty **with the loud note returned** — never a
    /// silent hole, never a hard failure on a box that simply does not
    /// run SPDK.
    pub fn live_shares_tolerant(&self) -> (Vec<LiveShare>, Option<String>) {
        let client = self.client();
        match client.version() {
            Err(e) => (
                Vec::new(),
                Some(format!(
                    "SPDK target {e} — a stopped target serves nothing; no live SPDK state to \
                     reconcile"
                )),
            ),
            Ok(_) => match self.walk_live(&client) {
                Ok(live) => (live, None),
                Err(e) => (
                    Vec::new(),
                    Some(format!("SPDK live-state walk failed: {e}")),
                ),
            },
        }
    }

    /// The §6.4 duplicate-guard classification half: how a refusal names
    /// a live holder.
    fn classify_holder(&self, nqn: &str) -> String {
        match self.ledger.find(nqn) {
            Ok(Some(rec)) => format!("managed — ledger state {}", rec.state.as_str()),
            Ok(None) => "foreign — not in the share ledger".to_string(),
            Err(e) => format!("unknown — share ledger unreadable: {e}"),
        }
    }

    /// §6.4 live-state duplicate guard, SPDK arm: (a) live subsystems
    /// serving the requested backing or squatting the requested NQN,
    /// (b) the `bdev_get_bdevs` filename scan — an aio bdev already
    /// opening the backing (attached to a subsystem or not) refuses, as
    /// does a bdev-name squat. Runs BEFORE the intent record; refusals
    /// are the runbook and mutate nothing.
    fn duplicate_guard(
        &self,
        client: &SpdkRpcClient,
        req: &ShareRequest,
        bdev_name: &str,
    ) -> Result<(), NvmeofError> {
        for live in self.walk_live(client)? {
            let materialized = !live.device_path.is_empty() || live.enabled;
            if !materialized {
                continue;
            }
            let class = self.classify_holder(&live.subnqn);
            let exit = if class.starts_with("managed") {
                format!(
                    "  unshare the holder first:\n    sudo squeezefs nvmeof unshare {}",
                    live.subnqn
                )
            } else {
                format!(
                    "  removal-first is the only re-share path while the old object serves \
                     (docs/design-nvmeof-target-management.md §6.4):\n{}",
                    manual_removal_steps(&self.paths.rpc_sock(), &live.subnqn, None)
                )
            };
            if live.subnqn == req.subnqn {
                return Err(NvmeofError::Refused(format!(
                    "subsystem '{}' is already live on the SPDK target (classification: \
                     {class}); SqueezeFS never adopts or clobbers a live object implicitly.\n\
                     {exit}",
                    live.subnqn
                )));
            }
            let same_backing = (!live.backing_canonical.is_empty()
                && live.backing_canonical == req.backing_canonical)
                || (!live.device_path.is_empty()
                    && (live.device_path == req.backing_path
                        || live.device_path == req.backing_canonical));
            if same_backing {
                return Err(NvmeofError::Refused(format!(
                    "backing path '{}' is already served by live SPDK subsystem '{}' \
                     (classification: {class}) — the same backing must never be double-served, \
                     across stacks included.\n{exit}",
                    req.backing_path, live.subnqn
                )));
            }
        }
        // The bare-bdev filename scan: catches backings opened by a bdev
        // that is not (or not yet) attached to any subsystem.
        for (name, filename) in self.aio_bdevs(client)? {
            if filename == req.backing_path || filename == req.backing_canonical {
                return Err(NvmeofError::Refused(format!(
                    "backing path '{}' is already opened by SPDK bdev '{name}' — the same \
                     backing must never be double-served.\n  if it belongs to a managed share, \
                     unshare that share; a foreign/orphaned bdev is removed manually:\n{}",
                    req.backing_path,
                    manual_removal_steps(
                        &self.paths.rpc_sock(),
                        "<its subsystem, if any>",
                        Some(&name)
                    ),
                )));
            }
            if name == bdev_name {
                return Err(NvmeofError::Refused(format!(
                    "derived bdev name '{name}' already exists on the target (serving \
                     '{filename}') — a duplicate --ns-uuid seed? Choose a different --ns-uuid \
                     or remove the holder:\n{}",
                    manual_removal_steps(
                        &self.paths.rpc_sock(),
                        "<its subsystem, if any>",
                        Some(&name)
                    ),
                )));
            }
        }
        Ok(())
    }

    /// The recorded ptpl path made absolute under the state dir.
    fn ptpl_abs(&self, rec: &ShareRecord) -> Option<PathBuf> {
        rec.ptpl_file
            .as_deref()
            .map(|rel| self.paths.state_dir.join(rel))
    }

    /// The share-time RPC mutations (also the restore re-share body —
    /// §6.6-parity: the recorded identity is re-presented verbatim).
    /// Every call is checked; a failure part-way leaves the caller's
    /// intent record claiming whatever exists (§6.4 law 6).
    fn apply_share(&self, client: &SpdkRpcClient, rec: &ShareRecord) -> Result<(), NvmeofError> {
        let ns_uuid = rec.ns_uuid.as_deref().ok_or_else(|| {
            NvmeofError::Refused(format!(
                "record '{}' carries no ns_uuid — the SPDK path always records the namespace \
                 identity before mutating (PTPL binds to it)",
                rec.subnqn
            ))
        })?;
        let bdev = rec.bdev_name.as_deref().ok_or_else(|| {
            NvmeofError::Refused(format!(
                "record '{}' carries no bdev_name — N4+ SPDK records always do",
                rec.subnqn
            ))
        })?;
        let ptpl = self.ptpl_abs(rec).ok_or_else(|| {
            NvmeofError::Refused(format!(
                "record '{}' carries no ptpl_file — N4+ SPDK records always do (reservation \
                 persistence is not optional on this stack)",
                rec.subnqn
            ))
        })?;
        let nsid = rec.nsid.unwrap_or(1);

        // Transport: ensure TCP exists (a fresh target has none).
        let transports = client
            .call("nvmf_get_transports", None)
            .map_err(rpc_refused)?;
        let has_tcp = transports
            .as_array()
            .map(|arr| {
                arr.iter().any(|t| {
                    t.get("trtype")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case("tcp"))
                })
            })
            .unwrap_or(false);
        if !has_tcp {
            client
                .call("nvmf_create_transport", Some(json!({ "trtype": "TCP" })))
                .map_err(rpc_refused)?;
        }

        // bdev (block_size 4096 pinned — §6.5).
        client
            .call(
                "bdev_aio_create",
                Some(json!({
                    "name": bdev,
                    "filename": rec.backing_path,
                    "block_size": 4096,
                })),
            )
            .map_err(rpc_refused)?;

        // Subsystem (+ §Security allow-host wiring, the nvmet precedent:
        // an allowlist share is never allow-any).
        client
            .call(
                "nvmf_create_subsystem",
                Some(json!({
                    "nqn": rec.subnqn,
                    "allow_any_host": rec.allow_hosts.is_empty(),
                    "serial_number": serial_of(ns_uuid),
                })),
            )
            .map_err(rpc_refused)?;
        for host in &rec.allow_hosts {
            client
                .call(
                    "nvmf_subsystem_add_host",
                    Some(json!({ "nqn": rec.subnqn, "host": host })),
                )
                .map_err(rpc_refused)?;
        }

        // Namespace — THE §6.5 identity pinning: -n nsid, -u uuid,
        // --ptpl-file <state>/spdk/ptpl/<uuid>.json. The dir must exist
        // before add_ns (SPDK creates only the file).
        if let Some(parent) = ptpl.parent() {
            fs::create_dir_all(parent).map_err(NvmeofError::Io)?;
        }
        client
            .call(
                "nvmf_subsystem_add_ns",
                Some(json!({
                    "nqn": rec.subnqn,
                    "namespace": {
                        "bdev_name": bdev,
                        "nsid": nsid,
                        "uuid": ns_uuid,
                        "ptpl_file": ptpl.display().to_string(),
                    },
                })),
            )
            .map_err(rpc_refused)?;

        // One listener per (ip, port).
        for l in &rec.listeners {
            client
                .call(
                    "nvmf_subsystem_add_listener",
                    Some(json!({
                        "nqn": rec.subnqn,
                        "listen_address": {
                            "trtype": "TCP",
                            "adrfam": adrfam_spdk(&l.ip)?,
                            "traddr": l.ip,
                            "trsvcid": l.port.to_string(),
                        },
                    })),
                )
                .map_err(rpc_refused)?;
        }
        Ok(())
    }

    /// Live-consumer probe for the R7 unshare refusal. A vanished
    /// subsystem has zero consumers (RPC not-found is the verified
    /// no-op class); transport errors stay loud.
    fn live_controllers(&self, client: &SpdkRpcClient, nqn: &str) -> Result<usize, NvmeofError> {
        match client.call(
            "nvmf_subsystem_get_controllers",
            Some(json!({ "nqn": nqn })),
        ) {
            Ok(v) => Ok(v.as_array().map(Vec::len).unwrap_or(0)),
            Err(SpdkRpcError::Rpc { .. }) => Ok(0),
            Err(e) => Err(rpc_refused(e)),
        }
    }

    /// Teardown of one record's live objects — listener → subsystem →
    /// bdev, checked errors, tolerant of already-vanished pieces (§6.4
    /// law 4: a vanished object is a verified no-op). Returns whether
    /// any target state was actually mutated.
    fn teardown(&self, client: &SpdkRpcClient, rec: &ShareRecord) -> Result<bool, NvmeofError> {
        let mut mutated = false;
        let live = self.walk_live(client)?;
        if let Some(l) = live.iter().find(|l| l.subnqn == rec.subnqn) {
            // Drain the recorded listeners first (stops new connects),
            // then delete the subsystem (which drops namespaces).
            for listener in &rec.listeners {
                let live_has = l
                    .listeners
                    .iter()
                    .any(|x| x.ip == listener.ip && x.port == listener.port);
                if !live_has {
                    continue;
                }
                client
                    .call(
                        "nvmf_subsystem_remove_listener",
                        Some(json!({
                            "nqn": rec.subnqn,
                            "listen_address": {
                                "trtype": "TCP",
                                "adrfam": adrfam_spdk(&listener.ip)?,
                                "traddr": listener.ip,
                                "trsvcid": listener.port.to_string(),
                            },
                        })),
                    )
                    .map_err(rpc_refused)?;
            }
            client
                .call("nvmf_delete_subsystem", Some(json!({ "nqn": rec.subnqn })))
                .map_err(rpc_refused)?;
            mutated = true;
        } else {
            println!(
                "note: subsystem '{}' is no longer served by the SPDK target — nothing to \
                 tear down (cleaning the ledger record).",
                rec.subnqn
            );
        }

        match rec.bdev_name.as_deref() {
            Some(bdev) => {
                let holder = self
                    .aio_bdevs(client)?
                    .into_iter()
                    .find(|(name, _)| name == bdev);
                match holder {
                    Some((name, filename))
                        if filename == rec.backing_path || filename == rec.backing_canonical =>
                    {
                        client
                            .call("bdev_aio_delete", Some(json!({ "name": name })))
                            .map_err(rpc_refused)?;
                        mutated = true;
                    }
                    Some((name, filename)) => {
                        println!(
                            "note: bdev '{name}' now serves '{filename}', not the recorded \
                             backing '{}' — leaving it in place (it is not this share's \
                             object anymore).",
                            rec.backing_path
                        );
                    }
                    None => {
                        println!("note: bdev '{bdev}' is no longer present — nothing to delete.");
                    }
                }
            }
            None => {
                println!(
                    "note: record '{}' carries no bdev_name — no bdev teardown to perform.",
                    rec.subnqn
                );
            }
        }
        Ok(mutated)
    }

    /// Restore-time equivalence: the live object serves the record's
    /// backing under the record's identity (backing path **and** ns
    /// UUID — a changed identity is exactly what the kernel initiator's
    /// namespace revalidation trips on).
    fn live_matches_record(&self, live: &LiveShare, rec: &ShareRecord) -> Result<(), String> {
        let device_ok = (!live.backing_canonical.is_empty()
            && live.backing_canonical == rec.backing_canonical)
            || live.device_path == rec.backing_path
            || live.device_path == rec.backing_canonical;
        if !device_ok {
            return Err(format!(
                "live bdev backing '{}' (canonical '{}') does not serve the recorded backing \
                 '{}'",
                live.device_path, live.backing_canonical, rec.backing_canonical
            ));
        }
        if let Some(rec_uuid) = rec.ns_uuid.as_deref() {
            match live.ns_uuid.as_deref() {
                Some(live_uuid) if live_uuid.eq_ignore_ascii_case(rec_uuid) => {}
                Some(live_uuid) => {
                    return Err(format!(
                        "live ns UUID '{live_uuid}' mismatches the recorded ns_uuid \
                         '{rec_uuid}' — a changed identity is exactly what the kernel \
                         initiator's namespace revalidation trips on (and what PTPL state \
                         binds to)"
                    ));
                }
                None => {
                    return Err(format!(
                        "live namespace exposes no UUID while the record carries ns_uuid \
                         '{rec_uuid}'"
                    ));
                }
            }
        }
        Ok(())
    }

    /// One record's restore replay (§6.4 laws 4 + 6). Returns the
    /// outcome plus whether target/ledger state changed (the §6.4
    /// persistence law's save trigger).
    fn restore_record(
        &self,
        client: &SpdkRpcClient,
        rec: &ShareRecord,
    ) -> Result<RestoreOutcome, NvmeofError> {
        match rec.state {
            ShareState::Removing => {
                log::warn!(
                    "restore: resuming interrupted teardown of '{}' (removing intent)",
                    rec.subnqn
                );
                self.teardown(client, rec)?;
                self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
                Ok(RestoreOutcome::TeardownResumed)
            }
            ShareState::Pending => {
                let live = self
                    .walk_live(client)?
                    .into_iter()
                    .find(|l| l.subnqn == rec.subnqn);
                match live {
                    Some(live) if !live.device_path.is_empty() || live.enabled => {
                        match self.live_matches_record(&live, rec) {
                            Ok(()) => {
                                log::warn!(
                                    "restore: finalizing crash-window pending intent '{}' — \
                                     its live objects exist and match",
                                    rec.subnqn
                                );
                                self.ledger
                                    .finalize_share(&rec.subnqn)
                                    .map_err(NvmeofError::Io)?;
                                Ok(RestoreOutcome::FinalizedPending)
                            }
                            Err(why) => Ok(RestoreOutcome::Failed(format!(
                                "pending intent '{}' has live objects that DO NOT match \
                                 ({why}) — refusing to finalize or clobber; resolve manually \
                                 (unshare the record or remove the live object)",
                                rec.subnqn
                            ))),
                        }
                    }
                    other => {
                        log::warn!(
                            "restore: garbage-collecting pending intent '{}' — {} (the \
                             interrupted share never completed)",
                            rec.subnqn,
                            if other.is_some() {
                                "only an unmaterialized subsystem shell exists"
                            } else {
                                "no live objects"
                            }
                        );
                        self.teardown(client, rec)?;
                        self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
                        Ok(RestoreOutcome::GarbageCollectedPending)
                    }
                }
            }
            ShareState::Active => {
                let live = self
                    .walk_live(client)?
                    .into_iter()
                    .find(|l| l.subnqn == rec.subnqn);
                if let Some(live) = live {
                    return match self.live_matches_record(&live, rec) {
                        Ok(()) => Ok(RestoreOutcome::VerifiedNoop),
                        Err(why) => Ok(RestoreOutcome::Failed(format!(
                            "'{}' exists live but MISMATCHES its record ({why}) — never \
                             clobbered; resolve manually",
                            rec.subnqn
                        ))),
                    };
                }
                // Gone: re-establish, re-presenting the recorded identity
                // (same uuid/nsid/ptpl_file/bdev/serial — PTPL re-binds).
                self.apply_share(client, rec)?;
                log::info!("restore: re-shared '{}'", rec.subnqn);
                Ok(RestoreOutcome::Restored)
            }
        }
    }
}

impl TargetStack for SpdkStack {
    fn kind(&self) -> StackKind {
        StackKind::Spdk
    }

    fn preflight(&self, op: PreflightOp) -> Result<(), PreflightError> {
        match op {
            // The gather for list is tolerant by design
            // (live_shares_tolerant) — nothing to preflight.
            PreflightOp::List => Ok(()),
            PreflightOp::Share | PreflightOp::Unshare => self.mutating_preflight(false),
            // §6.5: the systemd ExecStartPost restore waits out the
            // target's RPC-readiness window.
            PreflightOp::Restore => self.mutating_preflight(true),
        }
    }

    fn share(&self, req: &ShareRequest) -> Result<ShareRecord, NvmeofError> {
        let client = self.client();
        let bdev_name = bdev_name_of(&req.ns_uuid);

        // Live-state duplicate guard BEFORE any side effect (§6.4: the
        // guard is ledger + live state; `begin_share` below re-checks
        // the ledger half atomically under the ledger flock).
        self.duplicate_guard(&client, req, &bdev_name)?;

        let mut record = ShareRecord {
            subnqn: req.subnqn.clone(),
            stack: StackKind::Spdk,
            state: ShareState::Pending,
            backing_path: req.backing_path.clone(),
            backing_canonical: req.backing_canonical.clone(),
            // §6.2: --nsid default 1, SPDK-only, recorded.
            nsid: Some(req.nsid.unwrap_or(1)),
            ns_uuid: Some(req.ns_uuid.clone()),
            listeners: req.listeners.clone(),
            bdev_name: Some(bdev_name),
            // Recorded state-dir-relative (§6.4 schema); the RPC gets
            // the absolute path.
            ptpl_file: Some(format!("spdk/ptpl/{}.json", req.ns_uuid)),
            loop_device: None,
            created_utc: super::ledger::utc_now_rfc3339(),
            allow_hosts: req.allow_hosts.clone(),
            adopted_from: None,
        };

        // §6.4 law 6: the pending intent is recorded BEFORE the first
        // RPC mutation.
        self.ledger.begin_share(&record).map_err(NvmeofError::Io)?;

        let result = self
            .apply_share(&client, &record)
            // §6.4 SPDK persistence law: share ends with save_config —
            // and the record flips active only AFTER it (law 6: "after
            // the last mutation succeeds (SPDK: after save_config)").
            .and_then(|()| lifecycle::save_config(&client, &self.paths.tgt_config()));
        match result {
            Ok(()) => {
                self.ledger
                    .finalize_share(&record.subnqn)
                    .map_err(NvmeofError::Io)?;
                record.state = ShareState::Active;
                Ok(record)
            }
            Err(e) => {
                log::warn!(
                    "share of '{}' failed mid-flight ({e}); its pending intent record remains \
                     in the ledger and still claims any created objects — reconcile with \
                     'squeezefs nvmeof restore' or tear down with 'squeezefs nvmeof unshare \
                     {}'",
                    record.subnqn,
                    record.subnqn
                );
                Err(e)
            }
        }
    }

    fn unshare(&self, rec: &ShareRecord) -> Result<(), NvmeofError> {
        let client = self.client();

        // R7: refuse with live consumers unless --force — BEFORE the
        // removing intent (a refused unshare never leaves `removing`).
        if !self.unshare_force {
            let consumers = self.live_controllers(&client, &rec.subnqn)?;
            if consumers > 0 {
                return Err(NvmeofError::Refused(format!(
                    "unshare: refusing — subsystem '{}' has {consumers} live initiator \
                     connection(s).\n  sequence: unmount consumers → sudo squeezefs nvmeof \
                     disconnect {} → retry\n  or force the teardown (initiators enter \
                     reconnect storms until re-shared): sudo squeezefs nvmeof unshare {} \
                     --force",
                    rec.subnqn, rec.subnqn, rec.subnqn
                )));
            }
        }

        // §6.4 law 6: flip to `removing` BEFORE the first teardown
        // write; delete the record only after teardown (+ save_config)
        // completes.
        self.ledger
            .mark_removing(&rec.subnqn)
            .map_err(NvmeofError::Io)?;
        self.teardown(&client, rec)?;
        // §6.4 persistence law: unshare ALWAYS saves — even a vanished-
        // object no-op teardown must stop tgt-config.json describing the
        // share, or the next load_config resurrects it.
        lifecycle::save_config(&client, &self.paths.tgt_config())?;
        self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
        Ok(())
    }

    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        self.walk_live(&self.client())
    }

    fn restore(&self, recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError> {
        let client = self.client();
        let tgt_config = self.paths.tgt_config();

        // The §6.5 systemd-ExecStartPost half: replay the SPDK source of
        // truth via load_config — but only onto an EMPTY target (no
        // subsystems, no transports): a target that already carries
        // state (a `target start` already ran load_config, or objects
        // exist) must never double-apply config entries.
        if tgt_config.exists() {
            let live_empty = self.walk_live(&client)?.is_empty();
            let transports_empty = client
                .call("nvmf_get_transports", None)
                .map_err(rpc_refused)?
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if live_empty && transports_empty {
                let parsed: Result<serde_json::Value, String> = fs::read(&tgt_config)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
                let config = parsed.map_err(|e| {
                    NvmeofError::Refused(format!(
                        "restore: {} is unreadable ({e}) — fix or move the config aside, then \
                         restore again",
                        tgt_config.display()
                    ))
                })?;
                let report = lifecycle::load_config(&client, &config)?;
                println!(
                    "restore: load_config applied {} method(s) from {}",
                    report.applied,
                    tgt_config.display()
                );
                for method in &report.skipped {
                    println!(
                        "restore: load_config SKIPPED '{method}' — not callable in the \
                         current RPC state (STARTUP-only entry on a runtime target)"
                    );
                }
            }
        }

        let mut out = RestoreReport::default();
        let mut changed = false;
        for rec in recs {
            if rec.stack != StackKind::Spdk {
                out.entries.push(super::stack::RestoreEntry {
                    subnqn: rec.subnqn.clone(),
                    outcome: RestoreOutcome::Skipped(format!(
                        "recorded on the {} stack — not replayed by the spdk stack (bare \
                         `restore` dispatches each record to its recorded stack)",
                        rec.stack.as_str()
                    )),
                });
                continue;
            }
            // Per-share errors are collected, not short-circuited
            // (a report, not a first-failure bail — §6.1).
            let outcome = match self.restore_record(&client, rec) {
                Ok(outcome) => outcome,
                Err(e) => RestoreOutcome::Failed(e.to_string()),
            };
            changed |= matches!(
                outcome,
                RestoreOutcome::Restored
                    | RestoreOutcome::TeardownResumed
                    | RestoreOutcome::FinalizedPending
                    | RestoreOutcome::GarbageCollectedPending
            );
            out.entries.push(super::stack::RestoreEntry {
                subnqn: rec.subnqn.clone(),
                outcome,
            });
        }

        // §6.4 SPDK persistence law (rev-2 issue 20): save whenever
        // reconciliation changed anything — a re-added subsystem left
        // unsaved flaps off at the next load_config; a resumed teardown
        // left unsaved gets RESURRECTED by it. A pure verified-no-op
        // pass skips the save.
        if changed {
            lifecycle::save_config(&client, &tgt_config)?;
            println!(
                "restore: reconciliation changed target/ledger state — config saved to {}",
                tgt_config.display()
            );
        }
        Ok(out)
    }

    /// Programmatic inventory counts for trait completeness — the rich
    /// §6.9 payload (pin, rpc, reactors, hugepages, ptpl, ledger
    /// reconciliation) is `lifecycle::status_payload`, which the CLI
    /// `target status` verb renders; the kernel-module/configfs fields
    /// are structurally nvmet's and read false here.
    fn target_status(&self) -> Result<TargetStatus, NvmeofError> {
        let live = self.walk_live(&self.client())?;
        Ok(TargetStatus {
            stack: StackKind::Spdk,
            modules_present: false,
            configfs_mounted: false,
            subsystems: live.len(),
            namespaces: live.iter().filter(|l| l.enabled).count(),
            ports: live.iter().map(|l| l.listeners.len()).sum(),
            resv_enabled_namespaces: 0,
        })
    }
}
