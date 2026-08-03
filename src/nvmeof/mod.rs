//! NVMe-oF target + initiator management (`squeezefs nvmeof …`).
//!
//! N4 state of the dual-stack target-management program
//! (`docs/design-nvmeof-target-management.md`, PRs 1–4): the kernel-nvmet
//! target path is **rebuilt** (`nvmet::NvmetStack` — no fake configfs
//! files, checked errors, `resv_enable` + `device_uuid` before enable,
//! reserved-range port allocator, loop handling via the ledger) and the
//! SPDK stack is **fully live** (`spdk::SpdkStack` — `save_config`-backed
//! share/unshare/restore with pinned nsid + ns UUID + `ptpl_file`, plus
//! the N3 lifecycle verbs), both behind the finalized `TargetStack`
//! trait (`stack.rs`), under the top-level `squeezefs nvmeof` grammar
//! (§6.2: `--target-stack` default **spdk**, explicit selection, loud
//! failure — never a silent cross-stack fallback). Stack dispatch and
//! the **cross-stack live-state duplicate guard** live HERE, never
//! inside a stack subtree (the G3 module-graph rule). The pre-rebuild
//! transitional paths (the collision-prone configfs share path, the raw
//! SPDK RPC paths, the `spdk-*` lifecycle verbs, the silent sparse
//! auto-create, and every `SQUEEZEFS_MOCK_NVMEOF*` fork) are
//! **deleted** — the §6.8 zero-mock policy: unit tests ride injection
//! seams (explicit configfs roots, relocated state dirs, fake RPC
//! servers on real sockets), never env behavior forks; correctness
//! claims for target serving come from the real-kernel tiers.
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

pub mod fabric;
pub mod initiator;
pub mod ledger;
pub mod nocow;
pub mod nvmet;
pub mod spdk;
pub mod stack;

pub use initiator::{connect_target, disconnect_target, ConnectOptions};

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use ledger::Ledger;
use stack::{
    AdoptClass, AdoptedFrom, Listener, LiveShare, NvmeofError, PreflightOp, RestoreOutcome,
    ShareRecord, ShareRequest, ShareState, TargetStack,
};

/// The N2+ ownership NQN prefix (§6.2): default `share` NQNs are minted
/// under it, and — because ownership is ledger membership, never a
/// prefix — its only other job is CLASSIFICATION of unledgered live
/// objects (an unledgered product-prefix NQN is the ledger-loss shape,
/// §6.10).
pub const OWNERSHIP_NQN_PREFIX: &str = "nqn.2026-07.io.squeezefs:share-";

/// The pre-rebuild binary's default NQN domain (`subsystem-…` /
/// `spdk-subsystem-…` — src/nvmeof.rs @ 49ad606): the §6.10
/// provenance-classification heuristic for pre-rebuild shares.
pub const PRE_REBUILD_NQN_PREFIX: &str = "nqn.2026-06.io.squeezefs:";

/// Which target stack owns a share for its lifetime
/// (`docs/design-nvmeof-target-management.md` §6.1/§6.4 — the ledger's
/// `"stack"` field; a share is owned by exactly one stack, dispatch is
/// resolved from the ledger, never guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

/// Constructs the selected stack for the share verbs (real on BOTH arms
/// from N4 on — the N2/N3 interim SPDK milestone refusal is dead; the
/// §Migration pt 3 unavailability window is closed). The two verb-layer
/// options ride the SPDK stack only: `accept_version_drift` gates
/// preflight rung 4 (§6.2 flag placement — the nvmet arm refuses the
/// flag before this constructor runs), `unshare_force` overrides the R7
/// live-consumer refusal.
fn stack_for(
    kind: StackKind,
    accept_version_drift: bool,
    unshare_force: bool,
) -> Result<Box<dyn TargetStack>, NvmeofError> {
    match kind {
        StackKind::Nvmet => Ok(Box::new(nvmet::NvmetStack::open_default()?)),
        StackKind::Spdk => Ok(Box::new(spdk::SpdkStack::open_default(
            accept_version_drift,
            unshare_force,
        ))),
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

pub(crate) fn canonical_or_raw(path: &str) -> String {
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
    /// §6.2 flag placement: proceed against a version-drifted SPDK
    /// target (preflight rung 4). SPDK-only — explicit nvmet selection
    /// refuses it loud.
    pub accept_version_drift: bool,
}

/// VAL-7g (pre-RC spec §3): validate one NQN-shaped **configfs path
/// component**.
///
/// The value is joined onto a configfs root to name an object directory,
/// and the object-removal helper's `ENOTEMPTY` fallback is
/// `remove_dir_all` on that path (`nvmet::remove_configfs_object`). The
/// pre-fix predicate rejected `/` and whitespace but NOT `.` or `..`, so
/// `..` addressed the configfs PARENT — the subsystems/ports collection —
/// and a single bad argument became a recursive delete of live kernel
/// target state. Dot-only and leading-dot components now refuse loud.
///
/// Dots INSIDE a component stay legal: that is the NQN grammar itself
/// (`nqn.2026-07.io.squeezefs:share-x`).
pub fn validate_nqn_component(what: &str, value: &str) -> Result<(), NvmeofError> {
    let refuse = |why: &str| {
        Err(NvmeofError::Refused(format!(
            "{what} '{value}' is not a valid NQN ({why} — it names a configfs object \
             directory, and the removal path's ENOTEMPTY fallback is a recursive delete \
             of it)"
        )))
    };
    if value.is_empty() {
        return refuse("must be non-empty");
    }
    if value.contains('/') || value.chars().any(char::is_whitespace) {
        return refuse("no '/' or whitespace");
    }
    // Path traversal: `.` is the directory itself, `..` is its PARENT, and
    // a leading dot is the hidden-name class that no legitimate NQN uses.
    if value.starts_with('.') {
        return refuse("must not start with '.' (no path-traversal or hidden components)");
    }
    Ok(())
}

/// Refusal for `--accept-version-drift` under an explicit nvmet
/// selection (§6.2 flag placement: the flag gates SPDK preflight rung 4
/// "wherever that rung runs" — the kernel target has no version to
/// drift, and SqueezeFS never silently ignores an explicit flag).
fn refuse_drift_flag_on_nvmet(
    kind: StackKind,
    accept_version_drift: bool,
) -> Result<(), NvmeofError> {
    if kind == StackKind::Nvmet && accept_version_drift {
        return Err(NvmeofError::Refused(
            "--accept-version-drift is SPDK-only: it gates the SPDK preflight's version-drift \
             rung, and the kernel nvmet target has no spdk_tgt version to drift — drop the \
             flag with --target-stack nvmet (SqueezeFS never silently ignores an explicit \
             flag)"
                .to_string(),
        ));
    }
    Ok(())
}

/// The `nvmeof share` verb (§6.2): grammar validation → stack resolution
/// → root → stack preflight → backing preparation → the cross-stack
/// live-state duplicate guard → the selected stack's share flow (intent
/// protocol + same-stack live duplicate guard inside).
pub fn share(opts: &ShareOptions) -> Result<ShareRecord, NvmeofError> {
    // Grammar rungs first (pure argument semantics — before root, before
    // any side effect).
    let kind = resolve_stack(opts.stack)?;
    validate_share_flags(kind, opts.nsid, opts.ns_uuid.as_deref())?;
    refuse_drift_flag_on_nvmet(kind, opts.accept_version_drift)?;
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

    let stack = stack_for(kind, opts.accept_version_drift, false)?;
    check_root()?;
    stack.preflight(PreflightOp::Share)?;
    retire_old_registry_once();

    prepare_backing(&opts.backing_path, opts.create_size)?;

    let subnqn = match &opts.subnqn {
        Some(s) => s.clone(),
        // The N2+ ownership-prefix default (§6.2) — distinguishable from
        // devsub/foreign objects; classification of unledgered live
        // objects only, never an ownership test.
        None => format!("{OWNERSHIP_NQN_PREFIX}{}", Uuid::new_v4()),
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
        nsid: opts.nsid,
        ns_uuid,
        listeners,
        allow_hosts: opts.allow_hosts.clone(),
    };

    // §6.4 cross-stack live-state duplicate guard (fully live at N4):
    // walk the OTHER stack before this stack mutates anything. (The
    // ledger half — which covers both stacks' records — and the
    // same-stack live walk run inside `stack.share`.)
    let other_kind = match kind {
        StackKind::Spdk => StackKind::Nvmet,
        StackKind::Nvmet => StackKind::Spdk,
    };
    let other_live = other_stack_live_state(other_kind)?;
    cross_stack_duplicate_guard(&request, other_kind, &other_live, &Ledger::open_default())?;

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
/// we did not record — the dev_substrate ownership law). `force`
/// overrides the SPDK live-consumer refusal (R7);
/// `accept_version_drift` gates SPDK preflight rung 4 — both are
/// SPDK-only and refuse loud on an nvmet-recorded NQN (never a silent
/// flag-ignore).
pub fn unshare(subnqn: &str, force: bool, accept_version_drift: bool) -> Result<(), NvmeofError> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    match ledger.find(subnqn).map_err(NvmeofError::Io)? {
        Some(record) => {
            if record.stack == StackKind::Nvmet {
                refuse_drift_flag_on_nvmet(record.stack, accept_version_drift)?;
                if force {
                    return Err(NvmeofError::Refused(format!(
                        "--force overrides the SPDK live-consumer refusal, and '{subnqn}' is \
                         recorded on the kernel nvmet stack (which exposes no per-subsystem \
                         consumer view — its unshare never refuses on consumers) — drop the \
                         flag (SqueezeFS never silently ignores an explicit flag)"
                    )));
                }
            }
            let stack = stack_for(record.stack, accept_version_drift, force)?;
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

/// Print one stack's restore report; returns (replayed, failures).
fn print_restore_report(report: &stack::RestoreReport) -> (usize, usize) {
    let mut failures = 0usize;
    for entry in &report.entries {
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
    (report.entries.len(), failures)
}

/// The `nvmeof restore` verb (§6.2): bare replays EVERY ledger record
/// into its recorded stack (both stacks touched when both have
/// records); `--target-stack X` filters, never retargets. Reconciles
/// §6.4 law-6 intents; per-share report; idempotent. The SPDK leg
/// additionally runs the §6.5 systemd-ExecStartPost half (RPC-live wait
/// + `load_config` onto an empty target) and — the §6.4 persistence law
/// — ends with `save_config` whenever reconciliation changed anything,
/// so an explicit `--target-stack spdk` runs even with zero records.
/// Exits nonzero when any record failed.
pub fn restore(filter: Option<StackKind>, accept_version_drift: bool) -> Result<(), NvmeofError> {
    if let Some(kind) = filter {
        refuse_drift_flag_on_nvmet(kind, accept_version_drift)?;
    }
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;

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
        let stack = stack_for(StackKind::Nvmet, false, false)?;
        stack.preflight(PreflightOp::Restore)?;
        let (n, f) = print_restore_report(&stack.restore(&nvmet_records)?);
        replayed += n;
        failures += f;
    }

    let spdk_records: Vec<ShareRecord> = records
        .iter()
        .filter(|r| r.stack == StackKind::Spdk)
        .cloned()
        .collect();
    // The SPDK leg runs when records exist OR the filter selects it
    // explicitly (the systemd ExecStartPost path must replay
    // tgt-config.json even before the first ledgered share).
    if selected(StackKind::Spdk) && (!spdk_records.is_empty() || filter == Some(StackKind::Spdk)) {
        let stack = stack_for(StackKind::Spdk, accept_version_drift, false)?;
        stack.preflight(PreflightOp::Restore)?;
        let (n, f) = print_restore_report(&stack.restore(&spdk_records)?);
        replayed += n;
        failures += f;
    }

    if replayed == 0 && records.is_empty() {
        println!("No NVMe-oF target shares to restore.");
    }
    if failures > 0 {
        return Err(NvmeofError::Refused(format!(
            "{failures} of {replayed} replayed ledger share(s) failed to restore — see the \
             per-share report above"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `nvmeof target …` verbs (§6.2/§6.5, PR 3/N3) — stack dispatch lives HERE,
// never inside a stack subtree (the G3 module-graph rule: src/nvmeof/spdk/
// carries no reference to nvmet items and vice versa).
// ---------------------------------------------------------------------------

/// `target start` options carried from the CLI (§6.2 grammar). The
/// `Option` fields distinguish explicitly-passed values from defaults —
/// SPDK-only flags with `--target-stack nvmet` refuse loud, never a
/// silent flag-ignore (the `--disk-cache-paths` precedent).
#[derive(Debug, Clone, Default)]
pub struct TargetStartOptions {
    pub core_mask: Option<String>,
    pub cores: Option<u32>,
    pub dpdk_mem_mb: Option<u64>,
    pub accept_version_drift: bool,
}

/// Refuse SPDK-only flags on the nvmet arm loud (never silently ignore).
fn refuse_spdk_only_flags(kind: StackKind, set_flags: &[(&str, bool)]) -> Result<(), NvmeofError> {
    if kind != StackKind::Nvmet {
        return Ok(());
    }
    let offending: Vec<&str> = set_flags
        .iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| *name)
        .collect();
    if offending.is_empty() {
        return Ok(());
    }
    Err(NvmeofError::Refused(format!(
        "{} {} SPDK-only: the kernel nvmet target has no spdk_tgt process, hugepages, or \
         reactor cores — drop the flag(s) with --target-stack nvmet (SqueezeFS never \
         silently ignores an explicit flag)",
        offending.join(", "),
        if offending.len() == 1 { "is" } else { "are" },
    )))
}

/// `nvmeof target install` (§6.5): SPDK-only by definition — pinned tag +
/// sha verified build into `/opt/squeezefs/spdk/<tag>/`; `--with-pkgdep`
/// is the explicit consent for system package mutation.
pub fn target_install(version: Option<&str>, with_pkgdep: bool) -> Result<(), NvmeofError> {
    // Grammar rung first (fires before root — the validate_share_flags
    // precedent), then root, then the build.
    spdk::lifecycle::validate_install_version(version)?;
    check_root()?;
    spdk::lifecycle::install(&spdk::SpdkPaths::resolve(), version, with_pkgdep)
}

/// `nvmeof target setup` (§6.5): SPDK — hugepage reservation with the
/// recorded-prior file (+ `--restore-prior` restore path); nvmet —
/// modprobe + configfs mount checks.
pub fn target_setup(
    stack_flag: Option<StackKind>,
    hugemem_mb: Option<u64>,
    restore_prior: bool,
) -> Result<(), NvmeofError> {
    let kind = resolve_stack(stack_flag)?;
    refuse_spdk_only_flags(
        kind,
        &[
            ("--hugemem-mb", hugemem_mb.is_some()),
            ("--restore-prior", restore_prior),
        ],
    )?;
    check_root()?;
    match kind {
        StackKind::Spdk => {
            let paths = spdk::SpdkPaths::resolve();
            let sysfs = Path::new(spdk::hugepages::HUGEPAGES_2M_SYSFS_DIR);
            if restore_prior {
                let out = spdk::hugepages::restore_prior(sysfs, &paths.spdk_state_dir())
                    .map_err(NvmeofError::Io)?;
                println!(
                    "hugepages restored to the recorded prior: nr_hugepages = {} (record \
                     cleared).",
                    out.achieved
                );
                return Ok(());
            }
            let mb = hugemem_mb.unwrap_or(spdk::hugepages::DEFAULT_HUGEMEM_MB);
            let out = spdk::hugepages::setup(
                sysfs,
                &paths.spdk_state_dir(),
                mb,
                spdk::hugepages::mem_available_kb(),
            )
            .map_err(NvmeofError::Io)?;
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
            if let Some(prior) = out.prior_recorded {
                println!(
                    "recorded prior nr_hugepages = {prior} (restore with 'squeezefs nvmeof \
                     target setup --restore-prior')"
                );
            }
            if out.verified_noop {
                println!(
                    "hugepage pool already holds {} × 2 MiB pages — verified no-op.",
                    out.achieved_pages
                );
            } else {
                println!(
                    "reserved {} × 2 MiB hugepages ({} MiB requested{}).",
                    out.achieved_pages,
                    mb,
                    if out.clamped_to_in_use {
                        ", clamped to in-use pages"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        StackKind::Nvmet => {
            let stack = nvmet::NvmetStack::open_default()?;
            stack.ensure_ready()?;
            println!("kernel nvmet target ready: modules loaded, configfs mounted.");
            Ok(())
        }
    }
}

/// `nvmeof target start` (§6.5): SPDK — preflighted spawn with pidfile +
/// RPC-liveness wait + `load_config`; nvmet — modprobe + ledger `restore`
/// (configfs is the "running target").
pub fn target_start(
    stack_flag: Option<StackKind>,
    opts: &TargetStartOptions,
) -> Result<(), NvmeofError> {
    let kind = resolve_stack(stack_flag)?;
    refuse_spdk_only_flags(
        kind,
        &[
            ("--core-mask", opts.core_mask.is_some()),
            ("--cores", opts.cores.is_some()),
            ("--dpdk-mem-mb", opts.dpdk_mem_mb.is_some()),
            ("--accept-version-drift", opts.accept_version_drift),
        ],
    )?;
    check_root()?;
    match kind {
        StackKind::Spdk => spdk::lifecycle::start(
            &spdk::SpdkPaths::resolve(),
            &spdk::lifecycle::StartOptions {
                core_mask: opts.core_mask.clone(),
                cores: opts.cores,
                dpdk_mem_mb: opts
                    .dpdk_mem_mb
                    .unwrap_or(spdk::lifecycle::DEFAULT_DPDK_MEM_MB),
                accept_version_drift: opts.accept_version_drift,
            },
        ),
        StackKind::Nvmet => {
            let stack = nvmet::NvmetStack::open_default()?;
            stack.ensure_ready()?;
            println!(
                "kernel nvmet target ready (configfs is the running target) — replaying the \
                 share ledger:"
            );
            restore(Some(StackKind::Nvmet), false)
        }
    }
}

/// `nvmeof target stop` (§6.5): SPDK — `save_config` → SIGTERM by pidfile
/// → grace → SIGKILL; refuses while ledger shares are live-connected
/// unless `--force`. nvmet — refuses loud (the kernel target is not a
/// process; grammar-class refusal, before root).
pub fn target_stop(stack_flag: Option<StackKind>, force: bool) -> Result<(), NvmeofError> {
    let kind = resolve_stack(stack_flag)?;
    if kind == StackKind::Nvmet {
        return Err(NvmeofError::Refused(
            "the kernel nvmet target is not a process — there is nothing to stop.\n  configfs \
             objects are the 'running target': tear shares down instead:\n    sudo squeezefs \
             nvmeof unshare <subnqn>\n  (modules stay loaded by policy)"
                .to_string(),
        ));
    }
    check_root()?;
    spdk::lifecycle::stop(&spdk::SpdkPaths::resolve(), force)
}

/// `nvmeof target status` (§6.9): the diagnostic verb — never refuses on
/// drift; reports.
pub fn target_status(stack_flag: Option<StackKind>, json: bool) -> Result<(), NvmeofError> {
    let kind = resolve_stack(stack_flag)?;
    check_root()?;
    match kind {
        StackKind::Spdk => spdk::lifecycle::status(&spdk::SpdkPaths::resolve(), json),
        StackKind::Nvmet => {
            let stack = nvmet::NvmetStack::open_default()?;
            let status = stack.target_status()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "stack": "nvmet",
                        "modules_present": status.modules_present,
                        "configfs_mounted": status.configfs_mounted,
                        "subsystems": status.subsystems,
                        "namespaces": status.namespaces,
                        "ports": status.ports,
                        "resv_enabled_namespaces": status.resv_enabled_namespaces,
                    }))
                    .map_err(|e| NvmeofError::Io(io::Error::other(e)))?
                );
            } else {
                println!("=== Kernel nvmet NVMe-oF Target Status ===");
                println!(
                    "  modules:   {}",
                    if status.modules_present {
                        "loaded"
                    } else {
                        "absent"
                    }
                );
                println!(
                    "  configfs:  {}",
                    if status.configfs_mounted {
                        "mounted"
                    } else {
                        "absent"
                    }
                );
                println!(
                    "  serving:   {} subsystem(s), {} namespace(s) ({} with PR/resv_enable), \
                     {} port(s)",
                    status.subsystems,
                    status.namespaces,
                    status.resv_enabled_namespaces,
                    status.ports
                );
            }
            Ok(())
        }
    }
}

/// `nvmeof target systemd-unit` (§6.5): emits a unit to stdout with
/// values baked at emission — never installs, mutates nothing, needs no
/// root (the dev_substrate precedent).
pub fn target_systemd_unit(
    stack_flag: Option<StackKind>,
    core_mask: Option<String>,
    cores: Option<u32>,
    dpdk_mem_mb: Option<u64>,
) -> Result<(), NvmeofError> {
    let kind = resolve_stack(stack_flag)?;
    refuse_spdk_only_flags(
        kind,
        &[
            ("--core-mask", core_mask.is_some()),
            ("--cores", cores.is_some()),
            ("--dpdk-mem-mb", dpdk_mem_mb.is_some()),
        ],
    )?;
    match kind {
        StackKind::Spdk => {
            let unit = spdk::lifecycle::systemd_unit(
                &spdk::SpdkPaths::resolve(),
                core_mask,
                cores,
                dpdk_mem_mb.unwrap_or(spdk::lifecycle::DEFAULT_DPDK_MEM_MB),
            )?;
            print!("{unit}");
            Ok(())
        }
        StackKind::Nvmet => {
            let exe = std::env::current_exe().map_err(NvmeofError::Io)?;
            print!("{}", spdk::lifecycle::render_nvmet_unit(&exe));
            Ok(())
        }
    }
}

/// Tolerantly gather the OTHER stack's live state for the cross-stack
/// duplicate guard: an absent stack serves nothing. The nvmet walk
/// tolerates an absent configfs tree by construction (empty); the SPDK
/// walk degrades to empty when the target is dead (its state is
/// process-resident — a stopped target serves nothing), with the note
/// surfaced. Real read errors on a PRESENT stack stay loud — a guard
/// that silently skips is no guard.
fn other_stack_live_state(other_kind: StackKind) -> Result<Vec<LiveShare>, NvmeofError> {
    match other_kind {
        StackKind::Nvmet => nvmet::NvmetStack::open_default()
            .map_err(NvmeofError::Io)?
            .live_shares(),
        StackKind::Spdk => {
            let (live, note) = spdk::SpdkStack::open_default(false, false).live_shares_tolerant();
            if let Some(note) = note {
                log::info!("cross-stack duplicate guard: {note}");
            }
            Ok(live)
        }
    }
}

/// The manual removal steps for a live holder on `holder_kind` (§6.4:
/// the refusal message IS the runbook).
fn manual_steps_for(holder_kind: StackKind, nqn: &str) -> String {
    match holder_kind {
        StackKind::Nvmet => format!(
            "    rm  {root}/ports/<id>/subsystems/{nqn}    (for each port linking it)\n    \
             echo 0 > {root}/subsystems/{nqn}/namespaces/1/enable\n    \
             rmdir {root}/subsystems/{nqn}/namespaces/1\n    \
             rmdir {root}/subsystems/{nqn}",
            root = nvmet::NVMET_CONFIGFS_ROOT
        ),
        StackKind::Spdk => {
            spdk::manual_removal_steps(&spdk::SpdkPaths::resolve().rpc_sock(), nqn, None)
        }
    }
}

/// The cross-stack half of the §6.4 live-state duplicate-backing guard
/// (fully live at N4): before a share on stack X mutates anything, the
/// verb layer walks the OTHER stack's live state (`other_kind` +
/// `other_live`) and refuses when the requested backing (or NQN) is
/// already served there — a path shared via nvmet must refuse an SPDK
/// share of the same canonical path and vice versa. The refusal message
/// IS the runbook: it names the live holder, its ledger classification,
/// and the exit (`unshare` for anything ledgered; the exact manual
/// configfs / rpc.py removal steps for foreign objects). Lives HERE —
/// never inside a stack subtree — per the G3 module-graph rule.
pub fn cross_stack_duplicate_guard(
    req: &ShareRequest,
    other_kind: StackKind,
    other_live: &[LiveShare],
    ledger: &Ledger,
) -> Result<(), NvmeofError> {
    for live in other_live {
        let materialized = !live.device_path.is_empty() || live.enabled;
        if !materialized {
            continue;
        }
        let nqn_clash = live.subnqn == req.subnqn;
        let backing_clash = (!live.backing_canonical.is_empty()
            && live.backing_canonical == req.backing_canonical)
            || (!live.device_path.is_empty()
                && (live.device_path == req.backing_path
                    || live.device_path == req.backing_canonical));
        if !nqn_clash && !backing_clash {
            continue;
        }
        let class = match ledger.find(&live.subnqn) {
            Ok(Some(rec)) => format!("managed — ledger state {}", rec.state.as_str()),
            Ok(None) => "foreign — not in the share ledger".to_string(),
            Err(e) => format!("unknown — share ledger unreadable: {e}"),
        };
        let exit = if class.starts_with("managed") {
            format!(
                "  unshare the holder first:\n    sudo squeezefs nvmeof unshare {}",
                live.subnqn
            )
        } else {
            format!(
                "  while the old object serves, either remove it first \
                 (docs/design-nvmeof-target-management.md §6.4):\n{}\n  or absorb it into \
                 management instead — writes only the ledger, the live object keeps serving \
                 (§6.10):\n    sudo squeezefs nvmeof adopt {}",
                manual_steps_for(other_kind, &live.subnqn),
                live.subnqn
            )
        };
        let what = if nqn_clash {
            format!("subsystem NQN '{}' is already live", req.subnqn)
        } else {
            format!("backing path '{}' is already served", req.backing_path)
        };
        return Err(NvmeofError::Refused(format!(
            "{what} on the {} target stack by live subsystem '{}' (classification: {class}) \
             — the same backing must never be double-served, across stacks included.\n{exit}",
            other_kind.as_str(),
            live.subnqn
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `nvmeof adopt <subnqn>` — foreign-share absorption (§6.10, PR 4b/N4b).
// Adopt probes BOTH stacks (stack auto-detected from where the subnqn
// lives), so the whole flow lives HERE per the G3 module-graph rule —
// never inside a stack subtree.
// ---------------------------------------------------------------------------

/// Known test-harness NQN markers (§6.10 `adopt_harness_owned`): the
/// dev-substrate prefix (`tests/dev_substrate.sh`), the PR 5 fidelity
/// tier's prefix, and the scoping rig's domain
/// (`nqn.2026-07.io.spdkscope:*`). Harness objects belong to their
/// harness's teardown — never absorb the test fabric.
pub const HARNESS_NQN_MARKERS: [&str; 3] = [":devsub-", ":fideli-", "spdkscope"];

/// Known test-harness nvmet port ids (§6.10 `adopt_harness_owned`):
/// dev_substrate's 52026 and the scoping rig's 52470/52471 — refused by
/// name; a share serving through the test fabric's port objects is the
/// test fabric's.
pub const HARNESS_NVMET_PORT_IDS: [u32; 3] = [52026, 52470, 52471];

/// One stack's live probe for the adopt flow (§6.10 pt 1). Production
/// (`adopt`) fills it from the real walkers (`NvmetStack::live_shares`,
/// `SpdkStack::live_shares_tolerant` + `aio_bdevs_tolerant`); the unit
/// tier injects snapshots — the same classification code runs over both
/// (§6.8: seams relocate inputs, never fork behavior).
pub struct AdoptProbe {
    pub kind: StackKind,
    pub live: Vec<LiveShare>,
    /// SPDK bare-bdev inventory (`name -> aio filename`) for the §6.4
    /// filename-scan half of `adopt_backing_duplicated`; empty on nvmet
    /// (configfs subsystems ARE the whole walk there).
    pub aio_bdevs: Vec<(String, String)>,
    /// The tolerant gather's loud note (e.g. "SPDK target not
    /// answering — a stopped target serves nothing"), woven into the
    /// `adopt_not_live` refusal so a dead-target probe is never a
    /// silent hole.
    pub note: Option<String>,
}

/// Provenance-class heuristic for an unledgered live NQN (§6.10 pt 3 /
/// §6.4 `adopted_from.class`). The product's own N2+ ownership prefix on
/// an unledgered object means the ledger was lost; the pre-rebuild
/// binary's default prefixes mean a pre-rebuild share; anything else is
/// foreign. Classification is informational provenance — never an
/// ownership test (ownership = ledger membership).
pub fn adopt_class_of(subnqn: &str) -> AdoptClass {
    if subnqn.starts_with(OWNERSHIP_NQN_PREFIX) {
        AdoptClass::LedgerLoss
    } else if subnqn.starts_with(PRE_REBUILD_NQN_PREFIX) {
        AdoptClass::PreRebuild
    } else {
        AdoptClass::Foreign
    }
}

/// One §6.10 pt-2 named refusal — the class token is machine/test
/// grep-able, the detail is the runbook.
fn adopt_refusal(class: &str, subnqn: &str, detail: String) -> NvmeofError {
    NvmeofError::Refused(format!("refusing to adopt '{subnqn}' [{class}]: {detail}"))
}

/// Reads one located live holder into the candidate `pending` record
/// (§6.10 pt 1 identity capture) and pushes the loud notes (recorded
/// nulls, out-of-range port ids, ptpl re-binds). Pure over the holder +
/// state dir; mutates nothing.
fn build_adopt_record(
    kind: StackKind,
    holder: &LiveShare,
    state_dir: &Path,
    notes: &mut Vec<String>,
) -> ShareRecord {
    // nvmet loop mapping: the live `device_path` is the loop node, the
    // canonical resolves to the operator's file — the record carries the
    // file as backing and the node as `loop_device` (§6.4 law 5:
    // teardown learns the association from the ledger).
    let is_loop_served = kind == StackKind::Nvmet
        && holder.device_path.starts_with("/dev/loop")
        && !holder.backing_canonical.is_empty()
        && holder.backing_canonical != holder.device_path;
    let (backing_path, loop_device) = if is_loop_served {
        (
            holder.backing_canonical.clone(),
            Some(holder.device_path.clone()),
        )
    } else {
        (holder.device_path.clone(), None)
    };
    let backing_canonical = if holder.backing_canonical.is_empty() {
        canonical_or_raw(&backing_path)
    } else {
        holder.backing_canonical.clone()
    };
    if holder.ns_uuid.is_none() {
        notes.push(format!(
            "the live object exposes no namespace identity ({}) — recorded null; \
             restart-identity stability requires a re-share under management (a restore \
             re-establish would mint a fresh uuid once)",
            match kind {
                StackKind::Nvmet => "device_uuid",
                StackKind::Spdk => "ns UUID",
            }
        ));
    }
    let (nsid, bdev_name, ptpl_file) = match kind {
        StackKind::Nvmet => {
            // Out-of-range port ids (e.g. a pre-rebuild share's
            // small-int id) are recorded AS-IS: the §6.6 link-free
            // teardown law covers their later removal — the reserved
            // range governs only allocation.
            let base = nvmet::NVMET_PORT_ID_BASE_DEFAULT;
            let end = base + nvmet::NVMET_PORT_ID_RANGE - 1;
            for l in &holder.listeners {
                if let Some(id) = l.nvmet_port_id {
                    if !(base..=end).contains(&id) {
                        notes.push(format!(
                            "listener {}:{} rides nvmet port id {id}, outside the reserved \
                             range [{base}, {end}] — recorded as-is; unshare removes the port \
                             object only when link-free (§6.6 teardown law)",
                            l.ip, l.port
                        ));
                    }
                }
            }
            (None, None, None)
        }
        StackKind::Spdk => {
            // Live nsid + serving bdev are recorded verbatim (teardown/
            // restore drive the RECORDED name). The ptpl posture is
            // probed against OUR state dir: a surviving
            // `spdk/ptpl/<uuid>.json` (the ledger-loss funnel) is
            // re-bound; anything else is a loud null (§6.10 pt 1).
            let ptpl = match holder.ns_uuid.as_deref() {
                Some(uuid) => {
                    let rel = format!("spdk/ptpl/{uuid}.json");
                    if state_dir.join(&rel).exists() {
                        notes.push(format!(
                            "existing reservation-persistence file {rel} re-bound to the \
                             adopted record (the ledger-loss funnel keeps PTPL)"
                        ));
                        Some(rel)
                    } else {
                        notes.push(
                            "no ptpl_file is known for this share — recorded null; \
                             PTPL/guard-persistence upgrade requires a re-share under \
                             management"
                                .to_string(),
                        );
                        None
                    }
                }
                None => None,
            };
            (
                holder.nsids.first().copied(),
                holder.bdev_name.clone(),
                ptpl,
            )
        }
    };
    let utc = ledger::utc_now_rfc3339();
    ShareRecord {
        subnqn: holder.subnqn.clone(),
        stack: kind,
        state: ShareState::Pending,
        backing_path,
        backing_canonical,
        nsid,
        ns_uuid: holder.ns_uuid.clone(),
        listeners: holder.listeners.clone(),
        bdev_name,
        ptpl_file,
        loop_device,
        created_utc: utc.clone(),
        allow_hosts: holder.allow_hosts.clone(),
        adopted_from: Some(AdoptedFrom {
            utc,
            class: adopt_class_of(&holder.subnqn),
        }),
    }
}

/// §6.10 pts 1–2: locate the live foreign object (exactly one stack;
/// `flag` disambiguates a both-stacks-live NQN), refuse loud on the six
/// named classes (`adopt_not_live` / `adopt_ambiguous` /
/// `adopt_already_ledgered` / `adopt_backing_duplicated` /
/// `adopt_harness_owned` / `adopt_shape_unsupported`), and read the live
/// object into a candidate `pending` record with `adopted_from`
/// provenance. Returns the candidate plus the loud notes (recorded
/// nulls, out-of-range port ids) for the caller to print — pure over the
/// injected probes; mutates nothing.
pub fn adopt_candidate(
    subnqn: &str,
    flag: Option<StackKind>,
    probes: &[AdoptProbe],
    ledger: &Ledger,
) -> Result<(ShareRecord, Vec<String>), NvmeofError> {
    // ---- locate (§6.10 pt 1): the flag filters WHERE adopt looks; the
    // guard scans below always see every probe. --------------------------
    let holders: Vec<(StackKind, &LiveShare)> = probes
        .iter()
        .filter(|p| flag.is_none_or(|f| p.kind == f))
        .flat_map(|p| {
            p.live
                .iter()
                .filter(|l| l.subnqn == subnqn)
                .map(move |l| (p.kind, l))
        })
        .collect();
    let (kind, holder) = match holders.as_slice() {
        [] => {
            let notes: String = probes
                .iter()
                .filter_map(|p| p.note.as_ref())
                .map(|n| format!("\n  note: {n}"))
                .collect();
            let filter_hint = if flag.is_some() {
                "\n  (--target-stack filtered the probe to that stack — drop the flag to \
                 auto-detect)"
            } else {
                ""
            };
            return Err(adopt_refusal(
                "adopt_not_live",
                subnqn,
                format!(
                    "the subsystem is live on neither target stack — adopt absorbs live \
                     foreign objects only.{filter_hint}\n  inspect live + ledger state:  sudo \
                     squeezefs nvmeof list\n  a ledgered-but-down share is `nvmeof restore` \
                     territory, never adopt{notes}"
                ),
            ));
        }
        [one] => *one,
        many => {
            let holders_txt: String = many
                .iter()
                .map(|(k, l)| {
                    format!(
                        "\n    {} stack: serving '{}' (backing {})",
                        k.as_str(),
                        l.device_path,
                        l.backing_canonical
                    )
                })
                .collect();
            return Err(adopt_refusal(
                "adopt_ambiguous",
                subnqn,
                format!(
                    "the NQN is live on BOTH target stacks — failing closed; adopt absorbs \
                     exactly one holder:{holders_txt}\n  disambiguate explicitly:  sudo \
                     squeezefs nvmeof adopt {subnqn} --target-stack <spdk|nvmet>"
                ),
            ));
        }
    };

    // ---- adopt_already_ledgered: NQN or backing, ANY intent state
    // (crash-window records belong to `restore`, active ones to
    // `unshare`; `begin_share` re-checks this atomically under the
    // ledger flock). ------------------------------------------------------
    if let Some(rec) = ledger.find(subnqn).map_err(NvmeofError::Io)? {
        let remediation = match rec.state {
            ShareState::Active => {
                "it is already managed — `nvmeof unshare`/`nvmeof restore` territory"
            }
            ShareState::Pending | ShareState::Removing => {
                "it is a crash-window intent record — `nvmeof restore` reconciles it \
                 (finalize / garbage-collect / resume), never adopt"
            }
        };
        return Err(adopt_refusal(
            "adopt_already_ledgered",
            subnqn,
            format!(
                "the NQN is already in the share ledger (stack {}, state {}); {remediation}",
                rec.stack.as_str(),
                rec.state.as_str()
            ),
        ));
    }
    if !holder.backing_canonical.is_empty() {
        let records = ledger.load().map_err(NvmeofError::Io)?;
        if let Some(rec) = records
            .iter()
            .find(|r| r.backing_canonical == holder.backing_canonical)
        {
            return Err(adopt_refusal(
                "adopt_already_ledgered",
                subnqn,
                format!(
                    "its backing '{}' is already recorded under subsystem '{}' (stack {}, \
                     state {}) — the same backing must never be double-served; `nvmeof \
                     restore` reconciles that record, `nvmeof unshare {}` removes it",
                    holder.backing_canonical,
                    rec.subnqn,
                    rec.stack.as_str(),
                    rec.state.as_str(),
                    rec.subnqn
                ),
            ));
        }
    }

    // ---- adopt_backing_duplicated: the §6.4 duplicate-guard laws apply
    // to adopt verbatim — live subsystems on BOTH stacks plus the SPDK
    // bare-bdev filename scan. --------------------------------------------
    for probe in probes {
        for other in &probe.live {
            if probe.kind == kind && other.subnqn == subnqn {
                continue; // the holder itself
            }
            let materialized = !other.device_path.is_empty() || other.enabled;
            if !materialized {
                continue;
            }
            let clash = (!holder.backing_canonical.is_empty()
                && other.backing_canonical == holder.backing_canonical)
                || (!holder.device_path.is_empty() && other.device_path == holder.device_path);
            if clash {
                return Err(adopt_refusal(
                    "adopt_backing_duplicated",
                    subnqn,
                    format!(
                        "another live object serves the same canonical backing '{}': \
                         subsystem '{}' on the {} stack — the same backing must never be \
                         double-served, across stacks included (§6.4); absorbing one of two \
                         same-backing servers would bless the double-serve.\n  remove one \
                         holder first (`nvmeof list` classifies both), then adopt or \
                         re-share",
                        holder.backing_canonical,
                        other.subnqn,
                        probe.kind.as_str()
                    ),
                ));
            }
        }
        for (name, filename) in &probe.aio_bdevs {
            if kind == StackKind::Spdk && holder.bdev_name.as_deref() == Some(name.as_str()) {
                continue; // the candidate's own serving bdev
            }
            if filename == &holder.backing_canonical
                || (!holder.device_path.is_empty() && filename == &holder.device_path)
            {
                return Err(adopt_refusal(
                    "adopt_backing_duplicated",
                    subnqn,
                    format!(
                        "SPDK bdev '{name}' already opens the same backing '{filename}' \
                         (attached to a subsystem or not) — the same backing must never be \
                         double-served (§6.4).\n  a foreign/orphaned bdev is removed \
                         manually:\n{}",
                        spdk::manual_removal_steps(
                            &spdk::SpdkPaths::resolve().rpc_sock(),
                            "<its subsystem, if any>",
                            Some(name)
                        )
                    ),
                ));
            }
        }
    }

    // ---- adopt_harness_owned: never absorb the test fabric. -------------
    for marker in HARNESS_NQN_MARKERS {
        if subnqn.contains(marker) {
            return Err(adopt_refusal(
                "adopt_harness_owned",
                subnqn,
                format!(
                    "the NQN carries the test-harness marker '{marker}' — harness objects \
                     belong to their harness's teardown (tests/dev_substrate.sh teardown / \
                     the scoping rig's teardown.sh), never to adoption"
                ),
            ));
        }
    }
    if kind == StackKind::Nvmet {
        for l in &holder.listeners {
            if let Some(id) = l.nvmet_port_id {
                if HARNESS_NVMET_PORT_IDS.contains(&id) {
                    return Err(adopt_refusal(
                        "adopt_harness_owned",
                        subnqn,
                        format!(
                            "it serves through nvmet port id {id} — a \
                             test-harness-reserved port (dev_substrate 52026 / scoping rig \
                             52470-52471); harness objects belong to their harness's \
                             teardown, never to adoption"
                        ),
                    ));
                }
            }
        }
    }

    // ---- adopt_shape_unsupported (§6.6 structural conventions). ---------
    match kind {
        StackKind::Nvmet => {
            if holder.nsids.is_empty() || holder.device_path.is_empty() {
                return Err(adopt_refusal(
                    "adopt_shape_unsupported",
                    subnqn,
                    format!(
                        "the live subsystem serves no materialized namespace (namespace \
                         index(es) {:?}, device_path '{}') — nothing absorbable; an empty \
                         shell is removed manually:\n{}",
                        holder.nsids,
                        holder.device_path,
                        manual_steps_for(kind, subnqn)
                    ),
                ));
            }
            if holder.nsids != [1] {
                return Err(adopt_refusal(
                    "adopt_shape_unsupported",
                    subnqn,
                    format!(
                        "the kernel-nvmet namespace index is structurally fixed at 1 (one \
                         namespace per subsystem — §6.6), but the live subsystem carries \
                         namespace index(es) {:?} — remediation is removal-first + re-share \
                         under management:\n{}",
                        holder.nsids,
                        manual_steps_for(kind, subnqn)
                    ),
                ));
            }
        }
        StackKind::Spdk => {
            if holder.nsids.len() != 1 {
                return Err(adopt_refusal(
                    "adopt_shape_unsupported",
                    subnqn,
                    format!(
                        "adopt supports exactly one namespace per subsystem, but the live \
                         subsystem carries {} (nsids {:?}) — remediation is removal-first + \
                         re-share under management:\n{}",
                        holder.nsids.len(),
                        holder.nsids,
                        manual_steps_for(kind, subnqn)
                    ),
                ));
            }
            if holder.device_path.is_empty() {
                return Err(adopt_refusal(
                    "adopt_shape_unsupported",
                    subnqn,
                    format!(
                        "its namespace does not resolve to a bdev_aio backing (bdev {}) — \
                         v1 serves kernel block nodes and files via bdev_aio only \
                         (§Non-Goals); remediation is removal-first + re-share under \
                         management:\n{}",
                        holder
                            .bdev_name
                            .as_deref()
                            .map(|b| format!("'{b}'"))
                            .unwrap_or_else(|| "<none>".to_string()),
                        manual_steps_for(kind, subnqn)
                    ),
                ));
            }
        }
    }
    if holder.listeners.is_empty() {
        // The §6.4 schema requires >= 1 listener — a subsystem with no
        // fabric presence has nothing an initiator can reach and nothing
        // the ledger can represent.
        return Err(adopt_refusal(
            "adopt_shape_unsupported",
            subnqn,
            format!(
                "the live subsystem exposes no listener (no fabric presence) — the share \
                 schema requires at least one; remediation is removal-first + re-share under \
                 management:\n{}",
                manual_steps_for(kind, subnqn)
            ),
        ));
    }

    // ---- candidate build (§6.10 pt 1 identity capture + loud notes). ----
    let mut notes = Vec::new();
    let record = build_adopt_record(kind, holder, ledger.state_dir(), &mut notes);
    record.validate().map_err(NvmeofError::Io)?;
    Ok((record, notes))
}

/// §6.10 pt 3 TOCTOU re-verify: the freshly re-probed live state must
/// still match the candidate record on every live-observable field
/// (backing, identity, listeners incl. nvmet port ids, namespace shape,
/// allow-hosts). Drift = `Err(reason)` — the caller aborts loud and
/// garbage-collects the pending intent.
pub fn adopt_verify_unchanged(
    candidate: &ShareRecord,
    live_now: &[LiveShare],
) -> Result<(), String> {
    let Some(live) = live_now.iter().find(|l| l.subnqn == candidate.subnqn) else {
        return Err(format!(
            "subsystem '{}' vanished from live state",
            candidate.subnqn
        ));
    };
    // Backing: the recorded device mapping must still hold (loop-served
    // records recorded the node in `loop_device`).
    let expect_device = candidate
        .loop_device
        .as_deref()
        .unwrap_or(&candidate.backing_path);
    if live.device_path != expect_device {
        return Err(format!(
            "backing device changed: recorded '{expect_device}', live '{}'",
            live.device_path
        ));
    }
    if !live.backing_canonical.is_empty() && live.backing_canonical != candidate.backing_canonical {
        return Err(format!(
            "backing canonical changed: recorded '{}', live '{}'",
            candidate.backing_canonical, live.backing_canonical
        ));
    }
    match (candidate.ns_uuid.as_deref(), live.ns_uuid.as_deref()) {
        (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => {}
        (None, None) => {}
        (a, b) => return Err(format!("ns_uuid changed: recorded {a:?}, live {b:?}")),
    }
    let expect_nsids: Vec<u32> = match candidate.stack {
        StackKind::Nvmet => vec![1],
        StackKind::Spdk => vec![candidate.nsid.unwrap_or(1)],
    };
    if live.nsids != expect_nsids {
        return Err(format!(
            "namespace shape changed: recorded {expect_nsids:?}, live {:?}",
            live.nsids
        ));
    }
    if candidate.stack == StackKind::Spdk && live.bdev_name != candidate.bdev_name {
        return Err(format!(
            "serving bdev changed: recorded {:?}, live {:?}",
            candidate.bdev_name, live.bdev_name
        ));
    }
    let mut recorded: Vec<(String, u16, Option<u32>)> = candidate
        .listeners
        .iter()
        .map(|l| (l.ip.clone(), l.port, l.nvmet_port_id))
        .collect();
    let mut observed: Vec<(String, u16, Option<u32>)> = live
        .listeners
        .iter()
        .map(|l| (l.ip.clone(), l.port, l.nvmet_port_id))
        .collect();
    recorded.sort();
    observed.sort();
    if recorded != observed {
        return Err(format!(
            "listener set changed: recorded {recorded:?}, live {observed:?}"
        ));
    }
    let mut recorded_hosts = candidate.allow_hosts.clone();
    let mut observed_hosts = live.allow_hosts.clone();
    recorded_hosts.sort();
    observed_hosts.sort();
    if recorded_hosts != observed_hosts {
        return Err(format!(
            "allow-hosts set changed: recorded {recorded_hosts:?}, live {observed_hosts:?}"
        ));
    }
    Ok(())
}

/// The adopt flow over explicit stacks + ledger (the §6.8 injection
/// seam: the unit tier drives injected stacks/fake targets through
/// exactly this production path). Probes both stacks → classify
/// (`adopt_candidate`) → absorb via the intent protocol (§6.4 law 6:
/// `begin_share(pending)` → TOCTOU re-verify → SPDK truth-capture
/// `save_config` → `finalize_share(active)`). **Mutates no target
/// state** on any path — drift aborts delete only the pending ledger
/// record.
pub fn adopt_over(
    subnqn: &str,
    flag: Option<StackKind>,
    ledger: &Ledger,
    nvmet_stack: &nvmet::NvmetStack,
    spdk_stack: &spdk::SpdkStack,
) -> Result<ShareRecord, NvmeofError> {
    // §6.10 pt 1: probe BOTH stacks — locate honors the flag inside the
    // classification, the duplicate-guard scans always see everything.
    // The nvmet walk is strict (a present-but-unreadable tree stays loud;
    // an absent tree walks empty); the SPDK gather is tolerant (a dead
    // target serves nothing — the note is woven into refusals).
    let nvmet_live = nvmet_stack.live_shares()?;
    let (spdk_live, spdk_note) = spdk_stack.live_shares_tolerant();
    let probes = [
        AdoptProbe {
            kind: StackKind::Nvmet,
            live: nvmet_live,
            aio_bdevs: Vec::new(),
            note: None,
        },
        AdoptProbe {
            kind: StackKind::Spdk,
            live: spdk_live,
            aio_bdevs: spdk_stack.aio_bdevs_tolerant(),
            note: spdk_note,
        },
    ];
    let (candidate, notes) = adopt_candidate(subnqn, flag, &probes, ledger)?;
    for note in &notes {
        println!("note: {note}");
    }

    // §6.4 law 6: the pending intent (with `adopted_from` provenance) is
    // recorded before anything else; `begin_share` re-checks the
    // NQN/backing duplicate laws atomically under the ledger flock.
    ledger.begin_share(&candidate).map_err(NvmeofError::Io)?;

    // §6.10 pt 3 TOCTOU re-verify: re-probe the holder's stack fresh and
    // compare every live-observable field. Any failure here aborts loud
    // and garbage-collects the pending intent adopt itself just wrote —
    // target state is untouched on every path.
    let verified = match candidate.stack {
        StackKind::Nvmet => nvmet_stack.live_shares(),
        StackKind::Spdk => spdk_stack.live_shares(),
    }
    .and_then(|live_now| {
        adopt_verify_unchanged(&candidate, &live_now).map_err(|why| {
            NvmeofError::Refused(format!(
                "adopt of '{subnqn}' aborted: live state changed between classification and \
                 absorption (TOCTOU drift: {why}) — the pending intent was \
                 garbage-collected, no target state was touched; re-run adopt against the \
                 settled state"
            ))
        })
    });
    if let Err(e) = verified {
        if let Err(gc) = ledger.delete(&candidate.subnqn) {
            log::warn!(
                "adopt abort: could not garbage-collect the pending intent for '{}' ({gc}) — \
                 `nvmeof restore` reconciles it",
                candidate.subnqn
            );
        }
        return Err(e);
    }

    // §6.10 pt 4: SPDK truth capture — tgt-config.json must describe
    // what the target now serves under management (read-only RPCs + a
    // state-dir write; not a target mutation). It runs BEFORE finalize
    // (the law-6 pattern: active only after the verb's last step — a
    // crash window leaves a pending intent that `restore` finalizes AND
    // saves).
    if candidate.stack == StackKind::Spdk {
        spdk_stack.save_config()?;
    }
    ledger
        .finalize_share(&candidate.subnqn)
        .map_err(NvmeofError::Io)?;
    let mut record = candidate;
    record.state = ShareState::Active;
    log::info!(
        "nvmeof adopt: stack={} nqn={} class={} backing={} listeners={}",
        record.stack.as_str(),
        record.subnqn,
        record
            .adopted_from
            .as_ref()
            .map(|a| a.class.as_str())
            .unwrap_or("-"),
        record.backing_path,
        record
            .listeners
            .iter()
            .map(|l| format!("{}:{}#{}", l.ip, l.port, l.nvmet_port_id.unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(","),
    );
    Ok(record)
}

/// The `nvmeof adopt <subnqn>` verb (§6.2/§6.10): explicit operator
/// action absorbing a live foreign (unledgered) share into management by
/// writing ONLY the ledger — the live target object is untouched (the
/// two funneling scenarios, pre-rebuild shares and ledger loss, have
/// data serving that must not bounce). Stack auto-detected from where
/// the subnqn lives; `--target-stack` only disambiguates a
/// both-stacks-live NQN (never a retarget, and the env knob is
/// deliberately not consulted — detection is live-state truth).
pub fn adopt(subnqn: &str, flag: Option<StackKind>) -> Result<ShareRecord, NvmeofError> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let nvmet_stack = nvmet::NvmetStack::open_default().map_err(NvmeofError::Io)?;
    let spdk_stack = spdk::SpdkStack::open_default(false, false);
    adopt_over(subnqn, flag, &ledger, &nvmet_stack, &spdk_stack)
}

/// Reconciliation classification of one ledger record for `list` (§6.2)
/// — complete for BOTH stacks from N4 on.
fn classification_of(record: &ShareRecord, live: bool) -> &'static str {
    match (record.state, live) {
        (ShareState::Pending, _) => {
            "pending — interrupted share; `nvmeof restore` finalizes or garbage-collects it"
        }
        (ShareState::Removing, _) => {
            "removing — interrupted unshare; `nvmeof restore` resumes the teardown"
        }
        (ShareState::Active, true) => "managed",
        (ShareState::Active, false) => "down — restore candidate (`nvmeof restore`)",
    }
}

/// The `nvmeof list` verb (§6.2): ledger ∪ live-state reconciliation —
/// managed / down / pending / removing / foreign, complete for BOTH
/// stacks (N4) — plus the kept connected-fabric-disks section. The SPDK
/// gather is tolerant (a stopped target serves nothing — noted, never a
/// hard failure); the nvmet gather tolerates an absent configfs tree.
pub fn list(json: bool) -> Result<(), NvmeofError> {
    check_root()?;
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;

    let nvmet_stack = nvmet::NvmetStack::open_default()?;
    nvmet_stack.preflight(PreflightOp::List)?;
    let nvmet_live = nvmet_stack.live_shares()?;
    let (spdk_live, spdk_note) = spdk::SpdkStack::open_default(false, false).live_shares_tolerant();

    // (kind, share) for every live object, both stacks.
    let live: Vec<(StackKind, &LiveShare)> = nvmet_live
        .iter()
        .map(|l| (StackKind::Nvmet, l))
        .chain(spdk_live.iter().map(|l| (StackKind::Spdk, l)))
        .collect();
    // A record is "live" when its OWN stack serves its NQN.
    let live_by_key: HashMap<(StackKind, &str), &LiveShare> = live
        .iter()
        .map(|(kind, l)| ((*kind, l.subnqn.as_str()), *l))
        .collect();

    let connected = initiator::connected_fabric_disks().map_err(NvmeofError::Io)?;

    if json {
        let shares: Vec<serde_json::Value> = records
            .iter()
            .map(|r| {
                let is_live = live_by_key.contains_key(&(r.stack, r.subnqn.as_str()));
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
            .filter(|(_, l)| !records.iter().any(|r| r.subnqn == l.subnqn))
            .map(|(kind, l)| {
                serde_json::json!({
                    "stack": kind.as_str(),
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
    if let Some(note) = &spdk_note {
        println!("  ({note})");
    }
    if records.is_empty() {
        println!("  (no ledgered shares)");
    }
    for record in &records {
        let live_entry = live_by_key.get(&(record.stack, record.subnqn.as_str()));
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
        if let Some(adopted) = &record.adopted_from {
            println!(
                "  Adopted:        class {} at {}",
                adopted.class.as_str(),
                adopted.utc
            );
        }
        println!(
            "  State:          {}",
            classification_of(record, live_entry.is_some())
        );
        println!();
    }

    let foreign: Vec<&(StackKind, &LiveShare)> = live
        .iter()
        .filter(|(_, l)| !records.iter().any(|r| r.subnqn == l.subnqn))
        .collect();
    println!("=== Foreign / Unmanaged Live Subsystems (displayed, never touched) ===");
    if foreign.is_empty() {
        println!("  (none)");
    }
    for (kind, l) in foreign {
        println!("  NQN:     {}", l.subnqn);
        println!("  Stack:   {}", kind.as_str());
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
        println!(
            "  Device:  {}",
            d.device
                .as_deref()
                .unwrap_or("(no namespace block device visible yet)")
        );
        println!("  NQN:     {}", d.subnqn);
        println!("  Target:  {}", d.address);
        println!();
    }
    Ok(())
}
