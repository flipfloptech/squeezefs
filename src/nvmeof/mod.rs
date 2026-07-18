//! NVMe-oF target + initiator management (`squeezefs nvmeof …`).
//!
//! N2 state of the dual-stack target-management program
//! (`docs/design-nvmeof-target-management.md`, PR 2): the kernel-nvmet
//! target path is **rebuilt** (`nvmet::NvmetStack` — no fake configfs
//! files, checked errors, `resv_enable` + `device_uuid` before enable,
//! reserved-range port allocator, loop handling via the ledger) behind
//! the finalized `TargetStack` trait (`stack.rs`), and the CLI moved to
//! the top-level `squeezefs nvmeof` grammar (§6.2: `--target-stack`
//! default **spdk**, which **fails loud** here with the milestone
//! message until N3/N4 land SPDK lifecycle + sharing — the loud-fail UX
//! is itself a deliverable). The pre-rebuild transitional paths (the
//! collision-prone configfs share path, the raw SPDK RPC paths, the
//! `spdk-*` lifecycle verbs, the silent sparse auto-create, and every
//! `SQUEEZEFS_MOCK_NVMEOF*` fork) are **deleted** — the §6.8 zero-mock
//! policy: unit tests ride injection seams (explicit configfs roots,
//! relocated state dirs), never env behavior forks; correctness claims
//! for target serving come from the real-kernel tiers.
//!
//! Kept: the client/initiator half (`initiator.rs` — binding decision 3)
//! and the NoCOW guard (`nocow.rs`). Ownership is **ledger membership**,
//! never NQN prefix: N1-era records keep old-style NQNs and remain fully
//! managed; the `share-` prefix is only the classification heuristic for
//! unledgered live objects.
//!
//! io_uring note: everything here is one-shot mount-time/admin control
//! plane (configfs writes, nvme-cli/losetup shell-outs) — the sanctioned
//! `reservation.rs` precedent; no data path is touched.

pub mod initiator;
pub mod ledger;
pub mod nocow;
pub mod nvmet;
pub mod stack;

pub use initiator::{connect_target, disconnect_target};

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use ledger::Ledger;
use stack::{
    Listener, LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreOutcome, ShareRecord,
    ShareRequest, ShareState, TargetStack,
};

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
    if let Some(kind) = flag {
        return Ok(kind);
    }
    match std::env::var(TARGET_STACK_ENV) {
        Ok(v) if !v.is_empty() => v.parse::<StackKind>().map_err(|e| {
            NvmeofError::Refused(format!(
                "{TARGET_STACK_ENV}='{v}' is invalid: {e} — SqueezeFS never falls back to a \
                 default on a malformed selection"
            ))
        }),
        _ => Ok(StackKind::Spdk),
    }
}

/// The N2 SPDK loud-fail (§6.2/§6.3 + PR-plan PR 2): the default stack is
/// spdk, whose target management lands with the N3 (lifecycle) / N4
/// (share path) milestones — until then every SPDK-selected verb fails
/// LOUD with this designed preflight message. Only the remediation text
/// changes when N3/N4 land.
fn spdk_unavailable() -> PreflightError {
    PreflightError {
        message: "error: SPDK target stack unavailable: SPDK target management lands with the \
                  next milestone of this program (design milestones N3/N4 — \
                  docs/design-nvmeof-target-management.md)\n  select the kernel target stack \
                  explicitly:\n    sudo squeezefs nvmeof <verb> … --target-stack nvmet\n    \
                  (or export SQUEEZEFS_NVMEOF_TARGET_STACK=nvmet)\n  or install/start the SPDK \
                  target once available:\n    sudo squeezefs nvmeof target install    (lands \
                  with N3)\n    sudo squeezefs nvmeof target start      (lands with N3)\nnote: \
                  SqueezeFS never falls back between target stacks automatically —\n      they \
                  differ in reservation persistence (PTPL) and latency envelope."
            .to_string(),
    }
}

/// Constructs the selected stack. The SPDK arm is the N2 loud-fail; it
/// becomes a real `SpdkStack` at N3/N4 (§Migration pt 3: between N2 and
/// N4, SPDK target serving is deliberately unavailable on `dev`).
fn stack_for(kind: StackKind) -> Result<Box<dyn TargetStack>, NvmeofError> {
    match kind {
        StackKind::Nvmet => Ok(Box::new(nvmet::NvmetStack::open_default()?)),
        StackKind::Spdk => Err(NvmeofError::Preflight(spdk_unavailable())),
    }
}

pub(crate) fn check_root() -> io::Result<()> {
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

pub(crate) fn execute_cmd(cmd_name: &str, args: &[&str]) -> io::Result<String> {
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

/// First-mutating-verb hook of the §6.4 old-registry migration. Rides
/// the state-dir relocation seam (§6.8): a relocated
/// `SQUEEZEFS_NVMEOF_STATE_DIR` means "not this host's production state",
/// so the production-migration rename of the host's
/// `/etc/squeezefs/nvmeof_shares.json` is skipped — a test run must
/// never mutate live host files outside its relocated state surface.
fn retire_old_registry_once() {
    if std::env::var(ledger::STATE_DIR_ENV).is_ok_and(|v| !v.is_empty()) {
        return;
    }
    ledger::retire_old_registry(Path::new(ledger::OLD_REGISTRY_PATH));
}

fn canonical_or_raw(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
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
    if stack == StackKind::Nvmet {
        if let Some(n) = nsid {
            if n != 1 {
                return Err(NvmeofError::Refused(format!(
                    "--nsid is SPDK-only: the kernel-nvmet namespace index is structurally \
                     fixed at 1 (one namespace per subsystem — \
                     docs/design-nvmeof-target-management.md §6.6), got --nsid {n} with \
                     --target-stack nvmet.\n  drop the flag (or pass --nsid 1, the structural \
                     index); multi-namespace nvmet subsystems would be a schema-visible format \
                     change, never a silent flag reinterpretation"
                )));
            }
        }
    }
    if let Some(raw) = ns_uuid {
        Uuid::parse_str(raw).map_err(|e| {
            NvmeofError::Refused(format!(
                "--ns-uuid '{raw}' is not a valid UUID ({e}) — it seeds the recorded namespace \
                 identity on both stacks and must be well-formed"
            ))
        })?;
    }
    Ok(())
}

/// §6.2 backing preparation (both stacks): a missing path refuses loud
/// (the silent 1 GiB sparse auto-create is dead — `--create-size` is the
/// explicit opt-in), directories refuse, and regular-file backings get
/// the NoCOW guard (a btrfs-CoW backing silently downgrades O_DIRECT to
/// buffered and wedges the fabric under write load).
pub fn prepare_backing(backing_path: &str, create_size: Option<u64>) -> io::Result<()> {
    let path = PathBuf::from(backing_path);
    if !path.exists() {
        match create_size {
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "backing path '{backing_path}' does not exist — SqueezeFS never \
                         auto-creates backings (a typo must not conjure a device); pass \
                         --create-size <size> to create it as a sparse file explicitly"
                    ),
                ));
            }
            Some(size) => {
                let f = fs::File::create(&path).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("cannot create backing file '{backing_path}': {e}"),
                    )
                })?;
                f.set_len(size)?;
                println!("Created sparse backing file '{backing_path}' ({size} bytes).");
            }
        }
    }
    if path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "backing path '{backing_path}' is a directory — share a block device or a \
                     regular file"
            ),
        ));
    }
    if path.is_file() {
        nocow::ensure_nocow_backing(&path)?;
    }
    Ok(())
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

fn validate_nqn_component(what: &str, value: &str) -> Result<(), NvmeofError> {
    if value.is_empty() || value.contains('/') || value.chars().any(char::is_whitespace) {
        return Err(NvmeofError::Refused(format!(
            "{what} '{value}' is not a valid NQN (must be non-empty, no '/' or whitespace — it \
             names a configfs object)"
        )));
    }
    Ok(())
}

/// The `nvmeof share` verb (§6.2): grammar validation → stack resolution
/// (SPDK fails loud until N3/N4) → root → backing preparation → the
/// selected stack's share flow (intent protocol + live-state duplicate
/// guard inside).
pub fn share(opts: &ShareOptions) -> Result<ShareRecord, NvmeofError> {
    // Grammar rungs first (pure argument semantics — before root, before
    // any side effect).
    let kind = resolve_stack(opts.stack)?;
    validate_share_flags(kind, opts.nsid, opts.ns_uuid.as_deref())?;
    if opts.ips.is_empty() {
        return Err(NvmeofError::Refused(
            "at least one --ip listener address is required".to_string(),
        ));
    }
    let mut listeners = Vec::new();
    for ip in &opts.ips {
        nvmet::adrfam_of(ip)?; // loud on malformed addresses, both stacks
        let listener = Listener {
            ip: ip.clone(),
            port: opts.port,
            nvmet_port_id: None,
        };
        if listeners.contains(&listener) {
            return Err(NvmeofError::Refused(format!(
                "duplicate listener {ip}:{} — each (ip, port) pair may appear once",
                opts.port
            )));
        }
        listeners.push(listener);
    }
    for host in &opts.allow_hosts {
        validate_nqn_component("--allow-host", host)?;
    }
    if let Some(subnqn) = &opts.subnqn {
        validate_nqn_component("--subnqn", subnqn)?;
    }

    let stack = stack_for(kind)?;
    check_root()?;
    stack.preflight(PreflightOp::Share)?;
    retire_old_registry_once();

    prepare_backing(&opts.backing_path, opts.create_size)?;

    let subnqn = match &opts.subnqn {
        Some(s) => s.clone(),
        // The N2+ ownership-prefix default (§6.2) — distinguishable from
        // devsub/foreign objects; classification of unledgered live
        // objects only, never an ownership test.
        None => format!("nqn.2026-07.io.squeezefs:share-{}", Uuid::new_v4()),
    };
    let ns_uuid = match &opts.ns_uuid {
        Some(raw) => Uuid::parse_str(raw)
            .map_err(|e| NvmeofError::Refused(format!("--ns-uuid '{raw}': {e}")))?
            .to_string(),
        None => Uuid::new_v4().to_string(),
    };

    let request = ShareRequest {
        subnqn,
        backing_canonical: canonical_or_raw(&opts.backing_path),
        backing_path: opts.backing_path.clone(),
        ns_uuid,
        listeners,
        allow_hosts: opts.allow_hosts.clone(),
    };
    let record = stack.share(&request)?;
    log::info!(
        "nvmeof share: stack={} nqn={} backing={} ns_uuid={} listeners={} allow_hosts={}",
        record.stack.as_str(),
        record.subnqn,
        record.backing_path,
        record.ns_uuid.as_deref().unwrap_or("-"),
        record
            .listeners
            .iter()
            .map(|l| format!("{}:{}#{}", l.ip, l.port, l.nvmet_port_id.unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(","),
        record.allow_hosts.len(),
    );
    Ok(record)
}

/// The `nvmeof unshare` verb (§6.2): stack resolved from the ledger —
/// including `pending`/`removing` intent records (§6.4 law 6: a
/// crash-window share is still ours to remove); an NQN absent from the
/// ledger refuses loud with `list` guidance (we never tear down objects
/// we did not record — the dev_substrate ownership law).
pub fn unshare(subnqn: &str) -> Result<(), NvmeofError> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    match ledger.find(subnqn).map_err(NvmeofError::Io)? {
        Some(record) => {
            let stack = stack_for(record.stack)?;
            stack.preflight(PreflightOp::Unshare)?;
            stack.unshare(&record)?;
            Ok(())
        }
        None => Err(NvmeofError::Refused(format!(
            "subsystem '{subnqn}' is not in the share ledger — SqueezeFS never tears down \
             target objects it did not record (ownership = ledger membership).\n  inspect \
             managed + live state:  sudo squeezefs nvmeof list\n  pre-rebuild or foreign \
             kernel-nvmet objects are removed manually via configfs:\n    rm  \
             {root}/ports/<id>/subsystems/{subnqn}    (for each port linking it)\n    echo 0 > \
             {root}/subsystems/{subnqn}/namespaces/1/enable\n    rmdir \
             {root}/subsystems/{subnqn}/namespaces/1\n    rmdir {root}/subsystems/{subnqn}",
            root = nvmet::NVMET_CONFIGFS_ROOT
        ))),
    }
}

/// The `nvmeof restore` verb (§6.2): bare replays EVERY ledger record
/// into its recorded stack; `--target-stack X` filters, never retargets.
/// Reconciles §6.4 law-6 intents; per-share report; idempotent. Exits
/// nonzero when any record failed.
pub fn restore(filter: Option<StackKind>) -> Result<(), NvmeofError> {
    // An explicit SPDK filter selects a stack that cannot restore yet:
    // loud milestone failure (§Migration pt 3), not a silent no-op.
    // (When N4 lands, this gate is replaced by the real SPDK restore
    // dispatch — stack_for's SPDK arm stops failing.)
    if filter == Some(StackKind::Spdk) {
        stack_for(StackKind::Spdk)?;
    }
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;
    if records.is_empty() {
        println!("No NVMe-oF target shares to restore.");
        return Ok(());
    }

    // Bare `restore` replays every record into its RECORDED stack; a
    // filter selects records recorded for that stack, never retargets.
    let selected = |kind: StackKind| filter.is_none() || filter == Some(kind);

    let mut failures = 0usize;
    let mut replayed = 0usize;
    let nvmet_records: Vec<ShareRecord> = records
        .iter()
        .filter(|r| r.stack == StackKind::Nvmet)
        .cloned()
        .collect();
    if selected(StackKind::Nvmet) && !nvmet_records.is_empty() {
        let stack = stack_for(StackKind::Nvmet)?;
        stack.preflight(PreflightOp::Restore)?;
        let report = stack.restore(&nvmet_records)?;
        for entry in &report.entries {
            replayed += 1;
            let line = match &entry.outcome {
                RestoreOutcome::Restored => "restored".to_string(),
                RestoreOutcome::VerifiedNoop => "already live — verified no-op".to_string(),
                RestoreOutcome::FinalizedPending => {
                    "pending intent finalized (live objects exist)".to_string()
                }
                RestoreOutcome::GarbageCollectedPending => {
                    "pending intent garbage-collected (no live objects)".to_string()
                }
                RestoreOutcome::TeardownResumed => {
                    "interrupted teardown resumed; record removed".to_string()
                }
                RestoreOutcome::Skipped(why) => format!("skipped: {why}"),
                RestoreOutcome::Failed(why) => {
                    failures += 1;
                    format!("FAILED: {why}")
                }
            };
            println!("restore {}: {line}", entry.subnqn);
        }
    }

    if selected(StackKind::Spdk) {
        for rec in records.iter().filter(|r| r.stack == StackKind::Spdk) {
            println!(
                "restore {}: skipped — SPDK restore rides SPDK-native save_config/load_config \
                 from the SPDK milestones of this program (N3/N4); the ledger record is kept \
                 for ownership and unshare dispatch",
                rec.subnqn
            );
        }
    }

    if failures > 0 {
        return Err(NvmeofError::Refused(format!(
            "{failures} of {replayed} replayed ledger share(s) failed to restore — see the \
             per-share report above"
        )));
    }
    Ok(())
}

/// Reconciliation classification of one ledger record for `list` (§6.2).
fn classification_of(record: &ShareRecord, live: bool) -> &'static str {
    match (record.stack, record.state, live) {
        (StackKind::Spdk, _, _) => "unverified (SPDK stack management lands at N3/N4)",
        (_, ShareState::Pending, _) => {
            "pending — interrupted share; `nvmeof restore` finalizes or garbage-collects it"
        }
        (_, ShareState::Removing, _) => {
            "removing — interrupted unshare; `nvmeof restore` resumes the teardown"
        }
        (_, ShareState::Active, true) => "managed",
        (_, ShareState::Active, false) => "down — restore candidate (`nvmeof restore`)",
    }
}

/// The `nvmeof list` verb (§6.2): ledger ∪ live-state reconciliation —
/// managed / down / pending / removing / foreign — plus the kept
/// connected-fabric-disks section.
pub fn list(json: bool) -> Result<(), NvmeofError> {
    check_root()?;
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;

    let stack = nvmet::NvmetStack::open_default()?;
    stack.preflight(PreflightOp::List)?;
    let live = stack.live_shares()?;
    let live_by_nqn: HashMap<&str, &LiveShare> =
        live.iter().map(|l| (l.subnqn.as_str(), l)).collect();

    let connected = initiator::connected_fabric_disks().map_err(NvmeofError::Io)?;

    if json {
        let shares: Vec<serde_json::Value> = records
            .iter()
            .map(|r| {
                let is_live = live_by_nqn.contains_key(r.subnqn.as_str());
                let mut v = serde_json::to_value(r).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "classification".to_string(),
                        serde_json::Value::String(classification_of(r, is_live).to_string()),
                    );
                    obj.insert("live".to_string(), serde_json::Value::Bool(is_live));
                }
                v
            })
            .collect();
        let foreign: Vec<serde_json::Value> = live
            .iter()
            .filter(|l| !records.iter().any(|r| r.subnqn == l.subnqn))
            .map(|l| {
                serde_json::json!({
                    "subnqn": l.subnqn,
                    "device_path": l.device_path,
                    "backing_canonical": l.backing_canonical,
                    "ns_uuid": l.ns_uuid,
                    "enabled": l.enabled,
                    "listeners": l.listeners.iter().map(|x| serde_json::json!({
                        "ip": x.ip, "port": x.port, "nvmet_port_id": x.nvmet_port_id,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let connected_json: Vec<serde_json::Value> = connected
            .iter()
            .map(|d| {
                serde_json::json!({
                    "device": d.device, "subnqn": d.subnqn, "address": d.address,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "shares": shares,
                "foreign_live": foreign,
                "connected_fabric_disks": connected_json,
            }))
            .map_err(|e| NvmeofError::Io(io::Error::other(e)))?
        );
        return Ok(());
    }

    println!("=== Managed NVMe-oF Target Shares (ledger ∪ live state) ===");
    if records.is_empty() {
        println!("  (no ledgered shares)");
    }
    for record in &records {
        let live_entry = live_by_nqn.get(record.subnqn.as_str());
        println!("  NQN:            {}", record.subnqn);
        println!("  Stack:          {}", record.stack.as_str());
        println!(
            "  Backing:        {}{}",
            record.backing_path,
            record
                .loop_device
                .as_deref()
                .map(|l| format!(" (loop: {l})"))
                .unwrap_or_default()
        );
        if let Some(u) = &record.ns_uuid {
            println!("  NS UUID:        {u}");
        }
        println!(
            "  Listeners:      {}",
            record
                .listeners
                .iter()
                .map(|l| match l.nvmet_port_id {
                    Some(id) => format!("{}:{} (port id {id})", l.ip, l.port),
                    None => format!("{}:{}", l.ip, l.port),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !record.allow_hosts.is_empty() {
            println!("  Allowed hosts:  {}", record.allow_hosts.join(", "));
        }
        println!(
            "  State:          {}",
            classification_of(record, live_entry.is_some())
        );
        println!();
    }

    let foreign: Vec<&LiveShare> = live
        .iter()
        .filter(|l| !records.iter().any(|r| r.subnqn == l.subnqn))
        .collect();
    println!("=== Foreign / Unmanaged Live nvmet Subsystems (displayed, never touched) ===");
    if foreign.is_empty() {
        println!("  (none)");
    }
    for l in foreign {
        println!("  NQN:     {}", l.subnqn);
        println!(
            "  Serving: {} (backing {})",
            l.device_path, l.backing_canonical
        );
        println!(
            "  Listen:  {}",
            l.listeners
                .iter()
                .map(|x| format!("{}:{}", x.ip, x.port))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!();
    }

    println!("=== Connected Fabric Disks ===");
    if connected.is_empty() {
        println!("  No connected remote NVMe-oF fabric disks.");
    }
    for d in &connected {
        println!("  Device:  {}", d.device);
        println!("  NQN:     {}", d.subnqn);
        println!("  Target:  {}", d.address);
        println!();
    }
    Ok(())
}
