//! Initiator-side fabric controller-state sampling (PR 6 / N6 of
//! `docs/design-nvmeof-target-management.md`, §6.9 Observability).
//!
//! The daemon `.stats` `fabric_*` family and the `squeezefs status`
//! per-volume `"Fabric"` section both read from HERE — one sysfs source
//! (`/sys/class/nvme`, the same root the kept initiator half walks).
//! Everything in this module is one-shot control-plane sysfs reading on
//! a 10 s cadence (the sanctioned `nvmeof` module precedent — no data
//! path is touched, so `std::fs` on tiny virtual files is the right
//! tool, not io_uring).
//!
//! §6.8 zero-mock policy: unit tests ride the injection seam (an
//! explicit `sysfs_nvme_root` argument pointing at a fixture tree),
//! never env behavior forks.

use std::path::Path;

/// The initiator sysfs root every fabric enumeration walks — controllers
/// appear as `nvme<N>` children carrying `transport` / `state` /
/// `subsysnqn` / `address` attribute files. Shared with the kept
/// initiator half (`initiator.rs` connect/disconnect/list walk the same
/// root).
pub const SYSFS_NVME: &str = "/sys/class/nvme";

/// Identity of a fabric controller that is STABLE across kernel
/// controller renumbering: `nvme3` can give up (`ctrl_loss_tmo`), get
/// deleted, and reappear as `nvme7` on reconnect — but it still serves
/// the same `(transport, subsysnqn, address)` endpoint. The sampled
/// reconnect counter tracks THIS, never the `nvmeN` name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FabricIdentity {
    pub transport: String,
    pub subsysnqn: String,
    pub address: String,
}

/// One fabric-attached (`transport != "pcie"`) controller as read from
/// one sysfs sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricController {
    /// Kernel controller name (`nvme3`) — display + device matching
    /// only; renumbering-unstable, never identity.
    pub name: String,
    /// `transport` attribute (`tcp` / `rdma` / `fc` / `loop`).
    pub transport: String,
    /// Instantaneous `state` attribute (`live` / `connecting` /
    /// `resetting` / `deleting` / …). A missing/unreadable state file
    /// reads as `"unknown"` (counted not-live — a controller we cannot
    /// prove live is not live).
    pub state: String,
    /// `subsysnqn` attribute — the target NQN.
    pub subsysnqn: String,
    /// `address` attribute (`traddr=…,trsvcid=…`).
    pub address: String,
    /// Namespace block-device children observed under the controller
    /// directory (`nvme3n1`, or multipath path nodes like `nvme3c3n1`)
    /// — the device→controller matching input for `squeezefs status`.
    pub namespaces: Vec<String>,
}

impl FabricController {
    /// The renumbering-stable identity tuple (see [`FabricIdentity`]).
    pub fn identity(&self) -> FabricIdentity {
        unimplemented!("PR 6 RED skeleton")
    }

    /// `state == "live"`; every other state (connecting / resetting /
    /// deleting / unknown / …) is not-live.
    pub fn is_live(&self) -> bool {
        unimplemented!("PR 6 RED skeleton")
    }

    /// Does this controller back the block device `dev_base` (a
    /// partition-stripped basename, see [`normalize_nvme_base`])?
    /// Matches, in order: a plain namespace child (`nvme3n1`), a native
    /// multipath path node (`nvme3c<C>n1` backing head node `nvme3n1`),
    /// or the controller-number prefix (`nvme3` backs `nvme3n*` — the
    /// head-node shape `/dev/nvmeXn1` the initiator mints).
    pub fn backs_device(&self, _dev_base: &str) -> bool {
        unimplemented!("PR 6 RED skeleton")
    }
}

/// Strip an NVMe partition suffix from a device basename
/// (`nvme1n1p2` → `nvme1n1`); non-NVMe-namespace names pass through
/// unchanged.
pub fn normalize_nvme_base(_name: &str) -> String {
    unimplemented!("PR 6 RED skeleton")
}

/// Enumerate fabric controllers (`transport != "pcie"`) under
/// `sysfs_nvme_root`, sorted by controller name.
///
/// Missing-sysfs tolerance (design §6.9): a box with no fabric
/// controllers — no `/sys/class/nvme` at all, no fabric entries, or
/// unreadable attribute files — yields an EMPTY (or partial) result and
/// NEVER an error; the stats family emits zeros there.
pub fn enumerate_fabric_controllers(_sysfs_nvme_root: &Path) -> Vec<FabricController> {
    unimplemented!("PR 6 RED skeleton")
}

/// What one sampler pass observed (the deltas the daemon folds into the
/// `fabric_*` atomics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricSample {
    /// Gauge: fabric controllers present this sample.
    pub controllers: u64,
    /// Gauge: controllers whose state was anything but `live` — the
    /// reconnect-storm detector (§6.9).
    pub not_live: u64,
    /// Counter delta: reconnects OBSERVED this sample (see
    /// [`FabricStatsSampler::observe`] for the sampled-transition
    /// undercount caveat).
    pub reconnects_observed: u64,
}

/// The sampled-transition state machine behind `fabric_ctrl_reconnects`
/// (design §6.9): per [`FabricIdentity`], remembers whether the last
/// observed state was live.
#[derive(Debug, Default)]
pub struct FabricStatsSampler {}

impl FabricStatsSampler {
    /// Fold one sysfs sample into the sampler and return the gauges +
    /// the reconnect delta.
    ///
    /// `fabric_ctrl_reconnects` is a **sampled-transition counter, not a
    /// kernel counter** (design §6.9): sysfs exposes only instantaneous
    /// controller state — there is NO native cumulative reconnect count
    /// to read, so this counts *observed* not-live→live transitions per
    /// controller identity at the stats cadence and **undercounts flaps
    /// faster than the cadence** (a controller that bounces
    /// live→connecting→live entirely between two samples counts zero).
    /// That is acceptable for the storm detector it exists to be
    /// (measured storms run at 10 s cadence for ~10 min) — do NOT try to
    /// "fix" it against a kernel counter that does not exist.
    pub fn observe(&mut self, _controllers: &[FabricController]) -> FabricSample {
        unimplemented!("PR 6 RED skeleton")
    }
}

/// Resolve raw device paths (as recorded in the format config / passed
/// to `status`) to partition-stripped basenames for controller
/// matching: symlinks are canonicalized best-effort (an unresolvable
/// path falls back to its raw basename — matching then simply fails,
/// never errors).
pub fn device_base_names(_paths: &[String]) -> Vec<String> {
    unimplemented!("PR 6 RED skeleton")
}

/// Build the `squeezefs status` per-volume `"Fabric"` section (design
/// §6.9): `Some(section)` when at least one of the volume's backing
/// devices is fabric-attached, `None` otherwise (the section is simply
/// absent for non-fabric volumes).
///
/// The three family fields are rendered volume-scoped (counted over the
/// controllers backing THIS volume's devices — the daemon `.stats`
/// twins are box-wide), plus one `Controllers` row per matched
/// controller carrying the design-named target NQN / traddr / state.
/// `fabric_ctrl_reconnects` is 0 by construction here: a one-shot CLI
/// sample has no history to observe a transition in — the live counter
/// is the consuming daemon's `.stats` field.
pub fn fabric_status_section(
    _device_bases: &[String],
    _controllers: &[FabricController],
) -> Option<serde_json::Value> {
    unimplemented!("PR 6 RED skeleton")
}
