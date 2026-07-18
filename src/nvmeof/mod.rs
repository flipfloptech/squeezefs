//! NVMe-oF target + initiator management (`squeezefs storage nvmeof …`).
//!
//! N1 state of the dual-stack target-management program
//! (`docs/design-nvmeof-target-management.md`, PR 1): the module is split
//! (`ledger` / `stack` / `initiator` / `nocow`), the **share ledger**
//! (§6.4) is live as the record store for the existing verbs — `share`
//! and `unshare` record/consult it through the write-ahead intent API,
//! `restore-shares` replays it for nvmet, the ledger owns the
//! loop-device association (the configfs fake-file *read* is gone; the
//! never-fires loop-detach bug dies here) — and the dead code is purged
//! (`extract_nvmeof_connection_details`, `SqueezefsError::NvmeOfBackend`,
//! the `#![allow(clippy::all)]` exemption, the self-truncating
//! `/etc/squeezefs/nvmeof_shares.json` registry).
//!
//! **Transitional (dies at N2/N3/N4):** the kernel-nvmet configfs share
//! path, its collision-prone small-int port allocator, the configfs
//! fake-file *write* (silently no-ops on real kernels; nothing reads it
//! from N1 on), the silent sparse auto-create, the `SQUEEZEFS_MOCK_*`
//! env forks, and the raw `spdk-*` lifecycle verbs below are the
//! pre-rebuild paths, kept only until their rebuild PRs land. They are
//! JuiceFS-derived (Apache-2.0) — the retained keeper files
//! (`initiator.rs`, `nocow.rs`) carry the license header per the §6.1
//! provenance note. Ownership is **ledger membership**, never NQN
//! prefix: N1-era records keep the old-style default NQNs and remain
//! fully managed at N2+.
//!
//! io_uring note: everything here is one-shot mount-time/admin control
//! plane (configfs writes, JSON-RPC over a unix socket, nvme-cli
//! shell-outs) — the sanctioned `reservation.rs` precedent; no data path
//! is touched.

pub mod initiator;
pub mod ledger;
pub mod nocow;
pub mod nvmet;
pub mod stack;

pub use initiator::{connect_target, disconnect_target};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use ledger::Ledger;
use stack::{Listener, NvmeofError, ShareRecord, ShareState};

/// Which target stack owns a share for its lifetime
/// (`docs/design-nvmeof-target-management.md` §6.1/§6.4 — the ledger's
/// `"stack"` field; a share is owned by exactly one stack, dispatch is
/// resolved from the ledger, never guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackKind {
    Spdk,
    Nvmet,
}

impl StackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StackKind::Spdk => "spdk",
            StackKind::Nvmet => "nvmet",
        }
    }
}

impl std::str::FromStr for StackKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "spdk" => Ok(StackKind::Spdk),
            "nvmet" => Ok(StackKind::Nvmet),
            other => Err(format!(
                "unknown target stack '{other}' (expected 'spdk' or 'nvmet')"
            )),
        }
    }
}

/// Env half of the §6.2 stack-selection resolution order
/// (`--target-stack` flag > this env > default `spdk`).
pub const TARGET_STACK_ENV: &str = "SQUEEZEFS_NVMEOF_TARGET_STACK";

/// §6.2 stack-selection resolution: flag > `SQUEEZEFS_NVMEOF_TARGET_STACK`
/// > default `spdk`. An unparseable env value refuses loud — never a
/// silent default.
pub fn resolve_stack(flag: Option<StackKind>) -> Result<StackKind, NvmeofError> {
    let _ = flag;
    unimplemented!("PR 2 (N2) RED: resolve_stack")
}

/// §6.2 per-stack flag semantics (rev-3 issue 23): `--ns-uuid` seeds the
/// recorded namespace identity on BOTH stacks and must parse as a UUID;
/// `--nsid` is SPDK-only — the nvmet namespace index is structurally
/// fixed at 1, so `--nsid` ≠ 1 with nvmet refuses loud (the
/// `--disk-cache-paths` precedent: never a silent flag-ignore).
pub fn validate_share_flags(
    stack: StackKind,
    nsid: Option<u32>,
    ns_uuid: Option<&str>,
) -> Result<(), NvmeofError> {
    let _ = (stack, nsid, ns_uuid);
    unimplemented!("PR 2 (N2) RED: validate_share_flags")
}

/// §6.2 backing preparation (both stacks): a missing path refuses loud
/// (the silent 1 GiB sparse auto-create is dead — `--create-size` is the
/// explicit opt-in), directories refuse, and regular-file backings get
/// the NoCOW guard.
pub fn prepare_backing(backing_path: &str, create_size: Option<u64>) -> io::Result<()> {
    let _ = (backing_path, create_size);
    unimplemented!("PR 2 (N2) RED: prepare_backing")
}

/// Everything the `nvmeof share` verb collects from the CLI (§6.2 share
/// row).
#[derive(Debug, Clone)]
pub struct ShareOptions {
    pub backing_path: String,
    pub subnqn: Option<String>,
    pub port: u16,
    pub ips: Vec<String>,
    /// `--target-stack` (resolution: flag > env > default `spdk`).
    pub stack: Option<StackKind>,
    /// SPDK-only (§6.2); `Some(n)` with `n != 1` on nvmet refuses loud.
    pub nsid: Option<u32>,
    /// Seeds the recorded both-stack namespace identity; generated when
    /// absent.
    pub ns_uuid: Option<String>,
    /// Explicit opt-in size (bytes) for creating a missing file backing.
    pub create_size: Option<u64>,
    /// Host-NQN allowlist; empty = allow-any (trusted-fabric default).
    pub allow_hosts: Vec<String>,
}

/// The `nvmeof share` verb (§6.2): grammar validation → stack resolution
/// (SPDK fails loud until N3/N4) → root → backing preparation → the
/// selected stack's share flow (intent protocol + live-state duplicate
/// guard inside).
pub fn share(opts: &ShareOptions) -> Result<ShareRecord, NvmeofError> {
    let _ = opts;
    unimplemented!("PR 2 (N2) RED: share")
}

/// The `nvmeof unshare` verb (§6.2): stack resolved from the ledger —
/// including `pending`/`removing` intent records; an NQN absent from the
/// ledger refuses loud with `list` guidance (we never tear down objects
/// we did not record).
pub fn unshare(subnqn: &str) -> Result<(), NvmeofError> {
    let _ = subnqn;
    unimplemented!("PR 2 (N2) RED: unshare")
}

/// The `nvmeof list` verb (§6.2): ledger ∪ live-state reconciliation —
/// managed / down / pending / removing / foreign — plus the kept
/// connected-fabric-disks section.
pub fn list(json: bool) -> Result<(), NvmeofError> {
    let _ = json;
    unimplemented!("PR 2 (N2) RED: list")
}

/// The `nvmeof restore` verb (§6.2): bare replays EVERY ledger record
/// into its recorded stack; `--target-stack X` filters, never retargets.
/// Reconciles §6.4 law-6 intents; per-share report; idempotent.
pub fn restore(filter: Option<StackKind>) -> Result<(), NvmeofError> {
    let _ = filter;
    unimplemented!("PR 2 (N2) RED: restore")
}

pub(crate) fn is_mock() -> bool {
    std::env::var("SQUEEZEFS_MOCK_NVMEOF").is_ok()
}

pub(crate) fn check_root() -> io::Result<()> {
    if is_mock() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if unsafe { libc::getuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "This command requires root privileges. Please run with sudo.",
            ));
        }
    }
    Ok(())
}

fn configfs_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvmet")
    } else {
        PathBuf::from("/sys/kernel/config/nvmet")
    }
}

pub(crate) fn execute_cmd(cmd_name: &str, args: &[&str]) -> io::Result<String> {
    if is_mock() {
        return Ok(String::new());
    }
    let output = Command::new(cmd_name).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Command {} failed: {}",
            cmd_name,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// First-mutating-verb hook of the §6.4 old-registry migration; skipped
/// in mock mode so tests never touch host `/etc` state (the whole mock
/// fork dies at N2).
fn retire_old_registry_once() {
    if !is_mock() {
        ledger::retire_old_registry(Path::new(ledger::OLD_REGISTRY_PATH));
    }
}

fn canonical_or_raw(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Pre-side-effect duplicate refusal (before the transitional sparse
/// auto-create can fire). `Ledger::begin_share` re-checks the same laws
/// atomically under the ledger flock — this is the polite early exit,
/// that is the authority.
fn ledger_duplicate_precheck(
    ledger: &Ledger,
    subnqn_opt: Option<&str>,
    backing_path: &str,
    canonical: &str,
) -> io::Result<()> {
    for share in ledger.load()? {
        if share.backing_canonical == canonical || share.backing_path == backing_path {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Backing path '{}' is already shared under subsystem '{}' (stack {}, state \
                     {}); unshare it first: squeezefs storage nvmeof unshare {}",
                    backing_path,
                    share.subnqn,
                    share.stack.as_str(),
                    share.state.as_str(),
                    share.subnqn
                ),
            ));
        }
        if subnqn_opt == Some(share.subnqn.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Subsystem '{}' is already in the share ledger (stack {}, state {})",
                    share.subnqn,
                    share.stack.as_str(),
                    share.state.as_str()
                ),
            ));
        }
    }
    Ok(())
}

fn pending_record(
    subnqn: &str,
    stack: StackKind,
    backing_path: &str,
    canonical: &str,
    port: u16,
    ips: &[String],
) -> ShareRecord {
    // N1 transitional presence (§6.4 / PR-plan PR 1): required fields +
    // `loop_device` (recorded once losetup ran); `nvmet_port_id`,
    // `ns_uuid`, `nsid`, `ptpl_file`, `bdev_name` stay null — the
    // pre-rebuild paths stamp/pin nothing.
    ShareRecord {
        subnqn: subnqn.to_string(),
        stack,
        state: ShareState::Pending,
        backing_path: backing_path.to_string(),
        backing_canonical: canonical.to_string(),
        nsid: None,
        ns_uuid: None,
        listeners: ips
            .iter()
            .map(|ip| Listener {
                ip: ip.clone(),
                port,
                nvmet_port_id: None,
            })
            .collect(),
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: ledger::utc_now_rfc3339(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    }
}

pub fn share_target(
    backing_path: &str,
    subnqn_opt: Option<&str>,
    port: u16,
    ips: &[String],
) -> io::Result<String> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();

    // Duplicate refusal before any side effect (ledger membership is the
    // ownership law; live-state walks join the guard with the N2 rebuild).
    ledger_duplicate_precheck(
        &ledger,
        subnqn_opt,
        backing_path,
        &canonical_or_raw(backing_path),
    )?;

    // 1. Check/create backing path if it's a regular file path.
    //    (Transitional: the silent 1 GiB sparse auto-create dies at N2 —
    //    replaced by refuse-loud + the explicit `--create-size` opt-in.)
    let backing_path_buf = PathBuf::from(backing_path);
    if !backing_path_buf.exists() {
        println!(
            "Backing file '{}' does not exist. Auto-creating a 1GB sparse file...",
            backing_path
        );
        let f = fs::File::create(&backing_path_buf)?;
        f.set_len(1024 * 1024 * 1024)?; // 1GB default
    }
    if backing_path_buf.is_file() {
        // CoW guard: a btrfs-CoW backing file silently downgrades O_DIRECT
        // to buffered I/O and wedges the whole fabric under write load.
        nocow::ensure_nocow_backing(&backing_path_buf)?;
    }

    if backing_path_buf.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Backing path cannot be a directory.",
        ));
    }

    let subnqn = match subnqn_opt {
        Some(s) => s.to_string(),
        None => format!("nqn.2026-06.io.squeezefs:subsystem-{}", Uuid::new_v4()),
    };

    // §6.4 law 6: record the pending intent BEFORE the first stack
    // mutation — a crash in any later window leaves a record that still
    // claims the objects (never stranded as "foreign").
    let record = pending_record(
        &subnqn,
        StackKind::Nvmet,
        backing_path,
        &canonical_or_raw(backing_path),
        port,
        ips,
    );
    ledger.begin_share(&record)?;

    match share_nvmet_via_configfs(&subnqn, backing_path, &record.listeners) {
        Ok(loop_device) => {
            if loop_device.is_some() {
                // Law 5: the loop association is ledger bookkeeping.
                ledger.set_loop_device(&subnqn, loop_device)?;
            }
            ledger.finalize_share(&subnqn)?;
            Ok(subnqn)
        }
        Err(e) => {
            log::warn!(
                "share of '{}' failed mid-flight ({e}); its pending intent record remains in \
                 the ledger and still claims any created objects — reconcile with \
                 'squeezefs storage nvmeof restore-shares' or tear down with 'squeezefs \
                 storage nvmeof unshare {}'",
                subnqn,
                subnqn
            );
            Err(e)
        }
    }
}

/// The pre-rebuild kernel-nvmet configfs share path (transitional; N2
/// replaces it wholesale — including the collision-prone small-int port
/// allocator, accepted for this one-PR window). Shared by the `share`
/// verb and the ledger replay in `restore_shares`. Never auto-creates
/// backings. Returns the loop device attached for file backings.
fn share_nvmet_via_configfs(
    subnqn: &str,
    backing_path: &str,
    listeners: &[Listener],
) -> io::Result<Option<String>> {
    let backing_path_buf = PathBuf::from(backing_path);
    if !backing_path_buf.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Backing path '{}' does not exist.", backing_path),
        ));
    }

    // 2. Load modules
    if !is_mock() {
        let _ = execute_cmd("modprobe", &["nvmet"]);
        let _ = execute_cmd("modprobe", &["nvmet-tcp"]);
    }

    // 3. Mount configfs if not mounted
    let config_dir = configfs_path();
    if !is_mock() && !config_dir.exists() {
        let _ = execute_cmd("mount", &["-t", "configfs", "none", "/sys/kernel/config"]);
    }

    // 4. Handle regular files via loop devices
    let mut resolved_device = backing_path.to_string();
    let is_reg_file = !is_mock() && {
        let metadata = fs::metadata(&backing_path_buf)?;
        metadata.is_file()
    };

    let mut associated_loop = None;
    if is_reg_file || (is_mock() && backing_path.ends_with(".img")) {
        // Try to find if already associated
        let loop_list = if is_mock() {
            String::new()
        } else {
            execute_cmd("losetup", &["-j", backing_path]).unwrap_or_default()
        };

        if !loop_list.is_empty() {
            // Already associated, parse loop device
            if let Some(first_line) = loop_list.lines().next() {
                if let Some(pos) = first_line.find(':') {
                    resolved_device = first_line[..pos].to_string();
                    associated_loop = Some(resolved_device.clone());
                }
            }
        } else {
            // Find free loop device and associate
            let free_loop = if is_mock() {
                "/dev/loop99".to_string()
            } else {
                execute_cmd("losetup", &["-f"])?
            };
            if !is_mock() {
                execute_cmd("losetup", &[&free_loop, backing_path])?;
            }
            resolved_device = free_loop.clone();
            associated_loop = Some(free_loop);
        }
    }

    // 5. Setup subsystem
    let sub_dir = config_dir.join("subsystems").join(subnqn);
    fs::create_dir_all(&sub_dir)?;

    // Enable any host access
    fs::write(sub_dir.join("attr_allow_any_host"), "1")?;

    // Create namespace
    let ns_dir = sub_dir.join("namespaces").join("1");
    fs::create_dir_all(&ns_dir)?;
    fs::write(ns_dir.join("device_path"), &resolved_device)?;
    fs::write(ns_dir.join("enable"), "1")?;

    // Transitional (N1 contract): this configfs fake-file *write* is
    // impossible on a real kernel (fails silently) and survives only
    // until N2 deletes this whole path — NOTHING reads it from N1 on;
    // the ledger's `loop_device` field is the association of record.
    if let Some(loop_dev) = &associated_loop {
        let _ = fs::write(sub_dir.join("associated_loop_device"), loop_dev);
    }

    // 6. Setup Ports
    for listener in listeners {
        let ip = &listener.ip;
        let port = listener.port;
        let mut port_exists = false;
        let mut port_id = 1;
        let mut port_dir = config_dir.join("ports").join(port_id.to_string());
        while port_dir.exists() {
            // Read active address and port svc ID to see if it is our IP and port
            if let Ok(addr) = fs::read_to_string(port_dir.join("addr_traddr")) {
                if let Ok(svc) = fs::read_to_string(port_dir.join("addr_trsvcid")) {
                    if addr.trim() == ip && svc.trim() == port.to_string() {
                        port_exists = true;
                        break; // Port already exists and matches!
                    }
                }
            }
            port_id += 1;
            port_dir = config_dir.join("ports").join(port_id.to_string());
        }

        if !port_exists {
            fs::create_dir_all(&port_dir)?;
            fs::write(port_dir.join("addr_traddr"), ip)?;
            fs::write(port_dir.join("addr_trtype"), "tcp")?;
            fs::write(port_dir.join("addr_trsvcid"), port.to_string())?;
            fs::write(port_dir.join("addr_adrfam"), "ipv4")?;
        }

        // Link subsystem to port
        let link_dest = port_dir.join("subsystems").join(subnqn);
        fs::create_dir_all(port_dir.join("subsystems"))?;

        #[cfg(unix)]
        if !link_dest.exists() {
            std::os::unix::fs::symlink(&sub_dir, &link_dest)?;
        }
    }

    Ok(associated_loop)
}

fn call_spdk_rpc(method: &str, params: serde_json::Value) -> io::Result<serde_json::Value> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let socket_path =
        std::env::var("SQUEEZEFS_SPDK_SOCK").unwrap_or_else(|_| "/var/tmp/spdk.sock".to_string());

    if is_mock() {
        return Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": true
        }));
    }

    let mut stream = UnixStream::connect(&socket_path)?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });

    let req_str = request.to_string();
    stream.write_all(req_str.as_bytes())?;
    stream.flush()?;

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        response_bytes.extend_from_slice(&buf[..n]);
        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&response_bytes) {
            if val.get("result").is_some() || val.get("error").is_some() {
                return Ok(val);
            }
        }
    }

    let val: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(val)
}

pub fn share_target_spdk(
    backing_path: &str,
    subnqn_opt: Option<&str>,
    port: u16,
    ips: &[String],
) -> io::Result<String> {
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let canonical = canonical_or_raw(backing_path);

    // Ledger membership guard (cross-stack by construction) …
    ledger_duplicate_precheck(&ledger, subnqn_opt, backing_path, &canonical)?;

    // … plus the live-state check the old module got right: scan active
    // SPDK bdevs directly (a crash-window or foreign share of the same
    // backing still refuses).
    let canonical_target = PathBuf::from(&canonical);
    if let Ok(res) = call_spdk_rpc("bdev_get_bdevs", serde_json::json!({})) {
        if let Some(bdevs) = res.get("result").and_then(|r| r.as_array()) {
            for bdev in bdevs {
                let filename = bdev
                    .get("driver_specific")
                    .and_then(|d| d.get("aio"))
                    .and_then(|aio| aio.get("filename"))
                    .and_then(|f| f.as_str());
                if let Some(filename) = filename {
                    let canonical_existing =
                        fs::canonicalize(filename).unwrap_or_else(|_| PathBuf::from(filename));
                    if canonical_target == canonical_existing {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "Backing path '{}' is already shared by active SPDK bdev '{}'",
                                backing_path,
                                bdev.get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("unknown")
                            ),
                        ));
                    }
                }
            }
        }
    }

    // CoW guard (see nocow::ensure_nocow_backing): SPDK's AIO bdev opens
    // the file O_DIRECT, which btrfs silently downgrades to buffered on
    // CoW files — the single-threaded reactor then wedges in the
    // dirty-page throttle under write load and takes the whole fabric
    // down with it.
    let backing_path_buf = PathBuf::from(backing_path);
    if backing_path_buf.is_file() {
        nocow::ensure_nocow_backing(&backing_path_buf)?;
    }

    let subnqn = match subnqn_opt {
        Some(s) => s.to_string(),
        None => format!("nqn.2026-06.io.squeezefs:spdk-subsystem-{}", Uuid::new_v4()),
    };

    // §6.4 law 6: pending intent before the first RPC mutation.
    let record = pending_record(
        &subnqn,
        StackKind::Spdk,
        backing_path,
        &canonical,
        port,
        ips,
    );
    ledger.begin_share(&record)?;

    match share_spdk_via_rpc(&subnqn, backing_path, &record.listeners) {
        Ok(()) => {
            ledger.finalize_share(&subnqn)?;
            Ok(subnqn)
        }
        Err(e) => {
            log::warn!(
                "SPDK share of '{}' failed mid-flight ({e}); its pending intent record remains \
                 in the ledger and still claims any created objects — tear down with \
                 'squeezefs storage nvmeof unshare {}'",
                subnqn,
                subnqn
            );
            Err(e)
        }
    }
}

/// The pre-rebuild SPDK RPC share path (transitional; N4 replaces it
/// with pinned nsid + ns UUID + `ptpl_file` and the `save_config` law —
/// this path deliberately stamps/pins nothing, which is why the N1-era
/// record's SPDK-only fields stay null).
fn share_spdk_via_rpc(subnqn: &str, backing_path: &str, listeners: &[Listener]) -> io::Result<()> {
    // 1. Create transport (ignore if already exists)
    let _ = call_spdk_rpc(
        "nvmf_create_transport",
        serde_json::json!({
            "trtype": "TCP"
        }),
    );

    // 2. Create bdev from backing path
    let bdev_name = format!("bdev_{}", Uuid::new_v4().simple());

    let res = call_spdk_rpc(
        "bdev_aio_create",
        serde_json::json!({
            "name": bdev_name,
            "filename": backing_path,
            "block_size": 4096
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(io::Error::other(format!(
            "Failed to create SPDK AIO bdev: {}",
            err
        )));
    }

    // 3. Create subsystem
    let res = call_spdk_rpc(
        "nvmf_create_subsystem",
        serde_json::json!({
            "nqn": subnqn,
            "allow_any_host": true,
            "serial_number": format!("SQ{}", &Uuid::new_v4().to_string()[..10])
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(io::Error::other(format!(
            "Failed to create SPDK NVMe-oF subsystem: {}",
            err
        )));
    }

    // 4. Add namespace using our bdev
    let res = call_spdk_rpc(
        "nvmf_subsystem_add_ns",
        serde_json::json!({
            "nqn": subnqn,
            "namespace": {
                "bdev_name": bdev_name
            }
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(io::Error::other(format!(
            "Failed to add bdev to SPDK subsystem namespace: {}",
            err
        )));
    }

    // 5. Add listener to expose the port/IP for each address
    for listener in listeners {
        let res = call_spdk_rpc(
            "nvmf_subsystem_add_listener",
            serde_json::json!({
                "nqn": subnqn,
                "listen_address": {
                    "trtype": "TCP",
                    "adrfam": "IPv4",
                    "traddr": listener.ip,
                    "trsvcid": listener.port.to_string()
                }
            }),
        )?;

        if let Some(err) = res.get("error") {
            return Err(io::Error::other(format!(
                "Failed to expose SPDK subsystem listener on {}:{}: {}",
                listener.ip, listener.port, err
            )));
        }
    }

    Ok(())
}

/// SPDK teardown (transitional pre-rebuild RPC shapes). With
/// `tolerate_missing` (ledgered records — §6.4 law 4) a subsystem that
/// vanished from the live listing is a verified no-op that still cleans
/// the ledger; without it (pre-N1 unledgered shares) it is NotFound.
fn unshare_spdk_via_rpc(subnqn: &str, tolerate_missing: bool) -> io::Result<()> {
    // 1. Get subsystems to identify liveness + the associated bdev name.
    let res = call_spdk_rpc("nvmf_get_subsystems", serde_json::json!({}))?;
    let mut live = false;
    let mut bdev_to_delete = None;
    if let Some(result_arr) = res.get("result").and_then(|r| r.as_array()) {
        for sub in result_arr {
            if sub.get("nqn").and_then(|n| n.as_str()) == Some(subnqn) {
                live = true;
                if let Some(ns1) = sub
                    .get("namespaces")
                    .and_then(|ns| ns.as_array())
                    .and_then(|ns| ns.first())
                {
                    if let Some(name) = ns1.get("name").and_then(|n| n.as_str()) {
                        bdev_to_delete = Some(name.to_string());
                    }
                }
            }
        }
    }

    if !live {
        if tolerate_missing {
            println!(
                "SPDK subsystem '{}' is no longer live on the target — nothing to tear down \
                 (cleaning the ledger record).",
                subnqn
            );
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("SPDK subsystem NQN '{}' not found.", subnqn),
        ));
    }

    // 2. Delete the subsystem
    let res = call_spdk_rpc(
        "nvmf_delete_subsystem",
        serde_json::json!({
            "nqn": subnqn
        }),
    )?;
    if let Some(err) = res.get("error") {
        return Err(io::Error::other(format!(
            "Failed to delete SPDK subsystem: {}",
            err
        )));
    }

    // 3. Delete the associated bdev if we found it
    if let Some(bdev_name) = bdev_to_delete {
        if let Err(e) = call_spdk_rpc(
            "bdev_aio_delete",
            serde_json::json!({
                "name": bdev_name
            }),
        ) {
            log::warn!("bdev_aio_delete of '{bdev_name}' failed after subsystem delete: {e}");
        }
    }

    Ok(())
}

/// Kernel-nvmet configfs teardown (transitional pre-rebuild path; the
/// all-ports symlink walk is deliberate at N1 — the old allocator's
/// small-int port ids are untracked, §6.4 N1 contract). Loop detach does
/// NOT live here: the association is ledger bookkeeping (law 5).
fn unshare_nvmet_via_configfs(subnqn: &str, tolerate_missing: bool) -> io::Result<()> {
    let config_dir = configfs_path();
    let sub_dir = config_dir.join("subsystems").join(subnqn);
    if !sub_dir.exists() {
        if tolerate_missing {
            println!(
                "Subsystem '{}' is no longer present in configfs — nothing to tear down \
                 (cleaning the ledger record).",
                subnqn
            );
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Subsystem NQN '{}' not found.", subnqn),
        ));
    }

    // 1. Remove all symlinks from ports
    let ports_dir = config_dir.join("ports");
    if ports_dir.exists() {
        for entry in fs::read_dir(ports_dir)? {
            let entry = entry?;
            let port_subsystems = entry.path().join("subsystems");
            if port_subsystems.exists() {
                let link_path = port_subsystems.join(subnqn);
                if link_path.exists() {
                    let _ = fs::remove_file(link_path);
                }
            }
        }
    }

    // 2. Disable namespace and delete directories
    if is_mock() {
        let _ = fs::remove_dir_all(&sub_dir);
    } else {
        let ns_dir = sub_dir.join("namespaces").join("1");
        if ns_dir.exists() {
            let _ = fs::write(ns_dir.join("enable"), "0");
            let _ = fs::remove_file(ns_dir.join("device_path"));
            let _ = fs::remove_file(ns_dir.join("enable"));
            let _ = fs::remove_dir(ns_dir);
        }
        let _ = fs::remove_dir(sub_dir.join("namespaces"));
        let _ = fs::remove_file(sub_dir.join("associated_loop_device"));
        let _ = fs::remove_file(sub_dir.join("attr_allow_any_host"));
        let _ = fs::remove_dir(&sub_dir);
    }

    Ok(())
}

/// Detach a ledger-recorded loop device (law 5: the ledger, never
/// configfs, is where `unshare` learns the association — the pre-split
/// configfs read could never see anything on a real kernel, so the
/// detach never fired outside the mock).
fn detach_recorded_loop(loop_dev: &str) {
    if !is_mock() {
        if let Err(e) = execute_cmd("losetup", &["-d", loop_dev]) {
            log::warn!("losetup -d {loop_dev} failed (detach it manually): {e}");
            return;
        }
    }
    println!("Detached associated loop device '{}'.", loop_dev);
}

/// Unshare a target subsystem. Stack dispatch is resolved from the
/// **ledger** — including `pending`/`removing` intent records (§6.4
/// law 6: a crash-window share is still ours to remove); the `--spdk`
/// flag only matters for pre-N1 unledgered shares (transitional legacy
/// fallback, one-PR window — the rebuilt N2 verb refuses unledgered
/// NQNs by ownership law).
pub fn unshare_target(subnqn: &str, spdk_flag: bool) -> io::Result<()> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();

    match ledger.find(subnqn)? {
        Some(record) => {
            if spdk_flag && record.stack != StackKind::Spdk {
                println!(
                    "Note: '--spdk' ignored — the ledger records '{}' on the {} stack \
                     (dispatch is resolved from the ledger, never guessed).",
                    subnqn,
                    record.stack.as_str()
                );
            }
            // §6.4 law 6: flip to `removing` BEFORE the first teardown
            // write; delete the record only after teardown completes.
            ledger.mark_removing(subnqn)?;
            match record.stack {
                StackKind::Spdk => unshare_spdk_via_rpc(subnqn, true)?,
                StackKind::Nvmet => {
                    unshare_nvmet_via_configfs(subnqn, true)?;
                    if let Some(loop_dev) = &record.loop_device {
                        detach_recorded_loop(loop_dev);
                    }
                }
            }
            ledger.delete(subnqn)?;
            Ok(())
        }
        None => {
            // Pre-N1 share (never ledgered): legacy teardown for this
            // one-PR window. No loop detach — the configfs association
            // read is gone (it never worked on a real kernel), and there
            // is no ledger record to consult.
            if spdk_flag {
                unshare_spdk_via_rpc(subnqn, false)
            } else {
                unshare_nvmet_via_configfs(subnqn, false)
            }
        }
    }
}

pub fn list_nvmeof() -> io::Result<()> {
    check_root()?;
    let config_dir = configfs_path();
    let subs_dir = config_dir.join("subsystems");

    // Loop associations come from the ledger (law 5) — the configfs
    // fake file was unreadable on real kernels and nothing reads it now.
    let ledger_records = Ledger::open_default().load().unwrap_or_else(|e| {
        log::warn!("share ledger unreadable during list: {e}");
        Vec::new()
    });
    let loop_of = |nqn: &str| {
        ledger_records
            .iter()
            .find(|r| r.subnqn == nqn)
            .and_then(|r| r.loop_device.clone())
    };

    println!("=== Shared NVMe-oF Targets ===");
    let mut targets_found = false;
    if subs_dir.exists() {
        for entry in fs::read_dir(subs_dir)? {
            let entry = entry?;
            let sub_name = entry.file_name().to_string_lossy().to_string();
            let sub_path = entry.path();

            let backing = if let Ok(dev) =
                fs::read_to_string(sub_path.join("namespaces").join("1").join("device_path"))
            {
                dev.trim().to_string()
            } else {
                "unknown".to_string()
            };

            let assoc_loop = loop_of(&sub_name)
                .map(|l| format!(" (loop: {})", l))
                .unwrap_or_default();

            // Find bound IP and Port
            let mut bound_addr = "0.0.0.0:4420".to_string();
            let ports_dir = config_dir.join("ports");
            if ports_dir.exists() {
                for p_entry in fs::read_dir(ports_dir)? {
                    let p_entry = p_entry?;
                    if p_entry.path().join("subsystems").join(&sub_name).exists() {
                        let ip = fs::read_to_string(p_entry.path().join("addr_traddr"))
                            .unwrap_or_default();
                        let port = fs::read_to_string(p_entry.path().join("addr_trsvcid"))
                            .unwrap_or_default();
                        bound_addr = format!("{}:{}", ip.trim(), port.trim());
                        break;
                    }
                }
            }

            println!("  NQN:    {}", sub_name);
            println!("  Backing: {}{}", backing, assoc_loop);
            println!("  Listen:  {}", bound_addr);
            println!();
            targets_found = true;
        }
    }

    // SPDK Target Subsystems
    if let Ok(res) = call_spdk_rpc("nvmf_get_subsystems", serde_json::json!({})) {
        if let Some(result_arr) = res.get("result").and_then(|r| r.as_array()) {
            for sub in result_arr {
                let nqn = sub.get("nqn").and_then(|n| n.as_str()).unwrap_or_default();
                if nqn == "nqn.2014-08.org.nvmexpress.discovery" {
                    continue; // Skip discovery subsystem
                }

                // Backing bdev
                let mut backing = "unknown".to_string();
                if let Some(ns) = sub
                    .get("namespaces")
                    .and_then(|n| n.as_array())
                    .and_then(|n| n.first())
                {
                    if let Some(bdev_name) = ns.get("bdev_name").and_then(|b| b.as_str()) {
                        backing = bdev_name.to_string();
                    }
                }

                // Listen addresses
                let mut listen_str = Vec::new();
                if let Some(listeners) = sub.get("listen_addresses").and_then(|l| l.as_array()) {
                    for listener in listeners {
                        let ip = listener
                            .get("traddr")
                            .and_then(|i| i.as_str())
                            .unwrap_or("");
                        let port = listener
                            .get("trsvcid")
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        if !ip.is_empty() && !port.is_empty() {
                            listen_str.push(format!("{}:{}", ip, port));
                        }
                    }
                }
                let bound_addr = if listen_str.is_empty() {
                    "none".to_string()
                } else {
                    listen_str.join(", ")
                };

                println!("  NQN:    {}", nqn);
                println!("  Backing: {} (SPDK)", backing);
                println!("  Listen:  {}", bound_addr);
                println!();
                targets_found = true;
            }
        }
    }

    if !targets_found {
        println!("  No shared NVMe-oF targets configured.");
    }

    println!("=== Connected Fabric Disks ===");
    let initiator_found = initiator::print_connected_fabric_disks()?;
    if !initiator_found {
        println!("  No connected remote NVMe-oF fabric disks.");
    }

    Ok(())
}

/// Replay the share ledger (`restore-shares`). N1 semantics (PR-plan
/// PR 1 transitional contract + §6.4 laws 4/6):
///
/// * **nvmet records** replay through the pre-rebuild configfs path
///   (including its collision-prone port allocator — accepted for this
///   one-PR window; N2 replaces the path): `active` + already-live ⇒
///   verified no-op; `active` + gone ⇒ re-shared; `pending` + live ⇒
///   finalized with a loud line; `pending` + no live objects ⇒
///   garbage-collected loud; `removing` ⇒ teardown resumed, record
///   deleted.
/// * **spdk records are not replayed at this milestone** — SPDK restore
///   rides the SPDK-native `save_config`/`load_config` truth from the
///   SPDK rebuild (N3/N4) on; records are kept for ownership/dispatch
///   and each skip says so loudly.
///
/// Per-record failures are collected (a report, not a first-failure
/// bail); any failure makes the verb exit nonzero.
pub fn restore_shares() -> io::Result<()> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let records = ledger.load()?;
    if records.is_empty() {
        log::info!("No NVMe-oF target shares to restore.");
        return Ok(());
    }
    log::info!("Restoring {} NVMe-oF target share(s)...", records.len());

    let mut failures = 0usize;
    for record in &records {
        if record.stack == StackKind::Spdk {
            log::warn!(
                "restore-shares: not replaying SPDK share '{}' at this milestone — SPDK \
                 restore rides SPDK-native save_config/load_config from the SPDK rebuild \
                 (design PR N3/N4); the ledger record is kept for ownership and unshare \
                 dispatch",
                record.subnqn
            );
            continue;
        }
        if let Err(e) = restore_nvmet_record(&ledger, record) {
            failures += 1;
            log::error!(
                "Failed to restore target share for {}: {:?}",
                record.subnqn,
                e
            );
        }
    }

    if failures > 0 {
        return Err(io::Error::other(format!(
            "{failures} of {} ledger share(s) failed to restore — see the log for the \
             per-share report",
            records.len()
        )));
    }
    Ok(())
}

fn restore_nvmet_record(ledger: &Ledger, record: &ShareRecord) -> io::Result<()> {
    let sub_dir = configfs_path().join("subsystems").join(&record.subnqn);
    match record.state {
        ShareState::Removing => {
            // §6.4 law 6: an interrupted unshare is resumed, not revived.
            log::warn!(
                "restore-shares: resuming interrupted teardown of '{}' (removing intent)",
                record.subnqn
            );
            unshare_nvmet_via_configfs(&record.subnqn, true)?;
            if let Some(loop_dev) = &record.loop_device {
                detach_recorded_loop(loop_dev);
            }
            ledger.delete(&record.subnqn)
        }
        ShareState::Pending => {
            if sub_dir.exists() {
                // Live objects exist and match ⇒ the crash happened after
                // the mutations completed: finalize the intent.
                log::warn!(
                    "restore-shares: finalizing crash-window pending intent '{}' — its live \
                     objects exist",
                    record.subnqn
                );
                ledger.finalize_share(&record.subnqn)
            } else {
                // No live objects ⇒ the share verb never completed and
                // never returned success: garbage-collect the intent.
                log::warn!(
                    "restore-shares: garbage-collecting pending intent '{}' — no live objects \
                     (the interrupted share never completed)",
                    record.subnqn
                );
                ledger.delete(&record.subnqn)
            }
        }
        ShareState::Active => {
            if sub_dir.exists() {
                // §6.4 law 4: an already-live share is a verified no-op.
                log::info!(
                    "restore-shares: '{}' is already live — verified no-op",
                    record.subnqn
                );
                return Ok(());
            }
            if PathBuf::from(&record.backing_path).is_file() {
                nocow::ensure_nocow_backing(Path::new(&record.backing_path))?;
            }
            let loop_device =
                share_nvmet_via_configfs(&record.subnqn, &record.backing_path, &record.listeners)?;
            // The loop device may differ across boots — refresh law-5
            // bookkeeping.
            if loop_device != record.loop_device {
                ledger.set_loop_device(&record.subnqn, loop_device)?;
            }
            log::info!("restore-shares: re-shared '{}'", record.subnqn);
            Ok(())
        }
    }
}

pub fn spdk_install() -> io::Result<()> {
    check_root()?;
    println!("Installing SPDK dependencies and compiling from source...");
    if is_mock() {
        println!("MOCK: Cloning spdk, running pkgdep.sh, configuring, and building via make.");
        return Ok(());
    }

    // 1. Clone
    println!("Cloning SPDK repo to /opt/spdk...");
    let status = Command::new("git")
        .args(["clone", "https://github.com/spdk/spdk.git", "/opt/spdk"])
        .status()?;
    if !status.success() {
        println!(
            "SPDK repo already exists at /opt/spdk or git clone failed. Proceeding with update..."
        );
    }

    let status = Command::new("git")
        .current_dir("/opt/spdk")
        .args(["submodule", "update", "--init"])
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Failed to update SPDK submodules"));
    }

    println!("Running pkgdep.sh to install system dependencies...");
    let status = Command::new("./scripts/pkgdep.sh")
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Failed to install SPDK dependencies"));
    }

    // Install Python dependencies (tabulate)
    println!("Installing required Python modules (tabulate)...");
    let pip_status = Command::new("pip3")
        .args(["install", "tabulate", "--break-system-packages"])
        .status();
    if pip_status.is_err() || !pip_status.unwrap().success() {
        let pip_status2 = Command::new("pip").args(["install", "tabulate"]).status();
        if pip_status2.is_err() || !pip_status2.unwrap().success() {
            let apt_status = Command::new("apt-get")
                .args(["install", "-y", "python3-tabulate"])
                .status();
            if apt_status.is_err() || !apt_status.unwrap().success() {
                println!(
                    "Warning: Could not install python 'tabulate' library. Compilation might fail."
                );
            }
        }
    }

    println!("Configuring SPDK...");
    let status = Command::new("./configure")
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Failed to configure SPDK"));
    }

    println!("Building SPDK (this may take a few minutes)...");
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let status = Command::new("make")
        .arg(format!("-j{}", cores))
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Failed to compile SPDK"));
    }

    println!("Successfully installed and compiled SPDK at /opt/spdk.");
    Ok(())
}

pub fn spdk_setup(hugepages_mb: usize) -> io::Result<()> {
    check_root()?;
    println!("Configuring hugepages ({}MB)...", hugepages_mb);
    if is_mock() {
        println!("MOCK: Configuring hugepages.");
        return Ok(());
    }

    // 1. Allocate hugepages via sysfs
    let pages = hugepages_mb / 2; // 2MB pages
    let nr_hugepages_path = "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages";
    if Path::new(nr_hugepages_path).exists() {
        fs::write(nr_hugepages_path, pages.to_string())?;
        println!("Successfully allocated {} x 2MB hugepages.", pages);
    } else {
        // Fallback to setup.sh config_huge
        let setup_script = "/opt/spdk/scripts/setup.sh";
        if Path::new(setup_script).exists() {
            let status = Command::new(setup_script)
                .arg("config_huge")
                .env("HUGEMEM", hugepages_mb.to_string())
                .status()?;
            if !status.success() {
                return Err(io::Error::other("SPDK setup.sh config_huge failed."));
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "SPDK setup.sh not found at /opt/spdk/scripts/setup.sh.",
            ));
        }
    }

    println!("Successfully configured hugepages.");
    Ok(())
}

pub fn spdk_bind(pci_addr: &str) -> io::Result<()> {
    check_root()?;
    println!(
        "Binding device at PCI address {} to SPDK user-space driver...",
        pci_addr
    );
    if is_mock() {
        println!("MOCK: Binding device {} to SPDK.", pci_addr);
        return Ok(());
    }

    let setup_script = "/opt/spdk/scripts/setup.sh";
    if Path::new(setup_script).exists() {
        let status = Command::new(setup_script)
            .arg("bind")
            .arg(pci_addr)
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "Failed to bind device {} using SPDK setup.sh",
                pci_addr
            )));
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "SPDK setup.sh not found. Run 'squeezefs storage nvmeof spdk-install' first.",
        ));
    }

    println!("Successfully bound device {} to SPDK.", pci_addr);
    Ok(())
}

pub fn spdk_unbind(pci_addr: &str) -> io::Result<()> {
    check_root()?;
    println!("Unbinding device at PCI address {} from SPDK...", pci_addr);
    if is_mock() {
        println!("MOCK: Unbinding device {} from SPDK.", pci_addr);
        return Ok(());
    }

    // Unbind from SPDK driver (vfio-pci or uio_pci_generic) via sysfs
    let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", pci_addr);
    if Path::new(&unbind_path).exists() {
        let _ = fs::write(&unbind_path, pci_addr);
    }

    // Trigger driver probe to return it to the kernel NVMe driver
    let probe_path = "/sys/bus/pci/drivers_probe";
    if Path::new(probe_path).exists() {
        let _ = fs::write(probe_path, pci_addr);
    }

    println!("Successfully unbound device {} from SPDK.", pci_addr);
    Ok(())
}

pub fn spdk_start() -> io::Result<()> {
    check_root()?;
    println!("Starting SPDK NVMe-oF target daemon (nvmf_tgt)...");
    if is_mock() {
        println!("MOCK: Spawning /opt/spdk/build/bin/nvmf_tgt in background.");
        return Ok(());
    }

    let bin_path = "/opt/spdk/build/bin/nvmf_tgt";
    if !Path::new(bin_path).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "SPDK target binary not found. Run 'squeezefs storage nvmeof spdk-install' first.",
        ));
    }

    // Check if nvmf_tgt is already running
    let check = Command::new("pgrep").arg("nvmf_tgt").status();
    if let Ok(status) = check {
        if status.success() {
            println!("SPDK target daemon (nvmf_tgt) is already running.");
            return Ok(());
        }
    }

    // Spawn daemon in background
    let child = Command::new(bin_path)
        .arg("-i")
        .arg("0")
        .arg("-m")
        .arg("0x1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    println!("Spawned SPDK nvmf_tgt in background (PID: {}).", child.id());
    println!("JSON-RPC socket listening at /var/tmp/spdk.sock");
    Ok(())
}
