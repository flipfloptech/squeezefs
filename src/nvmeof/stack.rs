//! Share-record vocabulary for the NVMe-oF target-management module.
//!
//! These types are the **schema v1** of the share ledger
//! (`docs/design-nvmeof-target-management.md` §6.4): the versioned,
//! forward-only record store that owns cross-stack dispatch, ownership
//! metadata, nvmet bookkeeping (loop devices, port ids), and the
//! write-ahead intent states. Field-presence rules (§6.4):
//!
//! * **Required on every record**: `subnqn`, `stack`, `state`
//!   (`pending` | `active` | `removing`), `backing_path`,
//!   `backing_canonical`, `listeners` (≥ 1 entry; `ip` + `port` required
//!   per entry), `created_utc`.
//! * **Optional (`Option` here)**: `ns_uuid` (the both-stack namespace
//!   identity, populated from N2/nvmet / N4/spdk on — null on N1-era
//!   records); `nsid` / `ptpl_file` / `bdev_name` (SPDK-only, null on
//!   nvmet); `loop_device` + per-listener `nvmet_port_id` (nvmet-only,
//!   null on SPDK); `adopted_from` (written only by `nvmeof adopt`,
//!   PR 4b — absent on shares created by `share`).
//!
//! Schema evolution is forward-only: any field addition is a
//! schema-visible change, and unknown fields within format 1 are an
//! error, never ignored (§6.4 law 3 — `deny_unknown_fields`). The N1
//! transitional contract (PR-plan PR 1) pins what N1-era writers
//! populate: required fields plus `loop_device`; everything else stays
//! null because the pre-rebuild share paths stamp/pin nothing.
//!
//! (`TargetStack`/`ShareRequest`/`TargetStatus` — the trait surface this
//! file is named for — land with the stack rebuilds at N2+; defining
//! them now would be dead code.)

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
/// keeps the old all-ports symlink walk) and null on SPDK, where
/// listeners need no id bookkeeping.
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
        Ok(())
    }
}
