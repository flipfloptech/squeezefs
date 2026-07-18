//! Initiator-side fabric controller-state sampling (PR 6 / N6 of
//! `docs/design-nvmeof-target-management.md`, §6.9 Observability).
//!
//! The daemon `.stats` `fabric_*` family and the `squeezefs status`
//! per-volume `"Fabric"` section both read from HERE — one sysfs source
//! (`/sys/class/nvme`, the same root the kept initiator half walks).
//! Everything in this module is one-shot control-plane sysfs reading on
//! the 10 s stats cadence (the sanctioned `nvmeof` module precedent —
//! no data path is touched, so `std::fs` on tiny virtual files is the
//! right tool, not io_uring).
//!
//! §6.8 zero-mock policy: unit tests ride the injection seam (an
//! explicit `sysfs_nvme_root` argument pointing at a fixture tree),
//! never env behavior forks.

use std::collections::HashMap;
use std::fs;
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
        FabricIdentity {
            transport: self.transport.clone(),
            subsysnqn: self.subsysnqn.clone(),
            address: self.address.clone(),
        }
    }

    /// `state == "live"`; every other state (connecting / resetting /
    /// deleting / unknown / …) is not-live.
    pub fn is_live(&self) -> bool {
        self.state == "live"
    }

    /// Does this controller back the block device `dev_base` (a
    /// partition-stripped basename, see [`normalize_nvme_base`])?
    /// Matches, in order: a plain namespace child (`nvme3n1`), a native
    /// multipath path node (`nvme3c<C>n1` backing head node `nvme3n1`),
    /// or the controller-number prefix (`nvme3` backs `nvme3n*` — the
    /// head-node shape `/dev/nvmeXn1` the initiator mints).
    pub fn backs_device(&self, dev_base: &str) -> bool {
        // Plain namespace child: controller dir carries `nvme3n1`.
        if self.namespaces.iter().any(|ns| ns == dev_base) {
            return true;
        }
        // Head node vs multipath path node / controller prefix: parse
        // `nvme<X>n<Y>` out of the device basename.
        let Some((dev_ctrl_num, dev_ns_num)) = split_nvme_namespace(dev_base) else {
            return false;
        };
        // Native nvme multipath: the head node `nvme<X>n<Y>` the user
        // mounts is backed by path nodes `nvme<X>c<C>n<Y>` living under
        // the per-path controller directories.
        let multipath_child = self.namespaces.iter().any(|ns| {
            split_nvme_multipath_path_node(ns)
                .is_some_and(|(head, _c, nsid)| head == dev_ctrl_num && nsid == dev_ns_num)
        });
        if multipath_child {
            return true;
        }
        // Fallback: controller-number prefix (`nvme3` backs `nvme3n*`).
        self.name == format!("nvme{dev_ctrl_num}")
    }
}

/// Parse `nvme<X>n<Y>` → `(X, Y)`; anything else → `None`.
fn split_nvme_namespace(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix("nvme")?;
    let n_pos = rest.find('n')?;
    let ctrl: u64 = rest[..n_pos].parse().ok()?;
    let ns: u64 = rest[n_pos + 1..].parse().ok()?;
    Some((ctrl, ns))
}

/// Parse a native-multipath path node `nvme<X>c<C>n<Y>` → `(X, C, Y)`;
/// anything else → `None`.
fn split_nvme_multipath_path_node(name: &str) -> Option<(u64, u64, u64)> {
    let rest = name.strip_prefix("nvme")?;
    let c_pos = rest.find('c')?;
    let head: u64 = rest[..c_pos].parse().ok()?;
    let tail = &rest[c_pos + 1..];
    let n_pos = tail.find('n')?;
    let ctrl: u64 = tail[..n_pos].parse().ok()?;
    let ns: u64 = tail[n_pos + 1..].parse().ok()?;
    Some((head, ctrl, ns))
}

/// Strip an NVMe partition suffix from a device basename
/// (`nvme1n1p2` → `nvme1n1`); non-NVMe-namespace names pass through
/// unchanged.
pub fn normalize_nvme_base(name: &str) -> String {
    if let Some(p_pos) = name.rfind('p') {
        let (head, tail) = name.split_at(p_pos);
        if !tail[1..].is_empty()
            && tail[1..].bytes().all(|b| b.is_ascii_digit())
            && split_nvme_namespace(head).is_some()
        {
            return head.to_string();
        }
    }
    name.to_string()
}

/// Read one sysfs attribute file, trimmed; `None` when absent or
/// unreadable (tolerated — sysfs entries can vanish mid-walk while a
/// controller tears down).
fn read_attr(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Enumerate fabric controllers (`transport != "pcie"`) under
/// `sysfs_nvme_root`, sorted by controller name.
///
/// Missing-sysfs tolerance (design §6.9): a box with no fabric
/// controllers — no `/sys/class/nvme` at all, no fabric entries, or
/// unreadable attribute files — yields an EMPTY (or partial) result and
/// NEVER an error; the stats family emits zeros there.
pub fn enumerate_fabric_controllers(sysfs_nvme_root: &Path) -> Vec<FabricController> {
    let Ok(entries) = fs::read_dir(sysfs_nvme_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("nvme") {
            continue;
        }
        let dir = entry.path();
        // No transport attribute → unclassifiable → not fabric. `pcie`
        // → the user's local disks, never counted (the hard zero-case:
        // a box with only PCIe NVMe emits a zero family).
        let Some(transport) = read_attr(&dir, "transport") else {
            continue;
        };
        if transport == "pcie" {
            continue;
        }
        let state = read_attr(&dir, "state").unwrap_or_else(|| "unknown".to_string());
        let subsysnqn = read_attr(&dir, "subsysnqn").unwrap_or_default();
        let address = read_attr(&dir, "address").unwrap_or_default();
        let mut namespaces: Vec<String> = fs::read_dir(&dir)
            .map(|children| {
                children
                    .flatten()
                    .filter(|c| c.path().is_dir())
                    .map(|c| c.file_name().to_string_lossy().to_string())
                    .filter(|n| {
                        split_nvme_namespace(n).is_some()
                            || split_nvme_multipath_path_node(n).is_some()
                    })
                    .collect()
            })
            .unwrap_or_default();
        namespaces.sort();
        out.push(FabricController {
            name,
            transport,
            state,
            subsysnqn,
            address,
            namespaces,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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
pub struct FabricStatsSampler {
    /// identity → was the LAST observed state `live`? Identities absent
    /// from a sample are retained only while not-live (a give-up +
    /// renumbered reappearance as `live` must still count as one
    /// observed reconnect); identities that vanish while live are
    /// dropped (no observable transition can ever start from them, and
    /// dropping bounds the map at "endpoints currently attached +
    /// endpoints last seen mid-reconnect").
    last_live: HashMap<FabricIdentity, bool>,
}

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
    pub fn observe(&mut self, controllers: &[FabricController]) -> FabricSample {
        let mut next: HashMap<FabricIdentity, bool> = HashMap::with_capacity(controllers.len());
        let mut not_live = 0u64;
        let mut reconnects = 0u64;
        for ctrl in controllers {
            let live = ctrl.is_live();
            if !live {
                not_live += 1;
            }
            let identity = ctrl.identity();
            // Count at most one reconnect per identity per sample (two
            // controllers with one identity is a defensive case sysfs
            // should not produce).
            if live
                && !next.contains_key(&identity)
                && matches!(self.last_live.get(&identity), Some(false))
            {
                reconnects += 1;
            }
            // Live-wins on duplicate identities so a single reconnect
            // episode is never double-counted on later beats.
            next.entry(identity)
                .and_modify(|e| *e |= live)
                .or_insert(live);
        }
        // Retain vanished identities only while not-live (see field doc).
        for (identity, live) in self.last_live.drain() {
            if !live {
                next.entry(identity).or_insert(false);
            }
        }
        self.last_live = next;
        FabricSample {
            controllers: controllers.len() as u64,
            not_live,
            reconnects_observed: reconnects,
        }
    }
}

/// Resolve raw device paths (as recorded in the format config / passed
/// to `status`) to partition-stripped basenames for controller
/// matching: symlinks are canonicalized best-effort (an unresolvable
/// path falls back to its raw basename — matching then simply fails,
/// never errors).
pub fn device_base_names(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| {
            let path = Path::new(p);
            let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let base = resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            normalize_nvme_base(&base)
        })
        .collect()
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
    device_bases: &[String],
    controllers: &[FabricController],
) -> Option<serde_json::Value> {
    let mut matched: Vec<(String, &FabricController)> = Vec::new();
    for dev in device_bases {
        for ctrl in controllers {
            if ctrl.backs_device(dev) {
                matched.push((dev.clone(), ctrl));
            }
        }
    }
    if matched.is_empty() {
        return None;
    }
    // Field counts are over unique matched controller identities (a
    // multipath device matched by several path controllers counts each
    // controller once; several devices on one controller count it once).
    let mut seen: Vec<FabricIdentity> = Vec::new();
    let mut not_live = 0u64;
    for (_, ctrl) in &matched {
        let identity = ctrl.identity();
        if !seen.contains(&identity) {
            if !ctrl.is_live() {
                not_live += 1;
            }
            seen.push(identity);
        }
    }
    let rows: Vec<serde_json::Value> = matched
        .iter()
        .map(|(dev, ctrl)| {
            serde_json::json!({
                "Device": dev,
                "Controller": ctrl.name,
                "Transport": ctrl.transport,
                "State": ctrl.state,
                "SubsysNqn": ctrl.subsysnqn,
                "Address": ctrl.address,
            })
        })
        .collect();
    Some(serde_json::json!({
        "fabric_controllers": seen.len() as u64,
        "fabric_ctrl_not_live": not_live,
        "fabric_ctrl_reconnects": 0u64,
        "Controllers": rows,
    }))
}
