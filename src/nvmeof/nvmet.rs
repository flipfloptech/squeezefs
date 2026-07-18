//! `NvmetStack` — the rebuilt kernel-nvmet configfs target path
//! (`docs/design-nvmeof-target-management.md` §6.6, PR 2/N2).
//!
//! Rebuild laws (each pinned by tests):
//!
//! * **No fake files in configfs** — loop association and port ids live
//!   in the ledger (§6.4 law 5); nothing is ever written into configfs
//!   except real kernel attributes. Every configfs write's error is
//!   checked (the pre-rebuild `let _ =` swallows are dead).
//! * **Port-id allocation is collision-proof**: ports carry no name, so
//!   ownership rides the reserved id range `[base, base+999]`
//!   (`SQUEEZEFS_NVMET_PORT_ID_BASE`, default 53000 — disjoint from
//!   dev_substrate's 52026 and the scoping rig's 52470/52471).
//!   Deterministic first candidate `base + (fnv1a64("tcp:{ip}:{port}") %
//!   900)`, linear probe wrapping the full range; a candidate is
//!   reusable only if its attrs match exactly AND every subsystem link
//!   under it is ours (**ownership = ledger membership**); otherwise
//!   foreign — skip, never touch. Exhaustion refuses loud, naming the
//!   knob.
//! * **`resv_enable` default ON** (written before enable when the knob
//!   exists; absent ⇒ loud detection-grade note) and a stamped
//!   **`device_uuid`** — the recorded `ns_uuid`, generated once or
//!   seeded by `--ns-uuid`, re-presented by restore, never regenerated.
//! * **The namespace index is structurally fixed at 1** — one namespace
//!   per subsystem (the module/dev_substrate convention; what makes
//!   G2's same-nsid clause hold on this leg).
//! * **Restore** replays ledger records idempotently:
//!   existing-and-matching (`device_path` **and** `device_uuid`) is a
//!   verified no-op; existing-but-mismatched is a loud conflict, never
//!   clobbered; §6.4 law-6 intents reconcile (`pending` finalized or
//!   GC'd, `removing` resumed).
//!
//! io_uring note: one-shot admin control plane (configfs writes,
//! losetup/modprobe shell-outs) — the sanctioned `reservation.rs`
//! precedent; no data path is touched.

use std::path::PathBuf;

use super::ledger::Ledger;
use super::stack::{
    LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreReport, ShareRecord, ShareRequest,
    TargetStack, TargetStatus,
};
use super::StackKind;

/// Env knob relocating the reserved port-id range (§6.6). A relocation
/// seam in the §6.8 sense — the allocator's behavior is identical at any
/// base.
pub const NVMET_PORT_ID_BASE_ENV: &str = "SQUEEZEFS_NVMET_PORT_ID_BASE";
/// Default reserved range base: ids `[53000, 53999]`.
pub const NVMET_PORT_ID_BASE_DEFAULT: u32 = 53000;
/// Range width: 1000 ids.
pub const NVMET_PORT_ID_RANGE: u32 = 1000;
/// fnv1a first candidates spread over the first 900 slots; the last 100
/// are pure probe headroom.
pub const NVMET_PORT_HASH_SLOTS: u32 = 900;

/// Production configfs root.
pub const NVMET_CONFIGFS_ROOT: &str = "/sys/kernel/config/nvmet";

/// FNV-1a 64-bit (the §6.6 "fnv1a-style deterministic probe" hash).
/// Frozen by test vectors — allocated ids are ledger-recorded, so the
/// probe start must never drift across refactors.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let _ = bytes;
    unimplemented!("PR 2 (N2) RED: fnv1a_64")
}

/// One existing configfs port, snapshotted for the pure allocator.
#[derive(Debug, Clone)]
pub struct PortSnapshotEntry {
    pub id: u32,
    pub trtype: String,
    pub traddr: String,
    pub trsvcid: String,
    pub adrfam: String,
    pub subsystem_links: Vec<String>,
}

/// An allocator decision: a fresh id to create, or an existing
/// ours-and-matching port object to reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortAllocation {
    pub id: u32,
    pub reuse: bool,
}

/// The pure §6.6 port-id allocator over an injected configfs snapshot.
/// `is_ours` answers ledger membership for a subsystem NQN (ownership =
/// ledger membership; NQN prefixes are only a display heuristic for
/// unledgered objects).
pub fn allocate_port_id(
    base: u32,
    ip: &str,
    port: u16,
    existing: &[PortSnapshotEntry],
    is_ours: &dyn Fn(&str) -> bool,
) -> Result<PortAllocation, NvmeofError> {
    let _ = (base, ip, port, existing, is_ours);
    unimplemented!("PR 2 (N2) RED: allocate_port_id")
}

/// The kernel-nvmet target stack (§6.1/§6.6): configfs plumbing, port
/// allocator, loop handling — riding the shared ledger intent protocol.
pub struct NvmetStack {
    configfs_root: PathBuf,
    ledger: Ledger,
    port_id_base: u32,
}

impl NvmetStack {
    /// Stack rooted at an explicit configfs tree + ledger (the sanctioned
    /// injection seam for the unit tier: every code path executed is the
    /// production path pointed at a different location).
    pub fn new(configfs_root: impl Into<PathBuf>, ledger: Ledger, port_id_base: u32) -> Self {
        NvmetStack {
            configfs_root: configfs_root.into(),
            ledger,
            port_id_base,
        }
    }

    /// Production constructor: `/sys/kernel/config/nvmet`, the default
    /// ledger, and the (env-relocatable) reserved port-id base.
    pub fn open_default() -> Self {
        let _ = (NVMET_CONFIGFS_ROOT, NVMET_PORT_ID_BASE_ENV);
        unimplemented!("PR 2 (N2) RED: NvmetStack::open_default")
    }
}

impl TargetStack for NvmetStack {
    fn kind(&self) -> StackKind {
        StackKind::Nvmet
    }

    fn preflight(&self, op: PreflightOp) -> Result<(), PreflightError> {
        let _ = (op, &self.configfs_root, &self.ledger, self.port_id_base);
        unimplemented!("PR 2 (N2) RED: NvmetStack::preflight")
    }

    fn share(&self, req: &ShareRequest) -> Result<ShareRecord, NvmeofError> {
        let _ = req;
        unimplemented!("PR 2 (N2) RED: NvmetStack::share")
    }

    fn unshare(&self, rec: &ShareRecord) -> Result<(), NvmeofError> {
        let _ = rec;
        unimplemented!("PR 2 (N2) RED: NvmetStack::unshare")
    }

    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        unimplemented!("PR 2 (N2) RED: NvmetStack::live_shares")
    }

    fn restore(&self, recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError> {
        let _ = recs;
        unimplemented!("PR 2 (N2) RED: NvmetStack::restore")
    }

    fn target_status(&self) -> Result<TargetStatus, NvmeofError> {
        unimplemented!("PR 2 (N2) RED: NvmetStack::target_status")
    }
}
