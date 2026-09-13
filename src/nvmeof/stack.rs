//! Share-record vocabulary for the NVMe-oF target-management module.
//!
//! These types are the **schema v1** of the share ledger
//! (`docs/design-nvmeof-target-management.md` §6.4): the versioned,
//! forward-only record store that owns stack dispatch, ownership
//! metadata, nvmet bookkeeping (loop devices, port ids), and the
//! write-ahead intent states. Field-presence rules (§6.4):
//!
//! * **Required on every record**: `subnqn`, `stack`, `state`
//!   (`pending` | `active` | `removing`), `backing_path`,
//!   `backing_canonical`, `listeners` (≥ 1 entry; `ip` + `port` required
//!   per entry), `created_utc`.
//! * **Optional (`Option` here)**: `ns_uuid` (the namespace identity,
//!   populated from N2 on — null on N1-era records); `nsid` /
//!   `ptpl_file` / `bdev_name` (populated ONLY by the retired SPDK stack —
//!   kept so its ledger records stay decodable, never written by the
//!   nvmet stack); `loop_device` + per-listener `nvmet_port_id`
//!   (nvmet-only); `adopted_from` (written only by `nvmeof adopt`,
//!   PR 4b — absent on shares created by `share`).
//!
//! Schema evolution is forward-only: any field addition is a
//! schema-visible change, and unknown fields within format 1 are an
//! error, never ignored (§6.4 law 3 — `deny_unknown_fields`). The N1
//! transitional contract (PR-plan PR 1) pins what N1-era writers
//! populate: required fields plus `loop_device`; everything else stays
//! null because the pre-rebuild share paths stamp/pin nothing.
//!
//! From N2 on this file also carries the finalized **`TargetStack`
//! trait** (§6.1) and its request/report vocabulary: small, synchronous
//! one-shot control-plane verbs (the `ReservationClient` precedent) with
//! injectable seams instead of env-var mocks. `NvmetStack`
//! (`super::nvmet`) is its ONE implementation since SPDK was retired as a
//! target (R-SYM-8, `docs/design-symmetric-metadata.md` §5.8.1).

use serde::{Deserialize, Serialize};
use std::io;

use super::StackKind;

/// Write-ahead intent state of a ledger record (§6.4 law 6).
///
/// `share` appends `Pending` **before the first stack mutation** and
/// flips to `Active` only after the last mutation succeeds; `unshare`
/// flips to `Removing` **before the first teardown write** and deletes
/// the record only after teardown completes. A crash in any window
/// leaves an intent record that still claims the objects — the product
/// can never strand its own share as "foreign".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareState {
    Pending,
    Active,
    Removing,
}

impl ShareState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShareState::Pending => "pending",
            ShareState::Active => "active",
            ShareState::Removing => "removing",
        }
    }
}

/// One (ip, port) listener of a share. On nvmet every listener is its
/// own configfs port object with its own recorded id
/// (`nvmet_port_id`, §6.6) — **null on N1-era records** (the old
/// allocator's small-int ids are deliberately untracked; N1 `unshare`
/// keeps the old all-ports symlink walk) and null on the retired SPDK
/// stack's records, where listeners needed no id bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub nvmet_port_id: Option<u32>,
}

/// Classification of a foreign share absorbed by `nvmeof adopt`
/// (§6.10; written from PR 4b on — parsed at N1 so the ledger format
/// stays readable by any ≥ N1 binary, the §Migration rollback law).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdoptClass {
    PreRebuild,
    Foreign,
    LedgerLoss,
}

impl AdoptClass {
    /// The on-disk kebab spelling (`list` display + verb output).
    pub fn as_str(&self) -> &'static str {
        match self {
            AdoptClass::PreRebuild => "pre-rebuild",
            AdoptClass::Foreign => "foreign",
            AdoptClass::LedgerLoss => "ledger-loss",
        }
    }
}

/// Adoption provenance (§6.4 field-presence rules / §6.10): present only
/// on records written by `nvmeof adopt`, surfaced by `list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptedFrom {
    pub utc: String,
    pub class: AdoptClass,
}

/// One share ledger record — schema v1 (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRecord {
    pub subnqn: String,
    pub stack: StackKind,
    pub state: ShareState,
    pub backing_path: String,
    pub backing_canonical: String,
    #[serde(default)]
    pub nsid: Option<u32>,
    #[serde(default)]
    pub ns_uuid: Option<String>,
    pub listeners: Vec<Listener>,
    #[serde(default)]
    pub bdev_name: Option<String>,
    #[serde(default)]
    pub ptpl_file: Option<String>,
    #[serde(default)]
    pub loop_device: Option<String>,
    pub created_utc: String,
    /// `--allow-host` allowlist (§Security), recorded so `restore`
    /// re-presents it — a restored share must never silently widen to
    /// allow-any. Absent (not `[]`) on allow-any shares, so records
    /// without an allowlist stay byte-compatible with N1 readers
    /// (schema-visible v1 addition at N2, the `adopted_from` pattern).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_from: Option<AdoptedFrom>,
}

impl ShareRecord {
    /// Value-level half of the §6.4 field-presence rules (the type/serde
    /// layer enforces presence; this enforces non-emptiness and the
    /// `listeners ≥ 1` law). Checked on every load and on `begin_share`.
    pub fn validate(&self) -> io::Result<()> {
        let fail = |what: &str| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "share ledger record violates the schema-v1 field-presence rules: {what} \
                     (subnqn '{}')",
                    self.subnqn
                ),
            ))
        };
        if self.subnqn.is_empty() {
            return fail("empty subnqn");
        }
        if self.backing_path.is_empty() {
            return fail("empty backing_path");
        }
        if self.backing_canonical.is_empty() {
            return fail("empty backing_canonical");
        }
        if self.created_utc.is_empty() {
            return fail("empty created_utc");
        }
        if self.listeners.is_empty() {
            return fail("listeners must carry >= 1 entry");
        }
        if self.listeners.iter().any(|l| l.ip.is_empty()) {
            return fail("listener with empty ip");
        }
        if self.allow_hosts.iter().any(|h| h.is_empty()) {
            return fail("allow_hosts entry with empty host NQN");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The TargetStack trait + verb vocabulary (§6.1, finalized at N2)
// ---------------------------------------------------------------------------

/// The verb a preflight ladder is gating (§6.2: every verb runs the
/// selected stack's preflight first; failures are structured, ordered,
/// and name the remediation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightOp {
    Share,
    Unshare,
    List,
    Restore,
}

/// A loud, actionable preflight failure. `Display` renders the full
/// designed message — the error text IS the runbook (§6.2/§6.3), and it
/// never suggests falling back to the other stack as an automatic action.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PreflightError {
    pub message: String,
}

/// Module-local CLI-facing error (design §API: thiserror, never
/// FUSE-errno-mapped — nothing on the FUSE path constructs it).
#[derive(Debug, thiserror::Error)]
pub enum NvmeofError {
    #[error("{0}")]
    Preflight(#[from] PreflightError),
    /// A loud refusal whose message is the runbook: it names the holder /
    /// classification / remediation steps (§6.4 duplicate guard, §6.6
    /// port-range exhaustion, ownership refusals, per-stack flag
    /// semantics).
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What a stack needs to establish one share. Built by the verb layer
/// (`super::share`) after grammar validation, backing preparation, and
/// identity resolution — the stack performs target mutations and rides
/// the ledger intent protocol.
#[derive(Debug, Clone)]
pub struct ShareRequest {
    pub subnqn: String,
    pub backing_path: String,
    pub backing_canonical: String,
    /// The namespace identity (§6.4): seeded by `--ns-uuid` or generated
    /// once at share time — recorded, stamped as nvmet `device_uuid`,
    /// re-presented by restore.
    pub ns_uuid: String,
    /// One (ip, port) per listener; `nvmet_port_id` is allocated by the
    /// nvmet stack and recorded (`None` on entry).
    pub listeners: Vec<Listener>,
    /// `--allow-host` allowlist; empty = allow-any (the trusted-fabric
    /// default, stated loudly in docs).
    pub allow_hosts: Vec<String>,
}

/// One live share as the stack reports it (the configfs walk) —
/// reconciled against the ledger by `list` and classified by `nvmeof
/// adopt` (§6.10: the walker IS the adopt classification probe; adopt
/// reads the live object into a candidate record from exactly this
/// shape).
#[derive(Debug, Clone)]
pub struct LiveShare {
    pub subnqn: String,
    /// The device the target serves (nvmet `device_path`; may be a loop
    /// node for file backings).
    pub device_path: String,
    /// `device_path` resolved toward the operator's backing: loop nodes
    /// resolve to their backing file when the kernel exposes it.
    pub backing_canonical: String,
    pub ns_uuid: Option<String>,
    pub listeners: Vec<Listener>,
    pub enabled: bool,
    /// Every namespace index the live object carries (§6.10 shape
    /// classification: nvmet supports exactly `[1]` — the structural
    /// convention; anything else is `adopt_shape_unsupported`).
    pub nsids: Vec<u32>,
    /// The live host-NQN allowlist (nvmet `allowed_hosts` links); empty =
    /// allow-any. Recorded by adopt so a restored adopted share never
    /// silently widens to allow-any.
    pub allow_hosts: Vec<String>,
}

/// Per-record outcome of a `restore` replay (§6.4 laws 4 + 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Ledger record's live objects were gone — re-established,
    /// re-presenting the recorded identity.
    Restored,
    /// Already live and matching (`device_path` **and** `device_uuid`) —
    /// verified no-op.
    VerifiedNoop,
    /// Crash-window `pending` intent whose live objects exist — finalized
    /// `active`.
    FinalizedPending,
    /// Crash-window `pending` intent with no live objects — the
    /// interrupted share never completed; record garbage-collected loud.
    GarbageCollectedPending,
    /// `removing` intent — the interrupted teardown was resumed and the
    /// record deleted.
    TeardownResumed,
    /// Not replayed, with the loud reason — the retired SPDK stack's
    /// records: listed with the re-share sequence, never re-presented
    /// (R-SYM-8).
    Skipped(String),
    /// Loud per-record failure (collected, never short-circuiting the
    /// report) — e.g. existing-but-mismatched live state, never clobbered.
    Failed(String),
}

/// One `restore` report line.
#[derive(Debug, Clone)]
pub struct RestoreEntry {
    pub subnqn: String,
    pub outcome: RestoreOutcome,
}

/// The `restore` verb's per-share result report (a report, not a
/// first-failure bail — §6.1).
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub entries: Vec<RestoreEntry>,
}

impl RestoreReport {
    pub fn failures(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.outcome, RestoreOutcome::Failed(_)))
            .count()
    }
}

/// Stack health/inventory snapshot (§6.9): module presence, configfs,
/// object counts, per-ns PR enablement.
#[derive(Debug, Clone)]
pub struct TargetStatus {
    pub stack: StackKind,
    pub modules_present: bool,
    pub configfs_mounted: bool,
    pub subsystems: usize,
    pub namespaces: usize,
    pub ports: usize,
    pub resv_enabled_namespaces: usize,
}

/// One NVMe-oF target stack the product can manage (§6.1). The ONE
/// implementation is `NvmetStack` (kernel configfs) — SPDK was retired
/// as a target (R-SYM-8). Methods are synchronous one-shot control-plane
/// operations (CLI-driven).
pub trait TargetStack: Send + Sync {
    fn kind(&self) -> StackKind;

    /// Loud, actionable, ordered checks. Every error names its remediation
    /// verb.
    fn preflight(&self, op: PreflightOp) -> Result<(), PreflightError>;

    fn share(&self, req: &ShareRequest) -> Result<ShareRecord, NvmeofError>;
    fn unshare(&self, rec: &ShareRecord) -> Result<(), NvmeofError>;

    /// Live state as the stack reports it (the configfs walk) —
    /// reconciled against the ledger by `list` (managed / down / foreign).
    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError>;

    /// Re-establish every ledger share (idempotent; per-share errors are
    /// collected, not short-circuited — a report, not a first-failure
    /// bail).
    fn restore(&self, recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError>;

    fn target_status(&self) -> Result<TargetStatus, NvmeofError>;
}
