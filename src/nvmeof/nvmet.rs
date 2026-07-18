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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::ledger::Ledger;
use super::stack::{
    Listener, LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreEntry, RestoreOutcome,
    RestoreReport, ShareRecord, ShareRequest, ShareState, TargetStack, TargetStatus,
};
use super::{execute_cmd, StackKind};

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
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

/// The IPv4/IPv6 address family attr value for a listener address.
pub fn adrfam_of(ip: &str) -> io::Result<&'static str> {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => Ok("ipv4"),
        Ok(std::net::IpAddr::V6(_)) => Ok("ipv6"),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("listener address '{ip}' is not a valid IP address"),
        )),
    }
}

fn listener_matches(entry: &PortSnapshotEntry, ip: &str, port: u16, adrfam: &str) -> bool {
    entry.trtype == "tcp"
        && entry.traddr == ip
        && entry.trsvcid == port.to_string()
        && entry.adrfam == adrfam
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
    let adrfam = adrfam_of(ip)?;
    let by_id: HashMap<u32, &PortSnapshotEntry> = existing.iter().map(|e| (e.id, e)).collect();
    let key = format!("tcp:{ip}:{port}");
    let first = base + (fnv1a_64(key.as_bytes()) % u64::from(NVMET_PORT_HASH_SLOTS)) as u32;

    // Linear probe from the deterministic first candidate, wrapping
    // through the FULL [base, base+999] range (the last 100 ids are
    // probe headroom the hash never lands on directly).
    for step in 0..NVMET_PORT_ID_RANGE {
        let id = base + ((first - base) + step) % NVMET_PORT_ID_RANGE;
        match by_id.get(&id) {
            None => return Ok(PortAllocation { id, reuse: false }),
            Some(entry) => {
                let all_links_ours = entry.subsystem_links.iter().all(|nqn| is_ours(nqn));
                if listener_matches(entry, ip, port, adrfam) && all_links_ours {
                    // Exact attrs + every link ledgered (or link-free):
                    // our own object (possibly a leftover) — reuse.
                    return Ok(PortAllocation { id, reuse: true });
                }
                // Foreign or incompatible: skip, never touch.
            }
        }
    }
    Err(NvmeofError::Refused(format!(
        "nvmet port-id range exhausted: all {NVMET_PORT_ID_RANGE} ids in [{base}, {}] are \
         occupied by foreign or incompatible configfs ports; relocate the reserved range with \
         {NVMET_PORT_ID_BASE_ENV}",
        base + NVMET_PORT_ID_RANGE - 1
    )))
}

/// Reads a configfs attribute, trimmed. Missing attrs read as `None`.
fn read_attr_opt(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn write_attr(path: &Path, value: &str) -> Result<(), NvmeofError> {
    fs::write(path, value).map_err(|e| {
        NvmeofError::Io(io::Error::new(
            e.kind(),
            format!(
                "configfs write {} <- '{}' failed: {e}",
                path.display(),
                value
            ),
        ))
    })
}

fn create_dir_checked(path: &Path) -> Result<(), NvmeofError> {
    fs::create_dir_all(path).map_err(|e| {
        NvmeofError::Io(io::Error::new(
            e.kind(),
            format!("configfs mkdir {} failed: {e}", path.display()),
        ))
    })
}

/// Removes one configfs object directory. On real configfs the plain
/// `rmdir` succeeds once child *objects* are gone (attribute files and
/// default groups are intrinsic and never block it). The `remove_dir_all`
/// fallback exists for the injected-root unit tier, where attrs are real
/// files that DO block `rmdir`; on a real kernel a fallback attempt on a
/// genuinely non-empty object fails loud on the first attr unlink (EPERM)
/// — it can never silently destroy kernel state.
fn remove_configfs_object(path: &Path) -> Result<(), NvmeofError> {
    if !path.exists() {
        return Ok(());
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == io::ErrorKind::DirectoryNotEmpty
                || e.raw_os_error() == Some(libc::ENOTEMPTY) =>
        {
            fs::remove_dir_all(path).map_err(|e| {
                NvmeofError::Io(io::Error::new(
                    e.kind(),
                    format!("configfs rmdir {} failed: {e}", path.display()),
                ))
            })
        }
        Err(e) => Err(NvmeofError::Io(io::Error::new(
            e.kind(),
            format!("configfs rmdir {} failed: {e}", path.display()),
        ))),
    }
}

/// Resolves a served device path toward the operator's backing:
/// canonicalize, and resolve loop nodes to their backing file when the
/// kernel exposes it (`/sys/block/loopN/loop/backing_file`).
pub fn resolve_backing_canonical(device_path: &str) -> String {
    let canon = fs::canonicalize(device_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| device_path.to_string());
    if let Some(name) = canon.strip_prefix("/dev/") {
        if name.starts_with("loop") && !name.contains('/') {
            if let Ok(bf) = fs::read_to_string(format!("/sys/block/{name}/loop/backing_file")) {
                let bf = bf.trim();
                if !bf.is_empty() {
                    return fs::canonicalize(bf)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| bf.to_string());
                }
            }
        }
    }
    canon
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
    pub fn open_default() -> io::Result<Self> {
        let base = match std::env::var(NVMET_PORT_ID_BASE_ENV) {
            Ok(v) if !v.is_empty() => v.parse::<u32>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{NVMET_PORT_ID_BASE_ENV}='{v}' is not a valid port-id base"),
                )
            })?,
            _ => NVMET_PORT_ID_BASE_DEFAULT,
        };
        Ok(NvmetStack::new(
            NVMET_CONFIGFS_ROOT,
            Ledger::open_default(),
            base,
        ))
    }

    fn subsystems_dir(&self) -> PathBuf {
        self.configfs_root.join("subsystems")
    }

    fn ports_dir(&self) -> PathBuf {
        self.configfs_root.join("ports")
    }

    fn hosts_dir(&self) -> PathBuf {
        self.configfs_root.join("hosts")
    }

    /// The §6.2 nvmet preflight ladder, rungs 1–2: modules loadable
    /// (loading them IS the check — idempotent), configfs mounted, tree
    /// present. When the tree already exists this is a pure presence
    /// check (which is also what makes the injected-root unit tier ride
    /// the production path).
    fn ensure_tree(&self) -> Result<(), PreflightError> {
        if self.subsystems_dir().is_dir() {
            return Ok(());
        }
        // Rung 1: nvmet + nvmet-tcp loadable.
        for module in ["nvmet", "nvmet-tcp"] {
            if let Err(e) = execute_cmd("modprobe", &[module]) {
                return Err(PreflightError {
                    message: format!(
                        "kernel nvmet target stack unavailable: modprobe {module} failed ({e})\n  \
                         install the kernel's nvmet modules (linux-modules-extra on Ubuntu), \
                         then retry"
                    ),
                });
            }
        }
        // Rung 2: configfs mounted (only ever attempted against the real
        // mount point — an injected root is never conjured).
        if !self.subsystems_dir().is_dir() && self.configfs_root == Path::new(NVMET_CONFIGFS_ROOT) {
            let _mounted = execute_cmd("mount", &["-t", "configfs", "none", "/sys/kernel/config"]);
            // The mount may legitimately report "already mounted"; the
            // authoritative check is the re-probe below, which fails loud.
        }
        if self.subsystems_dir().is_dir() {
            Ok(())
        } else {
            Err(PreflightError {
                message: format!(
                    "kernel nvmet target stack unavailable: configfs tree {} not present after \
                     modprobe\n  check: mount | grep configfs   (expect configfs on \
                     /sys/kernel/config)\n  mount: sudo mount -t configfs none /sys/kernel/config",
                    self.configfs_root.display()
                ),
            })
        }
    }

    /// Snapshot every existing configfs port in our reserved range plus
    /// any port whose links matter for the walk (all of them — the
    /// allocator only decides about in-range ids, but `live_shares`
    /// listener resolution wants every port).
    fn snapshot_ports(&self) -> Result<Vec<PortSnapshotEntry>, NvmeofError> {
        let mut out = Vec::new();
        let ports = self.ports_dir();
        if !ports.is_dir() {
            return Ok(out);
        }
        for entry in fs::read_dir(&ports).map_err(NvmeofError::Io)? {
            let entry = entry.map_err(NvmeofError::Io)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(id) = name.parse::<u32>() else {
                continue; // not a port id we could ever own
            };
            let p = entry.path();
            let mut links = Vec::new();
            let subs = p.join("subsystems");
            if subs.is_dir() {
                for l in fs::read_dir(&subs).map_err(NvmeofError::Io)? {
                    let l = l.map_err(NvmeofError::Io)?;
                    links.push(l.file_name().to_string_lossy().into_owned());
                }
            }
            out.push(PortSnapshotEntry {
                id,
                trtype: read_attr_opt(&p.join("addr_trtype")).unwrap_or_default(),
                traddr: read_attr_opt(&p.join("addr_traddr")).unwrap_or_default(),
                trsvcid: read_attr_opt(&p.join("addr_trsvcid")).unwrap_or_default(),
                adrfam: read_attr_opt(&p.join("addr_adrfam")).unwrap_or_default(),
                subsystem_links: links,
            });
        }
        Ok(out)
    }

    fn ledgered_nqns(&self) -> Result<HashSet<String>, NvmeofError> {
        Ok(self
            .ledger
            .load()
            .map_err(NvmeofError::Io)?
            .into_iter()
            .map(|r| r.subnqn)
            .collect())
    }

    /// The §6.4 duplicate-guard classification half: how the refusal
    /// message names a live holder.
    fn classify_holder(&self, nqn: &str) -> String {
        match self.ledger.find(nqn) {
            Ok(Some(rec)) => format!("managed — ledger state {}", rec.state.as_str()),
            Ok(None) => "foreign — not in the share ledger".to_string(),
            Err(e) => format!("unknown — share ledger unreadable: {e}"),
        }
    }

    /// The manual removal runbook for a live nvmet object (§6.4: the
    /// refusal message IS the runbook).
    fn manual_removal_steps(&self, nqn: &str) -> String {
        let root = self.configfs_root.display();
        format!(
            "    rm  {root}/ports/<id>/subsystems/{nqn}    (for each port linking it)\n    \
             echo 0 > {root}/subsystems/{nqn}/namespaces/1/enable\n    \
             rmdir {root}/subsystems/{nqn}/namespaces/1\n    \
             rmdir {root}/subsystems/{nqn}"
        )
    }

    /// §6.4 live-state duplicate guard (the configfs `device_path` walk):
    /// refuses when the requested backing is already served by ANY live
    /// subsystem, or when the requested NQN is itself live. The refusal
    /// names the holder, its classification, and the exit. "Live" means
    /// **materialized** — a device_path was written or the namespace is
    /// enabled; an empty configfs shell (mkdir'd, nothing ever written —
    /// a crash-window artifact) serves nothing and is absorbed by the
    /// share's own idempotent mkdir instead of refused.
    fn duplicate_guard(&self, req: &ShareRequest) -> Result<(), NvmeofError> {
        for live in self.walk_live()? {
            let materialized = !live.device_path.is_empty() || live.enabled;
            if !materialized {
                continue;
            }
            if live.subnqn == req.subnqn {
                let class = self.classify_holder(&live.subnqn);
                return Err(NvmeofError::Refused(format!(
                    "subsystem '{}' is already live on the kernel nvmet target \
                     (classification: {class}); SqueezeFS never adopts or clobbers a live \
                     object implicitly.\n  if it is managed, unshare it first:\n    sudo \
                     squeezefs nvmeof unshare {}\n  if it is foreign/pre-rebuild, remove it \
                     manually:\n{}",
                    live.subnqn,
                    live.subnqn,
                    self.manual_removal_steps(&live.subnqn)
                )));
            }
            let same_backing = live.backing_canonical == req.backing_canonical
                || live.device_path == req.backing_path
                || live.device_path == req.backing_canonical;
            if same_backing {
                let class = self.classify_holder(&live.subnqn);
                let exit = if class.starts_with("managed") {
                    format!(
                        "  unshare the holder first:\n    sudo squeezefs nvmeof unshare {}",
                        live.subnqn
                    )
                } else {
                    format!(
                        "  removal-first is the only re-share path while the old object \
                         serves (docs/design-nvmeof-target-management.md §6.4):\n{}",
                        self.manual_removal_steps(&live.subnqn)
                    )
                };
                return Err(NvmeofError::Refused(format!(
                    "backing path '{}' is already served by live nvmet subsystem '{}' \
                     (classification: {class}) — the same backing must never be double-served, \
                     across stacks included.\n{exit}",
                    req.backing_path, live.subnqn
                )));
            }
        }
        Ok(())
    }

    /// The configfs walk behind `live_shares` (tolerates an absent tree:
    /// nothing is live).
    fn walk_live(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        let subs_dir = self.subsystems_dir();
        if !subs_dir.is_dir() {
            return Ok(Vec::new());
        }
        // Port-link index: nqn -> listeners.
        let mut listeners_of: HashMap<String, Vec<Listener>> = HashMap::new();
        for port in self.snapshot_ports()? {
            for nqn in &port.subsystem_links {
                let svc_port = port.trsvcid.parse::<u16>().unwrap_or(0);
                listeners_of.entry(nqn.clone()).or_default().push(Listener {
                    ip: port.traddr.clone(),
                    port: svc_port,
                    nvmet_port_id: Some(port.id),
                });
            }
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&subs_dir).map_err(NvmeofError::Io)? {
            let entry = entry.map_err(NvmeofError::Io)?;
            let nqn = entry.file_name().to_string_lossy().into_owned();
            let ns = entry.path().join("namespaces").join("1");
            let device_path = read_attr_opt(&ns.join("device_path")).unwrap_or_default();
            let ns_uuid = read_attr_opt(&ns.join("device_uuid")).filter(|s| !s.is_empty());
            let enabled = read_attr_opt(&ns.join("enable")).as_deref() == Some("1");
            out.push(LiveShare {
                backing_canonical: resolve_backing_canonical(&device_path),
                device_path,
                ns_uuid,
                listeners: listeners_of.remove(&nqn).unwrap_or_default(),
                enabled,
                subnqn: nqn,
            });
        }
        Ok(out)
    }

    /// Create-or-reuse the port object for one listener at its allocated
    /// id (re-verifying ownership at creation time — the scan→create race
    /// window closes here, loud).
    fn ensure_port(
        &self,
        id: u32,
        ip: &str,
        port: u16,
        ours: &HashSet<String>,
    ) -> Result<(), NvmeofError> {
        let adrfam = adrfam_of(ip)?;
        let p = self.ports_dir().join(id.to_string());
        if p.is_dir() {
            let entry = PortSnapshotEntry {
                id,
                trtype: read_attr_opt(&p.join("addr_trtype")).unwrap_or_default(),
                traddr: read_attr_opt(&p.join("addr_traddr")).unwrap_or_default(),
                trsvcid: read_attr_opt(&p.join("addr_trsvcid")).unwrap_or_default(),
                adrfam: read_attr_opt(&p.join("addr_adrfam")).unwrap_or_default(),
                subsystem_links: Vec::new(),
            };
            let mut links_ours = true;
            let subs = p.join("subsystems");
            if subs.is_dir() {
                for l in fs::read_dir(&subs).map_err(NvmeofError::Io)? {
                    let l = l.map_err(NvmeofError::Io)?;
                    let nqn = l.file_name().to_string_lossy().into_owned();
                    if !ours.contains(&nqn) {
                        links_ours = false;
                    }
                }
            }
            if listener_matches(&entry, ip, port, adrfam) && links_ours {
                return Ok(()); // our matching object — reuse
            }
            return Err(NvmeofError::Refused(format!(
                "nvmet port id {id} was taken by a foreign/incompatible configfs port between \
                 allocation and creation (attrs {}/{}/{}) — retry the share (the deterministic \
                 probe will skip it), or relocate the range with {NVMET_PORT_ID_BASE_ENV}",
                entry.trtype, entry.traddr, entry.trsvcid
            )));
        }
        create_dir_checked(&p)?;
        write_attr(&p.join("addr_trtype"), "tcp")?;
        write_attr(&p.join("addr_adrfam"), adrfam)?;
        write_attr(&p.join("addr_traddr"), ip)?;
        write_attr(&p.join("addr_trsvcid"), &port.to_string())?;
        Ok(())
    }

    /// Attach (or reuse) a loop device for a regular-file backing. The
    /// association is ledger bookkeeping (§6.4 law 5), returned to the
    /// caller for recording — never written into configfs.
    fn attach_loop(&self, backing_path: &str) -> Result<String, NvmeofError> {
        let existing = execute_cmd("losetup", &["-j", backing_path]).unwrap_or_default();
        if let Some(first_line) = existing.lines().next() {
            if let Some(pos) = first_line.find(':') {
                let dev = first_line[..pos].to_string();
                log::info!("reusing existing loop association {dev} for {backing_path}");
                return Ok(dev);
            }
        }
        let free = execute_cmd("losetup", &["-f"]).map_err(NvmeofError::Io)?;
        execute_cmd("losetup", &[&free, backing_path]).map_err(NvmeofError::Io)?;
        Ok(free)
    }

    /// The share-time configfs mutations (also the restore replay body).
    /// Returns the loop device attached for file backings. Every write is
    /// checked; a failure part-way leaves the caller's intent record
    /// claiming whatever exists (§6.4 law 6).
    fn apply_share(&self, record: &ShareRecord) -> Result<Option<String>, NvmeofError> {
        let ns_uuid = record.ns_uuid.as_deref().ok_or_else(|| {
            NvmeofError::Refused(format!(
                "record '{}' carries no ns_uuid — the rebuilt nvmet path always records the \
                 namespace identity before mutating",
                record.subnqn
            ))
        })?;

        // Loop handling for regular-file backings (NoCOW guard already ran
        // at backing preparation). The association is recorded in the
        // ledger IMMEDIATELY (law 5 + law 6: a crash one instruction later
        // leaves a claimed, teardown-reachable device — never an orphan).
        let mut loop_device = None;
        let device_path = if Path::new(&record.backing_path).is_file() {
            let dev = self.attach_loop(&record.backing_path)?;
            self.ledger
                .set_loop_device(&record.subnqn, Some(dev.clone()))
                .map_err(NvmeofError::Io)?;
            println!(
                "note: '{}' is served via loop device {dev}; loop devices expose no NVMe \
                 Persistent Reservations, so the single-writer mount guard is DETECTION-grade \
                 on this share (README → Single-writer mount guard).",
                record.backing_path
            );
            loop_device = Some(dev.clone());
            dev
        } else {
            record.backing_path.clone()
        };

        let sub_dir = self.subsystems_dir().join(&record.subnqn);
        create_dir_checked(&sub_dir)?;
        if record.allow_hosts.is_empty() {
            write_attr(&sub_dir.join("attr_allow_any_host"), "1")?;
        } else {
            write_attr(&sub_dir.join("attr_allow_any_host"), "0")?;
            for host in &record.allow_hosts {
                let host_obj = self.hosts_dir().join(host);
                create_dir_checked(&host_obj)?;
                let link = sub_dir.join("allowed_hosts").join(host);
                create_dir_checked(&sub_dir.join("allowed_hosts"))?;
                if !link.exists() {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&host_obj, &link).map_err(|e| {
                        NvmeofError::Io(io::Error::new(
                            e.kind(),
                            format!("configfs allowed_hosts link {} failed: {e}", link.display()),
                        ))
                    })?;
                }
            }
        }

        // Namespace index structurally fixed at 1 (§6.6).
        let ns_dir = sub_dir.join("namespaces").join("1");
        create_dir_checked(&ns_dir)?;
        write_attr(&ns_dir.join("device_path"), &device_path)?;
        // Identity + PR enablement are stamped BEFORE enable.
        write_attr(&ns_dir.join("device_uuid"), ns_uuid)?;
        if ns_dir.join("resv_enable").exists() {
            write_attr(&ns_dir.join("resv_enable"), "1")?;
        } else {
            println!(
                "note: kernel exposes no resv_enable knob for '{}' — this namespace serves \
                 WITHOUT NVMe Persistent Reservations; the single-writer mount guard lands \
                 detection-grade here.",
                record.subnqn
            );
        }
        write_attr(&ns_dir.join("enable"), "1")?;

        // One configfs port object per (ip, port) listener.
        let mut ours = self.ledgered_nqns()?;
        ours.insert(record.subnqn.clone());
        for listener in &record.listeners {
            let id = listener.nvmet_port_id.ok_or_else(|| {
                NvmeofError::Refused(format!(
                    "listener {}:{} of '{}' carries no allocated nvmet_port_id",
                    listener.ip, listener.port, record.subnqn
                ))
            })?;
            self.ensure_port(id, &listener.ip, listener.port, &ours)?;
            let links_dir = self.ports_dir().join(id.to_string()).join("subsystems");
            create_dir_checked(&links_dir)?;
            let link = links_dir.join(&record.subnqn);
            if !link.exists() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&sub_dir, &link).map_err(|e| {
                    NvmeofError::Io(io::Error::new(
                        e.kind(),
                        format!("configfs port link {} failed: {e}", link.display()),
                    ))
                })?;
            }
        }
        Ok(loop_device)
    }

    /// Teardown of one record's live objects, child→parent, checked, and
    /// tolerant of already-vanished pieces (§6.4 law 4: a vanished object
    /// is a verified no-op). Never touches foreign state: ports are
    /// removed only when recorded AND link-free; host objects only when
    /// no other ledger record references them.
    fn teardown(&self, rec: &ShareRecord) -> Result<(), NvmeofError> {
        let sub_dir = self.subsystems_dir().join(&rec.subnqn);

        // 1. Port symlinks: the recorded ids, plus a full walk for
        //    N1-era records whose small-int ids were deliberately
        //    untracked (their symlinks are still OURS — children of our
        //    subsystem object).
        let ports_dir = self.ports_dir();
        if ports_dir.is_dir() {
            for entry in fs::read_dir(&ports_dir).map_err(NvmeofError::Io)? {
                let entry = entry.map_err(NvmeofError::Io)?;
                let link = entry.path().join("subsystems").join(&rec.subnqn);
                if link.symlink_metadata().is_ok() {
                    fs::remove_file(&link).map_err(|e| {
                        NvmeofError::Io(io::Error::new(
                            e.kind(),
                            format!("configfs unlink {} failed: {e}", link.display()),
                        ))
                    })?;
                }
            }
        }

        // 2. Namespace: disable (when the attr exists), then remove.
        let ns_dir = sub_dir.join("namespaces").join("1");
        if ns_dir.is_dir() {
            if ns_dir.join("enable").exists() {
                write_attr(&ns_dir.join("enable"), "0")?;
            }
            remove_configfs_object(&ns_dir)?;
        }

        // 3. allowed_hosts links (children of OUR subsystem object).
        let ah_dir = sub_dir.join("allowed_hosts");
        if ah_dir.is_dir() {
            for entry in fs::read_dir(&ah_dir).map_err(NvmeofError::Io)? {
                let entry = entry.map_err(NvmeofError::Io)?;
                fs::remove_file(entry.path()).map_err(|e| {
                    NvmeofError::Io(io::Error::new(
                        e.kind(),
                        format!(
                            "configfs allowed_hosts unlink {} failed: {e}",
                            entry.path().display()
                        ),
                    ))
                })?;
            }
        }

        // 4. The subsystem object itself.
        if sub_dir.exists() {
            remove_configfs_object(&sub_dir)?;
        } else {
            println!(
                "note: subsystem '{}' is no longer present in configfs — nothing to tear down \
                 (cleaning the ledger record).",
                rec.subnqn
            );
        }

        // 5. Recorded ports, only when link-free (last-out removal; a
        //    port carrying another share's link survives).
        let mut recorded_ids: Vec<u32> = rec
            .listeners
            .iter()
            .filter_map(|l| l.nvmet_port_id)
            .collect();
        recorded_ids.sort_unstable();
        recorded_ids.dedup();
        for id in recorded_ids {
            let p = self.ports_dir().join(id.to_string());
            if !p.is_dir() {
                continue;
            }
            let links = p.join("subsystems");
            let link_free = !links.is_dir()
                || fs::read_dir(&links)
                    .map_err(NvmeofError::Io)?
                    .next()
                    .is_none();
            if link_free {
                remove_configfs_object(&p)?;
            }
        }

        // 6. Host objects: removed only when no other ledger record
        //    references them (a foreign reference on a real kernel makes
        //    the rmdir fail EBUSY — logged, never fatal: the object is
        //    then observably shared).
        if !rec.allow_hosts.is_empty() {
            let others = self.ledger.load().map_err(NvmeofError::Io)?;
            for host in &rec.allow_hosts {
                let referenced_elsewhere = others
                    .iter()
                    .any(|r| r.subnqn != rec.subnqn && r.allow_hosts.contains(host));
                if referenced_elsewhere {
                    continue;
                }
                let host_obj = self.hosts_dir().join(host);
                if host_obj.exists() {
                    if let Err(e) = fs::remove_dir(&host_obj) {
                        log::warn!(
                            "leaving nvmet host object {} in place (removal failed: {e}) — \
                             another tenant may reference it",
                            host_obj.display()
                        );
                    }
                }
            }
        }

        // 7. The recorded loop association (§6.4 law 5: the ledger, never
        //    configfs, is where teardown learns it).
        if let Some(loop_dev) = &rec.loop_device {
            if Path::new(loop_dev).exists() {
                execute_cmd("losetup", &["-d", loop_dev]).map_err(|e| {
                    NvmeofError::Io(io::Error::other(format!(
                        "losetup -d {loop_dev} failed (detach it manually): {e}"
                    )))
                })?;
                println!("Detached associated loop device '{loop_dev}'.");
            }
        }
        Ok(())
    }

    /// Restore-time equivalence check: existing-and-matching means the
    /// live object serves the record's backing under the record's
    /// identity (`device_path` AND `device_uuid` — §6.6 Restore).
    fn live_matches_record(&self, live: &LiveShare, rec: &ShareRecord) -> Result<(), String> {
        let device_ok = live.backing_canonical == rec.backing_canonical
            || live.device_path == rec.backing_path
            || live.device_path == rec.backing_canonical;
        if !device_ok {
            return Err(format!(
                "live device_path '{}' (backing '{}') does not serve the recorded backing '{}'",
                live.device_path, live.backing_canonical, rec.backing_canonical
            ));
        }
        if let Some(rec_uuid) = rec.ns_uuid.as_deref() {
            match live.ns_uuid.as_deref() {
                Some(live_uuid) if live_uuid.eq_ignore_ascii_case(rec_uuid) => {}
                Some(live_uuid) => {
                    return Err(format!(
                        "live device_uuid '{live_uuid}' mismatches the recorded ns_uuid \
                         '{rec_uuid}' — a changed identity is exactly what the kernel \
                         initiator's namespace revalidation trips on"
                    ));
                }
                None => {
                    return Err(format!(
                        "live namespace exposes no device_uuid while the record carries \
                         ns_uuid '{rec_uuid}'"
                    ));
                }
            }
        }
        Ok(())
    }

    /// One record's restore replay (§6.4 laws 4 + 6; §6.6 Restore).
    fn restore_record(&self, rec: &ShareRecord) -> Result<RestoreOutcome, NvmeofError> {
        match rec.state {
            ShareState::Removing => {
                log::warn!(
                    "restore: resuming interrupted teardown of '{}' (removing intent)",
                    rec.subnqn
                );
                self.teardown(rec)?;
                self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
                Ok(RestoreOutcome::TeardownResumed)
            }
            ShareState::Pending => {
                let live = self
                    .walk_live()?
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
                        // No live objects, or only an unmaterialized
                        // configfs shell (crash between mkdir and the
                        // attribute writes): the interrupted share never
                        // completed and never returned success —
                        // garbage-collect the intent, sweeping the
                        // partial residue tolerantly.
                        log::warn!(
                            "restore: garbage-collecting pending intent '{}' — {} (the \
                             interrupted share never completed)",
                            rec.subnqn,
                            if other.is_some() {
                                "only an unmaterialized configfs shell exists"
                            } else {
                                "no live objects"
                            }
                        );
                        self.teardown(rec)?;
                        self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
                        Ok(RestoreOutcome::GarbageCollectedPending)
                    }
                }
            }
            ShareState::Active => {
                let live = self
                    .walk_live()?
                    .into_iter()
                    .find(|l| l.subnqn == rec.subnqn);
                if let Some(live) = live {
                    return match self.live_matches_record(&live, rec) {
                        Ok(()) => {
                            // Verified no-op; refresh law-5 loop
                            // bookkeeping (the device can differ across
                            // boots).
                            if Path::new(&rec.backing_path).is_file() {
                                let current = execute_cmd("losetup", &["-j", &rec.backing_path])
                                    .ok()
                                    .and_then(|out| {
                                        out.lines()
                                            .next()
                                            .and_then(|l| l.find(':').map(|p| l[..p].to_string()))
                                    });
                                if current != rec.loop_device {
                                    self.ledger
                                        .set_loop_device(&rec.subnqn, current)
                                        .map_err(NvmeofError::Io)?;
                                }
                            }
                            Ok(RestoreOutcome::VerifiedNoop)
                        }
                        Err(why) => Ok(RestoreOutcome::Failed(format!(
                            "'{}' exists live but MISMATCHES its record ({why}) — never \
                             clobbered; resolve manually",
                            rec.subnqn
                        ))),
                    };
                }

                // Gone: re-establish, re-presenting the recorded identity.
                let mut replay = rec.clone();
                if replay.ns_uuid.is_none() {
                    // N1-era record (the pre-rebuild path stamped nothing):
                    // mint the identity ONCE now, record it, and it is
                    // stable from here on.
                    let minted = uuid::Uuid::new_v4().to_string();
                    println!(
                        "note: pre-rebuild record '{}' carried no namespace identity — \
                         stamping ns_uuid {minted} now (stable across restores from here on).",
                        rec.subnqn
                    );
                    self.ledger
                        .update_record(&rec.subnqn, |r| r.ns_uuid = Some(minted.clone()))
                        .map_err(NvmeofError::Io)?;
                    replay.ns_uuid = Some(minted);
                }
                // Port ids: recorded ids are re-presented; N1-era records
                // (null ids) allocate from the reserved range now.
                if replay.listeners.iter().any(|l| l.nvmet_port_id.is_none()) {
                    let snapshot = self.snapshot_ports()?;
                    let ours = self.ledgered_nqns()?;
                    let is_ours = |nqn: &str| ours.contains(nqn);
                    let mut snapshot = snapshot;
                    for l in &mut replay.listeners {
                        if l.nvmet_port_id.is_none() {
                            let alloc = allocate_port_id(
                                self.port_id_base,
                                &l.ip,
                                l.port,
                                &snapshot,
                                &is_ours,
                            )?;
                            l.nvmet_port_id = Some(alloc.id);
                            snapshot.push(PortSnapshotEntry {
                                id: alloc.id,
                                trtype: "tcp".to_string(),
                                traddr: l.ip.clone(),
                                trsvcid: l.port.to_string(),
                                adrfam: adrfam_of(&l.ip)?.to_string(),
                                subsystem_links: vec![replay.subnqn.clone()],
                            });
                        }
                    }
                    let new_listeners = replay.listeners.clone();
                    self.ledger
                        .update_record(&rec.subnqn, |r| r.listeners = new_listeners.clone())
                        .map_err(NvmeofError::Io)?;
                }
                // apply_share records any fresh loop association itself
                // (law 5, mid-flight).
                self.apply_share(&replay)?;
                log::info!("restore: re-shared '{}'", rec.subnqn);
                Ok(RestoreOutcome::Restored)
            }
        }
    }
}

impl TargetStack for NvmetStack {
    fn kind(&self) -> StackKind {
        StackKind::Nvmet
    }

    fn preflight(&self, op: PreflightOp) -> Result<(), PreflightError> {
        match op {
            // Mutating/replaying verbs need the tree (modules + configfs).
            PreflightOp::Share | PreflightOp::Restore => self.ensure_tree(),
            // unshare tolerates an absent tree (vanished objects are a
            // verified no-op); list reads whatever is there.
            PreflightOp::Unshare | PreflightOp::List => Ok(()),
        }
    }

    fn share(&self, req: &ShareRequest) -> Result<ShareRecord, NvmeofError> {
        self.ensure_tree()?;
        // Live-state duplicate guard BEFORE any side effect (§6.4: the
        // guard is ledger + live state; `begin_share` below re-checks the
        // ledger half atomically under the ledger flock).
        self.duplicate_guard(req)?;

        // Port pre-scan + allocation (read-only), so the pending intent
        // record claims the ids it will create (§6.4 law 6: the teardown
        // law needs all of them, even across a crash window).
        let snapshot = self.snapshot_ports()?;
        let mut ours = self.ledgered_nqns()?;
        ours.insert(req.subnqn.clone());
        let is_ours = |nqn: &str| ours.contains(nqn);
        let mut snapshot = snapshot;
        let mut listeners = req.listeners.clone();
        for l in &mut listeners {
            let alloc = allocate_port_id(self.port_id_base, &l.ip, l.port, &snapshot, &is_ours)?;
            l.nvmet_port_id = Some(alloc.id);
            if !alloc.reuse {
                snapshot.push(PortSnapshotEntry {
                    id: alloc.id,
                    trtype: "tcp".to_string(),
                    traddr: l.ip.clone(),
                    trsvcid: l.port.to_string(),
                    adrfam: adrfam_of(&l.ip)?.to_string(),
                    subsystem_links: vec![req.subnqn.clone()],
                });
            }
        }

        let mut record = ShareRecord {
            subnqn: req.subnqn.clone(),
            stack: StackKind::Nvmet,
            state: ShareState::Pending,
            backing_path: req.backing_path.clone(),
            backing_canonical: req.backing_canonical.clone(),
            nsid: None, // structurally fixed at 1, never recorded (§6.4)
            ns_uuid: Some(req.ns_uuid.clone()),
            listeners,
            bdev_name: None,
            ptpl_file: None,
            loop_device: None,
            created_utc: super::ledger::utc_now_rfc3339(),
            allow_hosts: req.allow_hosts.clone(),
            adopted_from: None,
        };

        // §6.4 law 6: the pending intent is recorded BEFORE the first
        // stack mutation.
        self.ledger.begin_share(&record).map_err(NvmeofError::Io)?;

        match self.apply_share(&record) {
            Ok(loop_device) => {
                record.loop_device = loop_device;
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
                     'squeezefs nvmeof restore' or tear down with 'squeezefs nvmeof unshare {}'",
                    record.subnqn,
                    record.subnqn
                );
                Err(e)
            }
        }
    }

    fn unshare(&self, rec: &ShareRecord) -> Result<(), NvmeofError> {
        // §6.4 law 6: flip to `removing` BEFORE the first teardown write;
        // delete the record only after teardown completes.
        self.ledger
            .mark_removing(&rec.subnqn)
            .map_err(NvmeofError::Io)?;
        self.teardown(rec)?;
        self.ledger.delete(&rec.subnqn).map_err(NvmeofError::Io)?;
        Ok(())
    }

    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        self.walk_live()
    }

    fn restore(&self, recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError> {
        self.ensure_tree()?;
        let mut report = RestoreReport::default();
        for rec in recs {
            if rec.stack != StackKind::Nvmet {
                report.entries.push(RestoreEntry {
                    subnqn: rec.subnqn.clone(),
                    outcome: RestoreOutcome::Skipped(format!(
                        "recorded on the {} stack — not replayed by the nvmet stack (bare \
                         `restore` dispatches each record to its recorded stack)",
                        rec.stack.as_str()
                    )),
                });
                continue;
            }
            // Per-share errors are collected, not short-circuited
            // (a report, not a first-failure bail — §6.1).
            let outcome = match self.restore_record(rec) {
                Ok(outcome) => outcome,
                Err(e) => RestoreOutcome::Failed(e.to_string()),
            };
            report.entries.push(RestoreEntry {
                subnqn: rec.subnqn.clone(),
                outcome,
            });
        }
        Ok(report)
    }

    fn target_status(&self) -> Result<TargetStatus, NvmeofError> {
        let configfs_mounted = self.subsystems_dir().is_dir();
        let mut subsystems = 0usize;
        let mut namespaces = 0usize;
        let mut resv_enabled = 0usize;
        if configfs_mounted {
            for entry in fs::read_dir(self.subsystems_dir()).map_err(NvmeofError::Io)? {
                let entry = entry.map_err(NvmeofError::Io)?;
                subsystems += 1;
                let ns_root = entry.path().join("namespaces");
                if ns_root.is_dir() {
                    for ns in fs::read_dir(&ns_root).map_err(NvmeofError::Io)? {
                        let ns = ns.map_err(NvmeofError::Io)?;
                        namespaces += 1;
                        if read_attr_opt(&ns.path().join("resv_enable")).as_deref() == Some("1") {
                            resv_enabled += 1;
                        }
                    }
                }
            }
        }
        let ports = if self.ports_dir().is_dir() {
            fs::read_dir(self.ports_dir())
                .map_err(NvmeofError::Io)?
                .count()
        } else {
            0
        };
        Ok(TargetStatus {
            stack: StackKind::Nvmet,
            modules_present: Path::new("/sys/module/nvmet").exists(),
            configfs_mounted,
            subsystems,
            namespaces,
            ports,
            resv_enabled_namespaces: resv_enabled,
        })
    }
}
