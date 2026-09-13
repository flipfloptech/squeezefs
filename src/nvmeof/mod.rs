//! NVMe-oF target + initiator management (`squeezefs nvmeof …`).
//!
//! The kernel `nvmet` target (the sqz-kernel target) is **THE** target
//! (owner ruling R-SYM-8, 2026-09-12 — `docs/design-symmetric-metadata.md`
//! §5.8.1, KD-SYM-23): SPDK was retired as a target, forward-only — its
//! compiled-in `SPDK_NVMF_MAX_NUM_REGISTRANTS = 16` was the only hard
//! registrant ceiling in anything SqueezeFS shipped. `nvmet::NvmetStack`
//! (`docs/design-nvmeof-target-management.md` §6.6 — no fake configfs
//! files, checked errors, `resv_enable` + `device_uuid` before enable,
//! reserved-range port allocator, loop handling via the ledger) is the
//! one implementation of the `TargetStack` trait (`stack.rs`), under the
//! top-level `squeezefs nvmeof` grammar (§6.2: `--target-stack` default
//! **nvmet**, explicit selection, loud failure). The retired `spdk`
//! spelling survives ONLY as a parse-and-refuse arm ([`StackKind::Spdk`])
//! so the refusal can name it, nvmet and the operator's re-share
//! sequence ([`SPDK_RESHARE_SEQUENCE`]); a ledger record the retired
//! stack wrote stays decodable, is LISTED with that sequence, and is never
//! re-presented (never started, restored or adopted onto anything). The
//! §6.8 zero-mock policy stands: unit tests ride injection seams
//! (explicit configfs roots, relocated state dirs), never env behavior
//! forks; correctness claims for target serving come from the real-kernel
//! fidelity tier.
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
    AdoptClass, AdoptedFrom, Listener, LiveShare, NvmeofError, PreflightOp, RestoreEntry,
    RestoreOutcome, ShareRecord, ShareRequest, ShareState, TargetStack,
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
///
/// `Spdk` is the RETIRED stack (R-SYM-8): it survives only so its ledger
/// records stay decodable and so the spelling can be refused by name —
/// no execution path constructs a stack for it.
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
            "nvmet" => Ok(StackKind::Nvmet),
            "spdk" => Err(format!(
                "target stack 'spdk' was RETIRED (R-SYM-8, 2026-09-12) — the kernel nvmet \
                 target is THE target; {SPDK_RESHARE_SEQUENCE}"
            )),
            other => Err(format!(
                "unknown target stack '{other}' (expected 'nvmet' — the one supported target)"
            )),
        }
    }
}

/// The operator's path off a retired SPDK share, spelled once so every
/// refusal, the `list` classification and the `restore` skip agree (a
/// macro so it also composes into `concat!`-built `&'static str`s).
macro_rules! spdk_reshare_sequence {
    () => {
        "re-share on nvmet:\n  \
         1. sudo squeezefs nvmeof unshare <subnqn>   (removes the SPDK ledger record ONLY — no \
         spdk_tgt is driven; the backing is released for step 3)\n  \
         2. if an spdk_tgt still serves the old subsystem, tear it down yourself \
         (rpc.py nvmf_delete_subsystem <subnqn>; rpc.py bdev_aio_delete <bdev>)\n  \
         3. sudo squeezefs nvmeof share <backing> --ip <ip> --target-stack nvmet   (nvmet is \
         the default; the flag is optional)"
    };
}

/// The operator's path off a retired SPDK share: unshare (ledger-only)
/// → manual spdk_tgt teardown if one still serves → share on nvmet.
pub const SPDK_RESHARE_SEQUENCE: &str = spdk_reshare_sequence!();

/// The forward-only R-SYM-8 refusal: `what` names the surface that asked
/// for SPDK (a flag, an env value, a verb arm).
pub fn spdk_retired_refusal(what: &str) -> NvmeofError {
    NvmeofError::Refused(format!(
        "{what}: SPDK was RETIRED as an NVMe-oF target (owner ruling R-SYM-8, 2026-09-12 — \
         docs/design-symmetric-metadata.md §5.8.1, forward-only): its compiled-in 16-registrant \
         cap was the only hard registrant ceiling SqueezeFS shipped. The kernel nvmet target is \
         THE target — nothing falls back to it silently.\n  {SPDK_RESHARE_SEQUENCE}"
    ))
}

/// Env half of the §6.2 stack-selection resolution order
/// (`--target-stack` flag > this env > default `nvmet`).
pub const TARGET_STACK_ENV: &str = "SQUEEZEFS_NVMEOF_TARGET_STACK";

/// §6.2 stack-selection resolution: flag > `SQUEEZEFS_NVMEOF_TARGET_STACK`
/// > default `nvmet`. The retired `spdk` selection refuses loud on either
/// rung (never a silent default); an unparseable env value refuses loud.
pub fn resolve_stack(flag: Option<StackKind>) -> Result<StackKind, NvmeofError> {
    match flag {
        Some(StackKind::Spdk) => return Err(spdk_retired_refusal("--target-stack spdk")),
        Some(StackKind::Nvmet) => return Ok(StackKind::Nvmet),
        None => {}
    }
    match std::env::var(TARGET_STACK_ENV) {
        Ok(v) if !v.is_empty() => v.parse::<StackKind>().map_err(|e| {
            NvmeofError::Refused(format!(
                "{TARGET_STACK_ENV}='{v}' is invalid: {e} — SqueezeFS never falls back to a \
                 default on a malformed selection"
            ))
        }),
        _ => Ok(StackKind::Nvmet),
    }
}

/// The one stack the verbs construct (the retired kind never reaches
/// here: `resolve_stack` refuses it and the ledger arms route SPDK records
/// to `retire_spdk_share` / `partition_restorable`).
fn nvmet_stack() -> Result<nvmet::NvmetStack, NvmeofError> {
    Ok(nvmet::NvmetStack::open_default()?)
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

/// §6.2 flag semantics: `--ns-uuid` seeds the recorded namespace
/// identity and must parse as a UUID; the nvmet namespace index is
/// structurally fixed at 1, so `--nsid` ≠ 1 refuses loud (the
/// `--disk-cache-paths` precedent: never a silent flag-ignore).
pub fn validate_share_flags(nsid: Option<u32>, ns_uuid: Option<&str>) -> Result<(), NvmeofError> {
    if let Some(n) = nsid {
        if n != 1 {
            return Err(NvmeofError::Refused(format!(
                "--nsid {n}: the kernel-nvmet namespace index is structurally fixed at 1 (one \
                 namespace per subsystem — docs/design-nvmeof-target-management.md §6.6).\n  \
                 drop the flag (or pass --nsid 1, the structural index); multi-namespace nvmet \
                 subsystems would be a schema-visible format change, never a silent flag \
                 reinterpretation"
            )));
        }
    }
    if let Some(raw) = ns_uuid {
        Uuid::parse_str(raw).map_err(|e| {
            NvmeofError::Refused(format!(
                "--ns-uuid '{raw}' is not a valid UUID ({e}) — it seeds the recorded namespace \
                 identity and must be well-formed"
            ))
        })?;
    }
    Ok(())
}

/// §6.2 backing preparation: a missing path refuses loud
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
    /// `--target-stack` (resolution: flag > env > default `nvmet`; the
    /// retired `spdk` refuses loud).
    pub stack: Option<StackKind>,
    /// `--nsid`: the nvmet index is structurally 1 — `Some(n)` with
    /// `n != 1` refuses loud.
    pub nsid: Option<u32>,
    /// Seeds the recorded namespace identity; generated when absent.
    pub ns_uuid: Option<String>,
    /// Explicit opt-in size (bytes) for creating a missing file backing.
    pub create_size: Option<u64>,
    /// Host-NQN allowlist; empty = allow-any (trusted-fabric default).
    pub allow_hosts: Vec<String>,
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

/// The `nvmeof share` verb (§6.2): grammar validation → stack resolution
/// → root → stack preflight → backing preparation → the stack's share
/// flow (intent protocol + the ledger and live duplicate-backing guards
/// inside — a backing a retired SPDK record still holds refuses there,
/// naming the `unshare` that is step 1 of the re-share sequence).
pub fn share(opts: &ShareOptions) -> Result<ShareRecord, NvmeofError> {
    // Grammar rungs first (pure argument semantics — before root, before
    // any side effect).
    resolve_stack(opts.stack)?;
    validate_share_flags(opts.nsid, opts.ns_uuid.as_deref())?;
    if opts.ips.is_empty() {
        return Err(NvmeofError::Refused(
            "at least one --ip listener address is required".to_string(),
        ));
    }
    let mut listeners = Vec::new();
    for ip in &opts.ips {
        nvmet::adrfam_of(ip)?; // loud on malformed addresses
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

    let stack = nvmet_stack()?;
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

/// Step 1 of the re-share sequence for a record the RETIRED SPDK stack
/// wrote: `removing` → delete on the ledger ONLY (§6.4 law 6 ordering
/// kept) — no spdk_tgt is driven, no target object is touched. Returns
/// the operator note naming the manual teardown and the nvmet re-share.
/// Refuses any other stack's record (those ride their stack's unshare).
pub fn retire_spdk_share(ledger: &Ledger, record: &ShareRecord) -> Result<String, NvmeofError> {
    if record.stack != StackKind::Spdk {
        return Err(NvmeofError::Refused(format!(
            "retire_spdk_share: '{}' is recorded on the {} stack — only retired-SPDK records \
             are removed ledger-only",
            record.subnqn,
            record.stack.as_str()
        )));
    }
    ledger
        .mark_removing(&record.subnqn)
        .map_err(NvmeofError::Io)?;
    ledger.delete(&record.subnqn).map_err(NvmeofError::Io)?;
    Ok(format!(
        "removed the share ledger record for '{}' (recorded on the RETIRED SPDK target stack — \
         R-SYM-8): the ledger entry ONLY was removed; SqueezeFS drives no spdk_tgt anymore, so \
         the target object was NOT torn down.\n  {SPDK_RESHARE_SEQUENCE}\n  (this was step 1; \
         backing '{}' is released for the nvmet share)",
        record.subnqn, record.backing_path
    ))
}

/// The `nvmeof unshare` verb (§6.2): stack resolved from the ledger —
/// including `pending`/`removing` intent records (§6.4 law 6: a
/// crash-window share is still ours to remove); an NQN absent from the
/// ledger refuses loud with `list` guidance (we never tear down objects
/// we did not record — the dev_substrate ownership law). A record the
/// retired SPDK stack wrote is removed ledger-only ([`retire_spdk_share`]).
pub fn unshare(subnqn: &str) -> Result<(), NvmeofError> {
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    match ledger.find(subnqn).map_err(NvmeofError::Io)? {
        Some(record) if record.stack == StackKind::Spdk => {
            let note = retire_spdk_share(&ledger, &record)?;
            println!("{note}");
            Ok(())
        }
        Some(record) => {
            let stack = nvmet_stack()?;
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

/// Split a ledger into the records `restore` replays (nvmet) and the
/// report lines for the ones it never re-presents: a record the RETIRED
/// SPDK stack wrote is `Skipped` LOUD with the re-share sequence — never
/// silently dropped, never replayed onto anything (R-SYM-8).
pub fn partition_restorable(records: Vec<ShareRecord>) -> (Vec<ShareRecord>, Vec<RestoreEntry>) {
    let mut replay = Vec::with_capacity(records.len());
    let mut skipped = Vec::new();
    for record in records {
        match record.stack {
            StackKind::Nvmet => replay.push(record),
            StackKind::Spdk => skipped.push(RestoreEntry {
                outcome: RestoreOutcome::Skipped(format!(
                    "recorded on the RETIRED SPDK target stack (R-SYM-8) — never re-presented; \
                     {SPDK_RESHARE_SEQUENCE}"
                )),
                subnqn: record.subnqn,
            }),
        }
    }
    (replay, skipped)
}

/// The `nvmeof restore` verb (§6.2): replays EVERY nvmet ledger record
/// (`--target-stack nvmet` is the only admissible filter — the retired
/// `spdk` refuses); reconciles §6.4 law-6 intents; per-share report;
/// idempotent. Records the retired SPDK stack wrote are reported as
/// skipped with the re-share sequence and never re-presented. Exits
/// nonzero when any record failed.
pub fn restore(filter: Option<StackKind>) -> Result<(), NvmeofError> {
    resolve_stack(filter)?;
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;
    let total = records.len();
    let (nvmet_records, skipped) = partition_restorable(records);

    let mut failures = 0usize;
    let mut replayed = 0usize;
    if !nvmet_records.is_empty() {
        let stack = nvmet_stack()?;
        stack.preflight(PreflightOp::Restore)?;
        let (n, f) = print_restore_report(&stack.restore(&nvmet_records)?);
        replayed += n;
        failures += f;
    }
    print_restore_report(&stack::RestoreReport { entries: skipped });

    if replayed == 0 && total == 0 {
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
// `nvmeof target …` verbs (§6.2/§6.5) — the kernel nvmet target is not a
// process: setup/start are readiness + ledger replay, stop refuses, status
// reports. The SPDK lifecycle (install/hugepages/pidfile/RPC) is RETIRED
// (R-SYM-8); `target install` survives only as a refusing verb.
// ---------------------------------------------------------------------------

/// `nvmeof target install`: RETIRED with SPDK (R-SYM-8) — it existed only
/// to build the pinned spdk_tgt. Refuses loud naming the successor (the
/// `removed_verb()` convention: exit nonzero, never stub success).
pub fn target_install() -> Result<(), NvmeofError> {
    Err(NvmeofError::Refused(format!(
        "`squeezefs nvmeof target install` was removed: it built the pinned SPDK release, and \
         SPDK was RETIRED as an NVMe-oF target (owner ruling R-SYM-8, 2026-09-12 — \
         docs/design-symmetric-metadata.md §5.8.1, forward-only). The kernel nvmet target \
         needs no install: its modules ship with the kernel. Superseded by `squeezefs nvmeof \
         target setup` (modprobe + configfs checks) and `squeezefs nvmeof target start` \
         (ledger replay).\n  {SPDK_RESHARE_SEQUENCE}"
    )))
}

/// `nvmeof target setup` (§6.5): modprobe + configfs mount checks.
pub fn target_setup(stack_flag: Option<StackKind>) -> Result<(), NvmeofError> {
    resolve_stack(stack_flag)?;
    check_root()?;
    let stack = nvmet_stack()?;
    stack.ensure_ready()?;
    println!("kernel nvmet target ready: modules loaded, configfs mounted.");
    Ok(())
}

/// `nvmeof target start` (§6.5): modprobe + ledger `restore` (configfs
/// is the "running target").
pub fn target_start(stack_flag: Option<StackKind>) -> Result<(), NvmeofError> {
    resolve_stack(stack_flag)?;
    check_root()?;
    let stack = nvmet_stack()?;
    stack.ensure_ready()?;
    println!(
        "kernel nvmet target ready (configfs is the running target) — replaying the share \
         ledger:"
    );
    restore(Some(StackKind::Nvmet))
}

/// `nvmeof target stop` (§6.5): refuses loud — the kernel target is not a
/// process (grammar-class refusal, before root).
pub fn target_stop(stack_flag: Option<StackKind>) -> Result<(), NvmeofError> {
    resolve_stack(stack_flag)?;
    Err(NvmeofError::Refused(
        "the kernel nvmet target is not a process — there is nothing to stop.\n  configfs \
         objects are the 'running target': tear shares down instead:\n    sudo squeezefs \
         nvmeof unshare <subnqn>\n  (modules stay loaded by policy)"
            .to_string(),
    ))
}

/// `nvmeof target status` (§6.9): the diagnostic verb — reports.
pub fn target_status(stack_flag: Option<StackKind>, json: bool) -> Result<(), NvmeofError> {
    resolve_stack(stack_flag)?;
    check_root()?;
    let stack = nvmet_stack()?;
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
            "  serving:   {} subsystem(s), {} namespace(s) ({} with PR/resv_enable), {} port(s)",
            status.subsystems, status.namespaces, status.resv_enabled_namespaces, status.ports
        );
    }
    Ok(())
}

/// `nvmeof target systemd-unit` (§6.5): emits the nvmet oneshot unit to
/// stdout with values baked at emission — never installs, mutates
/// nothing, needs no root (the dev_substrate precedent).
pub fn target_systemd_unit(stack_flag: Option<StackKind>) -> Result<(), NvmeofError> {
    resolve_stack(stack_flag)?;
    let exe = std::env::current_exe().map_err(NvmeofError::Io)?;
    print!("{}", nvmet::render_nvmet_unit(&exe));
    Ok(())
}

/// The manual removal steps for a live nvmet holder (§6.4: the refusal
/// message IS the runbook).
fn manual_steps_nvmet(nqn: &str) -> String {
    format!(
        "    rm  {root}/ports/<id>/subsystems/{nqn}    (for each port linking it)\n    \
         echo 0 > {root}/subsystems/{nqn}/namespaces/1/enable\n    \
         rmdir {root}/subsystems/{nqn}/namespaces/1\n    \
         rmdir {root}/subsystems/{nqn}",
        root = nvmet::NVMET_CONFIGFS_ROOT
    )
}

// ---------------------------------------------------------------------------
// `nvmeof adopt <subnqn>` — foreign-share absorption (§6.10, PR 4b/N4b).
// The flow probes the ONE stack's live state and writes only the ledger.
// ---------------------------------------------------------------------------

/// Known test-harness NQN markers (§6.10 `adopt_harness_owned`): the
/// dev-substrate prefix (`tests/dev_substrate.sh`), the PR 5 fidelity
/// tier's prefix, and the retired scoping rig's domain
/// (`nqn.2026-07.io.spdkscope:*` — objects a pre-retirement rig may have
/// left behind). Harness objects belong to their harness's teardown —
/// never absorb the test fabric.
pub const HARNESS_NQN_MARKERS: [&str; 3] = [":devsub-", ":fideli-", "spdkscope"];

/// Known test-harness nvmet port ids (§6.10 `adopt_harness_owned`):
/// dev_substrate's 52026 and the scoping rig's 52470/52471 — refused by
/// name; a share serving through the test fabric's port objects is the
/// test fabric's.
pub const HARNESS_NVMET_PORT_IDS: [u32; 3] = [52026, 52470, 52471];

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
/// nulls, out-of-range port ids). Pure over the holder; mutates nothing.
fn build_adopt_record(holder: &LiveShare, notes: &mut Vec<String>) -> ShareRecord {
    // Loop mapping: the live `device_path` is the loop node, the
    // canonical resolves to the operator's file — the record carries the
    // file as backing and the node as `loop_device` (§6.4 law 5:
    // teardown learns the association from the ledger).
    let is_loop_served = holder.device_path.starts_with("/dev/loop")
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
        notes.push(
            "the live object exposes no namespace identity (device_uuid) — recorded null; \
             restart-identity stability requires a re-share under management (a restore \
             re-establish would mint a fresh uuid once)"
                .to_string(),
        );
    }
    // Out-of-range port ids (e.g. a pre-rebuild share's small-int id) are
    // recorded AS-IS: the §6.6 link-free teardown law covers their later
    // removal — the reserved range governs only allocation.
    let base = nvmet::NVMET_PORT_ID_BASE_DEFAULT;
    let end = base + nvmet::NVMET_PORT_ID_RANGE - 1;
    for l in &holder.listeners {
        if let Some(id) = l.nvmet_port_id {
            if !(base..=end).contains(&id) {
                notes.push(format!(
                    "listener {}:{} rides nvmet port id {id}, outside the reserved range \
                     [{base}, {end}] — recorded as-is; unshare removes the port object only \
                     when link-free (§6.6 teardown law)",
                    l.ip, l.port
                ));
            }
        }
    }
    let utc = ledger::utc_now_rfc3339();
    ShareRecord {
        subnqn: holder.subnqn.clone(),
        stack: StackKind::Nvmet,
        state: ShareState::Pending,
        backing_path,
        backing_canonical,
        nsid: None,
        ns_uuid: holder.ns_uuid.clone(),
        listeners: holder.listeners.clone(),
        bdev_name: None,
        ptpl_file: None,
        loop_device,
        created_utc: utc.clone(),
        allow_hosts: holder.allow_hosts.clone(),
        adopted_from: Some(AdoptedFrom {
            utc,
            class: adopt_class_of(&holder.subnqn),
        }),
    }
}

/// §6.10 pts 1–2: locate the live foreign object in the nvmet walk,
/// refuse loud on the named classes (`adopt_not_live` /
/// `adopt_already_ledgered` / `adopt_backing_duplicated` /
/// `adopt_harness_owned` / `adopt_shape_unsupported`), and read the live
/// object into a candidate `pending` record with `adopted_from`
/// provenance. Returns the candidate plus the loud notes (recorded
/// nulls, out-of-range port ids) for the caller to print — pure over the
/// injected live snapshot; mutates nothing.
pub fn adopt_candidate(
    subnqn: &str,
    live: &[LiveShare],
    ledger: &Ledger,
) -> Result<(ShareRecord, Vec<String>), NvmeofError> {
    // ---- locate (§6.10 pt 1). --------------------------------------------
    let Some(holder) = live.iter().find(|l| l.subnqn == subnqn) else {
        return Err(adopt_refusal(
            "adopt_not_live",
            subnqn,
            "the subsystem is not live on the kernel nvmet target — adopt absorbs live foreign \
             objects only.\n  inspect live + ledger state:  sudo squeezefs nvmeof list\n  a \
             ledgered-but-down share is `nvmeof restore` territory, never adopt"
                .to_string(),
        ));
    };

    // ---- adopt_already_ledgered: NQN or backing, ANY intent state
    // (crash-window records belong to `restore`, active ones to
    // `unshare` — including a record the RETIRED SPDK stack wrote, whose
    // exit is the re-share sequence; `begin_share` re-checks this
    // atomically under the ledger flock). --------------------------------
    if let Some(rec) = ledger.find(subnqn).map_err(NvmeofError::Io)? {
        let remediation = match (rec.stack, rec.state) {
            (StackKind::Spdk, _) => {
                "it is a RETIRED-SPDK record — `nvmeof unshare` removes it (ledger-only), then \
                 `nvmeof share --target-stack nvmet` re-shares the backing; never adopt"
            }
            (_, ShareState::Active) => {
                "it is already managed — `nvmeof unshare`/`nvmeof restore` territory"
            }
            (_, ShareState::Pending | ShareState::Removing) => {
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
    // to adopt verbatim over every other live subsystem. -----------------
    for other in live {
        if other.subnqn == subnqn {
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
                    "another live subsystem serves the same canonical backing '{}': '{}' — \
                     the same backing must never be double-served (§6.4); absorbing one of \
                     two same-backing servers would bless the double-serve.\n  remove one \
                     holder first (`nvmeof list` classifies both), then adopt or re-share",
                    holder.backing_canonical, other.subnqn
                ),
            ));
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
                     tests/nvmeof_target_substrate.sh teardown), never to adoption"
                ),
            ));
        }
    }
    for l in &holder.listeners {
        if let Some(id) = l.nvmet_port_id {
            if HARNESS_NVMET_PORT_IDS.contains(&id) {
                return Err(adopt_refusal(
                    "adopt_harness_owned",
                    subnqn,
                    format!(
                        "it serves through nvmet port id {id} — a test-harness-reserved port \
                         (dev_substrate 52026 / scoping rig 52470-52471); harness objects \
                         belong to their harness's teardown, never to adoption"
                    ),
                ));
            }
        }
    }

    // ---- adopt_shape_unsupported (§6.6 structural conventions). ---------
    if holder.nsids.is_empty() || holder.device_path.is_empty() {
        return Err(adopt_refusal(
            "adopt_shape_unsupported",
            subnqn,
            format!(
                "the live subsystem serves no materialized namespace (namespace index(es) \
                 {:?}, device_path '{}') — nothing absorbable; an empty shell is removed \
                 manually:\n{}",
                holder.nsids,
                holder.device_path,
                manual_steps_nvmet(subnqn)
            ),
        ));
    }
    if holder.nsids != [1] {
        return Err(adopt_refusal(
            "adopt_shape_unsupported",
            subnqn,
            format!(
                "the kernel-nvmet namespace index is structurally fixed at 1 (one namespace \
                 per subsystem — §6.6), but the live subsystem carries namespace index(es) \
                 {:?} — remediation is removal-first + re-share under management:\n{}",
                holder.nsids,
                manual_steps_nvmet(subnqn)
            ),
        ));
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
                manual_steps_nvmet(subnqn)
            ),
        ));
    }

    // ---- candidate build (§6.10 pt 1 identity capture + loud notes). ----
    let mut notes = Vec::new();
    let record = build_adopt_record(holder, &mut notes);
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
    // The nvmet shape is structurally [1] (§6.6).
    if live.nsids != [1] {
        return Err(format!(
            "namespace shape changed: recorded [1], live {:?}",
            live.nsids
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

/// The adopt flow over an explicit stack + ledger (the §6.8 injection
/// seam: the unit tier drives an injected configfs root through exactly
/// this production path). Probe → classify (`adopt_candidate`) → absorb
/// via the intent protocol (§6.4 law 6: `begin_share(pending)` → TOCTOU
/// re-verify → `finalize_share(active)`). **Mutates no target state** on
/// any path — drift aborts delete only the pending ledger record.
pub fn adopt_over(
    subnqn: &str,
    ledger: &Ledger,
    nvmet_stack: &nvmet::NvmetStack,
) -> Result<ShareRecord, NvmeofError> {
    // The walk is strict: a present-but-unreadable tree stays loud; an
    // absent tree walks empty.
    let live = nvmet_stack.live_shares()?;
    let (candidate, notes) = adopt_candidate(subnqn, &live, ledger)?;
    for note in &notes {
        println!("note: {note}");
    }

    // §6.4 law 6: the pending intent (with `adopted_from` provenance) is
    // recorded before anything else; `begin_share` re-checks the
    // NQN/backing duplicate laws atomically under the ledger flock.
    ledger.begin_share(&candidate).map_err(NvmeofError::Io)?;

    // §6.10 pt 3 TOCTOU re-verify: re-probe fresh and compare every
    // live-observable field. Any failure here aborts loud and
    // garbage-collects the pending intent adopt itself just wrote —
    // target state is untouched on every path.
    let verified = nvmet_stack.live_shares().and_then(|live_now| {
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
/// data serving that must not bounce). `--target-stack nvmet` names the
/// one stack adopt probes; the retired `spdk` refuses (the env knob is
/// deliberately not consulted — detection is live-state truth).
pub fn adopt(subnqn: &str, flag: Option<StackKind>) -> Result<ShareRecord, NvmeofError> {
    if let Some(kind) = flag {
        resolve_stack(Some(kind))?;
    }
    check_root()?;
    retire_old_registry_once();
    let ledger = Ledger::open_default();
    let nvmet_stack = nvmet::NvmetStack::open_default().map_err(NvmeofError::Io)?;
    adopt_over(subnqn, &ledger, &nvmet_stack)
}

/// The `list` classification of a record the RETIRED SPDK stack wrote:
/// listed with the re-share sequence, never managed, never a restore
/// candidate (R-SYM-8).
pub const SPDK_RETIRED_CLASSIFICATION: &str = concat!(
    "spdk — RETIRED target stack (R-SYM-8): never re-presented (not started, restored or ",
    "adopted); ",
    spdk_reshare_sequence!()
);

/// Reconciliation classification of one ledger record for `list` (§6.2).
/// A record the retired SPDK stack wrote classifies as retired in every
/// state — the ledger no longer has a stack to reconcile it against.
pub fn classification_of(record: &ShareRecord, live: bool) -> &'static str {
    if record.stack == StackKind::Spdk {
        return SPDK_RETIRED_CLASSIFICATION;
    }
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
/// managed / down / pending / removing / foreign / retired-spdk — plus
/// the kept connected-fabric-disks section. The nvmet gather tolerates an
/// absent configfs tree.
pub fn list(json: bool) -> Result<(), NvmeofError> {
    check_root()?;
    let ledger = Ledger::open_default();
    let records = ledger.load().map_err(NvmeofError::Io)?;

    let nvmet_stack = nvmet::NvmetStack::open_default()?;
    nvmet_stack.preflight(PreflightOp::List)?;
    let nvmet_live = nvmet_stack.live_shares()?;

    let live: Vec<(StackKind, &LiveShare)> =
        nvmet_live.iter().map(|l| (StackKind::Nvmet, l)).collect();
    // A record is "live" when its OWN stack serves its NQN — a retired
    // SPDK record therefore never is.
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
