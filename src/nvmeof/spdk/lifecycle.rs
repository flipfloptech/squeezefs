//! SPDK target lifecycle (`docs/design-nvmeof-target-management.md`
//! §6.5, PR 3/N3): pinned install, pidfile-direct start/stop, status,
//! systemd-unit emission, and the client-side `save_config`/
//! `load_config` composition (the §6.4 SPDK persistence law's
//! mechanics).
//!
//! **Version pinning**: `SPDK_PINNED_TAG` + `SPDK_PINNED_COMMIT` are the
//! only release this binary manages (evidence is version-bound —
//! scoping §3/§4 measured v26.05). `target install` verifies the cloned
//! HEAD against the pinned sha (tag-spoof defense) and refuses
//! dirty/unverified checkouts loud. Pin bumps are deliberate PRs gated
//! on a full fidelity-tier rerun (Risks R1).
//!
//! **Lifecycle owner**: systemd unit, **emitted never installed**
//! (`target systemd-unit`, the dev_substrate precedent) — plus the
//! pidfile **direct mode** (`target start`) for rigs and dev boxes.
//!
//! **No system mutation without consent**: `target install` probes the
//! toolchain and stops with the distro package list; `--with-pkgdep` is
//! the explicit opt-in that runs SPDK's `pkgdep.sh` (and never
//! `pip --break-system-packages`).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::super::ledger::{utc_now_rfc3339, Ledger};
use super::super::stack::{NvmeofError, PreflightError, ShareState};
use super::super::StackKind;
use super::hugepages;
use super::rpc::{SpdkRpcClient, SpdkRpcError, SpdkVersion};
use super::{SpdkPaths, SPDK_TGT_BIN_ENV};

/// The pinned SPDK release tag (§6.5).
pub const SPDK_PINNED_TAG: &str = "v26.05";
/// The pinned commit sha `v26.05` must resolve to (verified after clone —
/// tag-spoof defense; `.agents/spdk-scoping/scoping-report.md` §3.1).
pub const SPDK_PINNED_COMMIT: &str = "d519b163cbc0e2f28c35d9bc86d610da368b032c";
/// Upstream repository.
pub const SPDK_GIT_URL: &str = "https://github.com/spdk/spdk.git";
/// The target-serving configure surface (the scoping build recipe).
pub const SPDK_CONFIGURE_ARGS: &[&str] = &[
    "--disable-tests",
    "--disable-unit-tests",
    "--disable-examples",
];

/// RPC-liveness poll budget after spawn (§6.5 — the rig's proven loop).
pub const START_RPC_POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// RPC-liveness poll cadence.
pub const START_RPC_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// `target stop` SIGTERM grace before SIGKILL (§6.5).
pub const STOP_GRACE: Duration = Duration::from_secs(10);
/// `spdk_tgt -s` default (MiB) — scoping §5 sizing rule.
pub const DEFAULT_DPDK_MEM_MB: u64 = 1024;

// ---------------------------------------------------------------------------
// pidfile state machine
// ---------------------------------------------------------------------------

/// What the pidfile says about the target (pidfile direct mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfileState {
    /// No pidfile.
    NotRunning,
    /// Pidfile names a dead pid (crash leftover — removed loudly by the
    /// verbs, never silently reused).
    Stale(i32),
    /// Pidfile names a live pid.
    Running(i32),
}

/// Read + classify the pidfile. Absent → `NotRunning`; unparseable
/// content is an error (corrupt state is never guessed around).
pub fn read_pidfile_state(pidfile: &Path) -> io::Result<PidfileState> {
    let raw = match fs::read_to_string(pidfile) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PidfileState::NotRunning),
        Err(e) => return Err(e),
    };
    let pid: i32 = raw.trim().parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "pidfile {} holds non-numeric '{}' ({e}) — remove it by hand after verifying \
                 no spdk_tgt is running",
                pidfile.display(),
                raw.trim()
            ),
        )
    })?;
    if pid_alive(pid) {
        Ok(PidfileState::Running(pid))
    } else {
        Ok(PidfileState::Stale(pid))
    }
}

/// `kill(pid, 0)` liveness.
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs error checking only — no signal is sent.
    unsafe { libc::kill(pid, 0) == 0 }
}

// ---------------------------------------------------------------------------
// reactor core-mask math (§6.5 reactor/core budget)
// ---------------------------------------------------------------------------

/// Parse a kernel cpu-list string (`"0-31"`, `"0-3,8,10-11"`).
pub fn parse_cpu_list(s: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty element in cpu list '{s}'"));
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: u32 = lo.trim().parse().map_err(|e| format!("cpu '{lo}': {e}"))?;
                let hi: u32 = hi.trim().parse().map_err(|e| format!("cpu '{hi}': {e}"))?;
                if lo > hi {
                    return Err(format!("inverted range '{part}' in cpu list '{s}'"));
                }
                out.extend(lo..=hi);
            }
            None => out.push(part.parse().map_err(|e| format!("cpu '{part}': {e}"))?),
        }
    }
    if out.is_empty() {
        return Err(format!("cpu list '{s}' selects no cpus"));
    }
    Ok(out)
}

/// Hex mask (`"0x…"`) selecting exactly `cores` (arbitrary width — cpu
/// ids beyond 63 render as long hex strings, which SPDK `-m` accepts).
pub fn mask_for_cores(cores: &[u32]) -> Result<String, String> {
    let max = *cores.iter().max().ok_or("no cores selected")?;
    let mut nibbles = vec![0u8; (max as usize / 4) + 1];
    for &core in cores {
        let idx = nibbles.len() - 1 - (core as usize / 4);
        nibbles[idx] |= 1 << (core % 4);
    }
    let hex: String = nibbles.iter().map(|n| format!("{n:x}")).collect();
    Ok(format!("0x{}", hex.trim_start_matches('0')))
}

/// The §6.5 default: one reactor on the **highest-numbered online CPU**
/// (keeps CPU0/IRQ locality alone; scoping used core 24).
pub fn default_core_mask(online: &[u32]) -> Result<String, String> {
    let top = *online.iter().max().ok_or("no online cpus")?;
    mask_for_cores(&[top])
}

/// `--cores N`: N cores from the top down.
pub fn top_cores_mask(online: &[u32], n: u32) -> Result<String, String> {
    if n == 0 {
        return Err("--cores 0 selects no reactor cores".to_string());
    }
    if n as usize > online.len() {
        return Err(format!(
            "--cores {n} exceeds the {} online cpus",
            online.len()
        ));
    }
    let mut sorted: Vec<u32> = online.to_vec();
    sorted.sort_unstable();
    let top: Vec<u32> = sorted.into_iter().rev().take(n as usize).collect();
    mask_for_cores(&top)
}

/// Online cpu ids from `/sys/devices/system/cpu/online`.
pub fn online_cpus() -> io::Result<Vec<u32>> {
    let raw = fs::read_to_string("/sys/devices/system/cpu/online")?;
    parse_cpu_list(raw.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Validate an explicit `--core-mask` value (hex, nonzero).
fn validate_core_mask(mask: &str) -> Result<(), NvmeofError> {
    let hex = mask.strip_prefix("0x").ok_or_else(|| {
        NvmeofError::Refused(format!(
            "--core-mask '{mask}' must be a hex mask like 0x80000000"
        ))
    })?;
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(NvmeofError::Refused(format!(
            "--core-mask '{mask}' is not valid hex"
        )));
    }
    if hex.chars().all(|c| c == '0') {
        return Err(NvmeofError::Refused(format!(
            "--core-mask '{mask}' selects no reactor cores"
        )));
    }
    Ok(())
}

/// Resolve the reactor mask from the §6.2 flags (validated `--core-mask`
/// > `--cores N` top-down > the highest-online-CPU default).
fn resolve_core_mask(core_mask: Option<String>, cores: Option<u32>) -> Result<String, NvmeofError> {
    if let Some(mask) = core_mask {
        validate_core_mask(&mask)?;
        return Ok(mask);
    }
    let online = online_cpus().map_err(NvmeofError::Io)?;
    let mask = match cores {
        Some(n) => top_cores_mask(&online, n),
        None => default_core_mask(&online),
    };
    mask.map_err(NvmeofError::Refused)
}

// ---------------------------------------------------------------------------
// preflight ladder rungs (§6.2/§6.3 — the G3 loud-fail matrix)
// ---------------------------------------------------------------------------

const NO_FALLBACK_NOTE: &str = "note: SqueezeFS never falls back between target stacks \
                                automatically —\n      they differ in reservation persistence \
                                (PTPL) and latency envelope.";

/// Rung 1: the pinned binary is present, or `SQUEEZEFS_SPDK_TGT_BIN`
/// substitutes it (returned warning = the loud unpinned line). Missing
/// binary names `target install`; a set-but-missing override names the
/// env var — never a silent fallthrough to the pin.
pub fn preflight_binary(paths: &SpdkPaths) -> Result<(PathBuf, Option<String>), PreflightError> {
    if let Some(override_bin) = &paths.bin_override {
        if !override_bin.exists() {
            return Err(PreflightError {
                message: format!(
                    "error: SPDK target stack unavailable: {SPDK_TGT_BIN_ENV}={} does not \
                     exist\n  fix or unset the override — SqueezeFS never falls through to \
                     the pinned binary on a broken override.\n{NO_FALLBACK_NOTE}",
                    override_bin.display()
                ),
            });
        }
        return Ok((
            override_bin.clone(),
            Some(format!(
                "warning: using unpinned spdk_tgt via {SPDK_TGT_BIN_ENV}={} — this squeezefs \
                 pins {SPDK_PINNED_TAG} (commit {SPDK_PINNED_COMMIT}); the guard/PTPL evidence \
                 is version-bound",
                override_bin.display()
            )),
        ));
    }
    let pinned = paths.pinned_bin();
    if !pinned.exists() {
        return Err(PreflightError {
            message: format!(
                "error: SPDK target stack unavailable: pinned spdk_tgt binary not installed \
                 at {}\n  install the pinned release ({SPDK_PINNED_TAG}, commit \
                 {SPDK_PINNED_COMMIT}):\n    sudo squeezefs nvmeof target install\n  (dev/rig \
                 boxes may point {SPDK_TGT_BIN_ENV} at an existing build — loud, unpinned)\n  \
                 if you intended the kernel target stack, select it explicitly:\n    sudo \
                 squeezefs nvmeof <verb> … --target-stack nvmet\n{NO_FALLBACK_NOTE}",
                pinned.display()
            ),
        });
    }
    Ok((pinned, None))
}

/// Rung 3: the RPC socket answers `spdk_get_version` within the connect
/// timeout. The failure text is the §6.2 designed dead-RPC runbook
/// (check/start verbs, the explicit-nvmet choice, the never-falls-back
/// law).
pub fn preflight_rpc(paths: &SpdkPaths) -> Result<SpdkVersion, PreflightError> {
    let sock = paths.rpc_sock();
    let client = SpdkRpcClient::new(&sock);
    client.version().map_err(|e| {
        let condition = match &e {
            SpdkRpcError::Connect { .. } => e.to_string(),
            other => format!("RPC socket {} not answering ({other})", sock.display()),
        };
        PreflightError {
            message: format!(
                "error: SPDK target stack unavailable: {condition}\n  the target is not \
                 running or is unresponsive:\n    check:  sudo squeezefs nvmeof target \
                 status\n    start:  sudo squeezefs nvmeof target start\n  if you intended \
                 the kernel target stack, select it explicitly:\n    sudo squeezefs nvmeof \
                 share … --target-stack nvmet\n{NO_FALLBACK_NOTE}"
            ),
        }
    })
}

/// Rung-4 gating class (§6.2 flag placement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftPolicy {
    /// `share`/`unshare`/`restore`/`target start`: refuse on drift unless
    /// `--accept-version-drift`.
    Mutating { accept_version_drift: bool },
    /// `target stop`: refusing shutdown on version grounds inverts the
    /// risk — warn and proceed.
    WarnAndProceed,
    /// `target status`: the diagnostic verb never refuses on drift — it
    /// reports (`rpc.drift`).
    ReportOnly,
}

/// Rung 4: reported version vs the pin. `Ok(None)` = no drift (or a
/// report-only policy); `Ok(Some(line))` = drift allowed under the policy
/// (the loud warning to print); `Err` = the G3 wrong-version refusal.
pub fn gate_version_drift(
    version: &SpdkVersion,
    policy: DriftPolicy,
) -> Result<Option<String>, PreflightError> {
    let Some(class) = version.drift() else {
        return Ok(None);
    };
    let class_word = match class {
        super::rpc::VersionDrift::Minor => "minor",
        super::rpc::VersionDrift::Major => "MAJOR",
    };
    match policy {
        DriftPolicy::Mutating {
            accept_version_drift: false,
        } => Err(PreflightError {
            message: format!(
                "error: SPDK target version drift: the target reports '{raw}' but this \
                 squeezefs pins {SPDK_PINNED_TAG} ({class_word} drift)\n  the guard/PTPL \
                 evidence is version-bound; either:\n    run the pinned release:  sudo \
                 squeezefs nvmeof target install && sudo squeezefs nvmeof target start\n    \
                 or proceed against the drifted target explicitly: add \
                 --accept-version-drift (mutating verbs only)\nnote: 'target status' always \
                 reports drift; 'target stop' warns and proceeds.",
                raw = version.raw
            ),
        }),
        DriftPolicy::Mutating {
            accept_version_drift: true,
        } => Ok(Some(format!(
            "warning: proceeding under --accept-version-drift: target reports '{}' vs pinned \
             {SPDK_PINNED_TAG} ({class_word} drift)",
            version.raw
        ))),
        DriftPolicy::WarnAndProceed => Ok(Some(format!(
            "warning: SPDK version drift: target reports '{}' vs pinned {SPDK_PINNED_TAG} \
             ({class_word} drift) — proceeding (refusing shutdown on version grounds would \
             invert the risk)",
            version.raw
        ))),
        DriftPolicy::ReportOnly => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// toolchain probe (§6.5 — no pkgdep.sh without consent)
// ---------------------------------------------------------------------------

/// One build dependency the install preflight probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDep {
    /// Probe target: a binary name (`gcc`) or a header path
    /// (`/usr/include/libaio.h`).
    pub probe: &'static str,
    /// Distro package hint printed by the refusal (`debian … / fedora …`).
    pub package_hint: &'static str,
}

/// The probed toolchain (binaries + headers the §6.5 configure needs —
/// the set the scoping build exercised).
pub const SPDK_TOOLCHAIN: &[ToolDep] = &[
    ToolDep {
        probe: "git",
        package_hint: "git",
    },
    ToolDep {
        probe: "gcc",
        package_hint: "gcc (build-essential / gcc)",
    },
    ToolDep {
        probe: "g++",
        package_hint: "g++ (build-essential / gcc-c++)",
    },
    ToolDep {
        probe: "make",
        package_hint: "make",
    },
    ToolDep {
        probe: "python3",
        package_hint: "python3",
    },
    ToolDep {
        probe: "pkg-config",
        package_hint: "pkg-config / pkgconf",
    },
    ToolDep {
        probe: "patchelf",
        package_hint: "patchelf",
    },
    ToolDep {
        probe: "/usr/include/libaio.h",
        package_hint: "libaio-dev / libaio-devel",
    },
    ToolDep {
        probe: "/usr/include/numa.h",
        package_hint: "libnuma-dev / numactl-devel",
    },
    ToolDep {
        probe: "/usr/include/uuid/uuid.h",
        package_hint: "uuid-dev / libuuid-devel",
    },
    ToolDep {
        probe: "/usr/include/openssl/ssl.h",
        package_hint: "libssl-dev / openssl-devel",
    },
];

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
            && fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    })
}

/// Probe the toolchain; returns the missing deps (empty = buildable).
pub fn probe_missing_toolchain() -> Vec<ToolDep> {
    SPDK_TOOLCHAIN
        .iter()
        .filter(|dep| {
            if dep.probe.starts_with('/') {
                !Path::new(dep.probe).exists()
            } else {
                !binary_on_path(dep.probe)
            }
        })
        .copied()
        .collect()
}

/// The §6.5 consent refusal: lists every missing dep with its package
/// hint, states that the default mutates nothing, and names
/// `--with-pkgdep` as the explicit opt-in.
pub fn toolchain_refusal_message(missing: &[ToolDep]) -> String {
    let mut lines = String::from(
        "error: SPDK build toolchain incomplete — squeezefs never mutates system packages \
         without explicit consent.\n  missing:\n",
    );
    for dep in missing {
        lines.push_str(&format!("    {:<32} ({})\n", dep.probe, dep.package_hint));
    }
    lines.push_str(
        "  install the packages above with your package manager, or re-run with explicit \
         consent for SPDK's dependency installer:\n    sudo squeezefs nvmeof target install \
         --with-pkgdep\n  (--with-pkgdep runs the pinned tree's scripts/pkgdep.sh; it never \
         uses pip --break-system-packages)",
    );
    lines
}

/// `--version` must equal the pin (pin bumps are deliberate PRs — R1).
/// Pure grammar validation: runs before the root check.
pub fn validate_install_version(version: Option<&str>) -> Result<(), NvmeofError> {
    match version {
        None => Ok(()),
        Some(v) if v == SPDK_PINNED_TAG => Ok(()),
        Some(v) => Err(NvmeofError::Refused(format!(
            "--version {v} is not the pinned release: this squeezefs manages exactly \
             {SPDK_PINNED_TAG} (commit {SPDK_PINNED_COMMIT}) — the guard/PTPL evidence is \
             version-bound, and pin bumps are deliberate PRs gated on a fidelity-tier rerun \
             (design R1), never a CLI flag"
        ))),
    }
}

// ---------------------------------------------------------------------------
// systemd-unit emission (§6.5 — values BAKED, emitted never installed)
// ---------------------------------------------------------------------------

/// Render the SPDK unit with every value baked at emission time (no
/// variable indirection — systemd expands unset variables to empty and
/// would silently render a malformed spdk_tgt command line).
pub fn render_spdk_unit(
    spdk_tgt_bin: &Path,
    rpc_sock: &Path,
    core_mask: &str,
    dpdk_mem_mb: u64,
    squeezefs_exe: &Path,
) -> String {
    format!(
        "# squeezefs nvmeof target systemd-unit  (stdout; operator installs).\n\
         # Every value below is BAKED by the emitter at emission time: the resolved\n\
         # core mask / DPDK MB from the flags, the pinned spdk_tgt path, and the\n\
         # squeezefs binary path via /proc/self/exe. No environment-variable\n\
         # indirection — systemd expands unset variables to empty and would\n\
         # silently render a malformed spdk_tgt command line.\n\
         [Unit]\n\
         Description=SqueezeFS-managed SPDK NVMe-oF target (pinned {SPDK_PINNED_TAG})\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         StartLimitIntervalSec=60\n\
         StartLimitBurst=5\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} -r {sock} -m {core_mask} -s {dpdk_mem_mb}\n\
         ExecStartPost={exe} nvmeof restore --target-stack spdk\n\
         Restart=always\n\
         RestartSec=2\n\
         LimitMEMLOCK=infinity\n\
         RuntimeDirectory=squeezefs/nvmeof\n\
         RuntimeDirectoryMode=0700\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = spdk_tgt_bin.display(),
        sock = rpc_sock.display(),
        exe = squeezefs_exe.display(),
    )
}

/// The nvmet variant (§6.6): configfs is empty at boot by nature —
/// `Type=oneshot` + `RemainAfterExit=yes`, ExecStart = the product's
/// `restore --target-stack nvmet`.
pub fn render_nvmet_unit(squeezefs_exe: &Path) -> String {
    format!(
        "# squeezefs nvmeof target systemd-unit --target-stack nvmet  (stdout; operator \
         installs).\n\
         # The kernel nvmet target is configfs state — empty at boot by nature; this\n\
         # oneshot unit replays the share ledger through the product's restore verb.\n\
         [Unit]\n\
         Description=SqueezeFS-managed kernel nvmet NVMe-oF target (ledger restore)\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={exe} nvmeof restore --target-stack nvmet\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe = squeezefs_exe.display(),
    )
}

// ---------------------------------------------------------------------------
// save_config / load_config composition (§6.4 SPDK persistence law)
// ---------------------------------------------------------------------------

fn rpc_err(e: SpdkRpcError) -> NvmeofError {
    NvmeofError::Refused(format!("SPDK {e}"))
}

/// Atomic replace (the ledger law-2 discipline): tmp → fsync → rename →
/// fsync parent dir.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!("{file_name}.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Gather the target's running config (`framework_get_subsystems` +
/// per-subsystem `framework_get_config` — the rpc.py `save_config`
/// composition) and write it atomically to `tgt_config`.
pub fn save_config(client: &SpdkRpcClient, tgt_config: &Path) -> Result<(), NvmeofError> {
    let subsystems = client
        .call("framework_get_subsystems", None)
        .map_err(rpc_err)?;
    let subsystems = subsystems.as_array().ok_or_else(|| {
        NvmeofError::Refused(format!(
            "SPDK framework_get_subsystems returned a non-array: {subsystems}"
        ))
    })?;
    let mut out = Vec::with_capacity(subsystems.len());
    for elem in subsystems {
        let name = elem
            .get("subsystem")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                NvmeofError::Refused(format!(
                    "SPDK framework_get_subsystems entry carries no 'subsystem' name: {elem}"
                ))
            })?;
        let config = client
            .call("framework_get_config", Some(json!({ "name": name })))
            .map_err(rpc_err)?;
        out.push(json!({ "subsystem": name, "config": config }));
    }
    let mut body = serde_json::to_vec_pretty(&json!({ "subsystems": out }))
        .map_err(|e| NvmeofError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
    body.push(b'\n');
    write_atomic(tgt_config, &body).map_err(NvmeofError::Io)
}

/// What `load_config` applied / had to skip.
#[derive(Debug, Clone, Default)]
pub struct LoadConfigReport {
    pub applied: usize,
    /// Methods present in the config the current RPC state cannot call
    /// (STARTUP-only entries on a runtime target) — reported loud, never
    /// silently dropped.
    pub skipped: Vec<String>,
}

fn rpc_methods(client: &SpdkRpcClient) -> Result<HashSet<String>, NvmeofError> {
    let methods = client.call("rpc_get_methods", None).map_err(rpc_err)?;
    Ok(methods
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Replay a saved config into the running target (the rpc.py
/// `load_config` algorithm: `rpc_get_methods` gate, pass loop,
/// `framework_start_init` handling), collecting a loud report. A replayed
/// method failing is a hard error — the caller owns the blast radius.
pub fn load_config(
    client: &SpdkRpcClient,
    config: &Value,
) -> Result<LoadConfigReport, NvmeofError> {
    // (subsystem name, remaining config entries) — empty subsystems drop.
    let mut subsystems: Vec<(String, Vec<Value>)> = config
        .get("subsystems")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    (
                        s.get("subsystem")
                            .and_then(Value::as_str)
                            .unwrap_or("<unnamed>")
                            .to_string(),
                        s.get("config")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .filter(|(_, cfg)| !cfg.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut allowed = rpc_methods(client)?;
    let mut report = LoadConfigReport::default();
    loop {
        let mut progressed = false;
        for (_, entries) in subsystems.iter_mut() {
            let mut remaining = Vec::with_capacity(entries.len());
            for entry in entries.drain(..) {
                let method = entry
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !method.is_empty() && allowed.contains(&method) {
                    client
                        .call(&method, entry.get("params").cloned())
                        .map_err(|e| {
                            NvmeofError::Refused(format!(
                                "load_config: replaying '{method}' failed: SPDK {e}"
                            ))
                        })?;
                    report.applied += 1;
                    progressed = true;
                } else {
                    remaining.push(entry);
                }
            }
            *entries = remaining;
        }
        subsystems.retain(|(_, cfg)| !cfg.is_empty());
        if subsystems.is_empty() {
            break;
        }
        // A target started --wait-for-rpc parks in STARTUP state; kicking
        // framework_start_init unlocks the runtime methods (rpc.py parity).
        if allowed.contains("framework_start_init") {
            client.call("framework_start_init", None).map_err(rpc_err)?;
            allowed = rpc_methods(client)?;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    for (_, entries) in &subsystems {
        for entry in entries {
            report.skipped.push(
                entry
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing method>")
                    .to_string(),
            );
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// install (§6.5)
// ---------------------------------------------------------------------------

fn tail_of(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(content) => {
            let all: Vec<&str> = content.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(_) => String::from("<no log>"),
    }
}

/// Run one install step with its output appended to the build log; fail
/// loud with the step named + the log tail.
fn run_step(step: &str, log_path: &Path, cmd: &mut Command) -> Result<(), NvmeofError> {
    println!("target install: {step} … (log: {})", log_path.display());
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(NvmeofError::Io)?;
    let mut banner = log.try_clone().map_err(NvmeofError::Io)?;
    let _ = writeln!(banner, "\n=== {step} @ {} ===", utc_now_rfc3339());
    let status = cmd
        .stdout(Stdio::from(log.try_clone().map_err(NvmeofError::Io)?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|e| {
            NvmeofError::Refused(format!(
                "target install: step '{step}' failed to spawn: {e}"
            ))
        })?;
    if !status.success() {
        return Err(NvmeofError::Refused(format!(
            "target install: step '{step}' failed ({status})\n--- last log lines ({}) ---\n{}",
            log_path.display(),
            tail_of(log_path, 20)
        )));
    }
    Ok(())
}

fn git_output(src: &Path, args: &[&str]) -> Result<String, NvmeofError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .output()
        .map_err(|e| NvmeofError::Refused(format!("git {args:?} failed to spawn: {e}")))?;
    if !out.status.success() {
        return Err(NvmeofError::Refused(format!(
            "git {args:?} in {} failed: {}",
            src.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Verify an existing checkout is exactly the pin and clean — anything
/// else refuses loud (dirty/unverified checkouts are never built).
fn verify_checkout(src: &Path) -> Result<(), NvmeofError> {
    let head = git_output(src, &["rev-parse", "HEAD"]).map_err(|e| {
        NvmeofError::Refused(format!(
            "target install: {} exists but cannot be verified as a git checkout ({e})\n  \
             remove the prefix and re-run: sudo rm -r {} && sudo squeezefs nvmeof target \
             install",
            src.display(),
            src.parent().unwrap_or(src).display()
        ))
    })?;
    if head != SPDK_PINNED_COMMIT {
        return Err(NvmeofError::Refused(format!(
            "target install: existing checkout {} is at commit {head}, not the pinned \
             {SPDK_PINNED_COMMIT} ({SPDK_PINNED_TAG}) — refusing to build an unverified \
             tree.\n  remove it and re-run: sudo rm -r {} && sudo squeezefs nvmeof target \
             install",
            src.display(),
            src.parent().unwrap_or(src).display()
        )));
    }
    let dirt = git_output(src, &["status", "--porcelain"])?;
    if !dirt.is_empty() {
        return Err(NvmeofError::Refused(format!(
            "target install: existing checkout {} is DIRTY — refusing to build unverifiable \
             sources:\n{}\n  restore it (git -C {} checkout -- . && git -C {} clean -fd) or \
             remove the prefix and re-run target install",
            src.display(),
            dirt,
            src.display(),
            src.display()
        )));
    }
    Ok(())
}

fn write_build_info(paths: &SpdkPaths) -> Result<(), NvmeofError> {
    let cc = Command::new("gcc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let info = format!(
        "tag: {SPDK_PINNED_TAG}\ncommit: {SPDK_PINNED_COMMIT}\nconfigure: {}\ncc: {cc}\nbuilt: {}\n",
        SPDK_CONFIGURE_ARGS.join(" "),
        utc_now_rfc3339(),
    );
    write_atomic(&paths.build_info(), info.as_bytes()).map_err(NvmeofError::Io)
}

/// `target install` (§6.5): pinned clone + sha verify + build into the
/// prefix; `--with-pkgdep` consent; build-info provenance; idempotent on
/// a verified complete install; refuses dirty/unverified checkouts loud.
pub fn install(
    paths: &SpdkPaths,
    version: Option<&str>,
    with_pkgdep: bool,
) -> Result<(), NvmeofError> {
    validate_install_version(version)?;
    let prefix = &paths.install_prefix;
    let src = paths.src_dir();
    let log = paths.build_log();

    if src.exists() {
        verify_checkout(&src)?;
        if paths.pinned_bin().exists() && paths.build_info().exists() {
            println!(
                "SPDK {SPDK_PINNED_TAG} already installed and verified at {} (commit \
                 {SPDK_PINNED_COMMIT}) — nothing to do.",
                prefix.display()
            );
            return Ok(());
        }
        println!(
            "target install: verified pinned checkout present at {} — resuming the build.",
            src.display()
        );
    } else {
        // Toolchain gate BEFORE any clone: the §6.5 consent law.
        let missing = probe_missing_toolchain();
        if !missing.is_empty() && !with_pkgdep {
            return Err(NvmeofError::Refused(toolchain_refusal_message(&missing)));
        }
        fs::create_dir_all(prefix).map_err(NvmeofError::Io)?;
        run_step(
            &format!("git clone --depth 1 --branch {SPDK_PINNED_TAG}"),
            &log,
            Command::new("git").args([
                "clone",
                "--depth",
                "1",
                "--branch",
                SPDK_PINNED_TAG,
                SPDK_GIT_URL,
                &src.display().to_string(),
            ]),
        )?;
        // Tag-spoof defense: HEAD must be the pinned sha, else the tree
        // is removed (never left at the install prefix) and we refuse.
        let head = git_output(&src, &["rev-parse", "HEAD"])?;
        if head != SPDK_PINNED_COMMIT {
            let _ = fs::remove_dir_all(&src);
            return Err(NvmeofError::Refused(format!(
                "target install: cloned tag {SPDK_PINNED_TAG} resolves to commit {head}, NOT \
                 the pinned {SPDK_PINNED_COMMIT} — possible upstream tag move or spoof; the \
                 clone was removed. Verify upstream before retrying (pin bumps are deliberate \
                 PRs — design R1)."
            )));
        }
        println!("target install: verified HEAD == pinned {SPDK_PINNED_COMMIT}");
        run_step(
            "git submodule update --init --depth 1",
            &log,
            Command::new("git").arg("-C").arg(&src).args([
                "submodule",
                "update",
                "--init",
                "--depth",
                "1",
            ]),
        )?;
        if with_pkgdep {
            if missing.is_empty() {
                println!(
                    "target install: toolchain already complete — consent given but \
                     scripts/pkgdep.sh not needed; skipping the system mutation."
                );
            } else {
                run_step(
                    "scripts/pkgdep.sh (explicit --with-pkgdep consent)",
                    &log,
                    Command::new("bash").arg(src.join("scripts/pkgdep.sh")),
                )?;
            }
        }
    }

    let mut configure = Command::new("./configure");
    configure.args(SPDK_CONFIGURE_ARGS).current_dir(&src);
    run_step(
        &format!("./configure {}", SPDK_CONFIGURE_ARGS.join(" ")),
        &log,
        &mut configure,
    )?;
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    run_step(
        &format!("make -j{jobs}"),
        &log,
        Command::new("make")
            .arg(format!("-j{jobs}"))
            .current_dir(&src),
    )?;

    let built = src.join("build/bin/spdk_tgt");
    if !built.exists() {
        return Err(NvmeofError::Refused(format!(
            "target install: build completed but {} is missing — see the build log {}",
            built.display(),
            log.display()
        )));
    }
    // `<prefix>/build` → `src/build`, so the §6.5 canonical binary path
    // (`<prefix>/build/bin/spdk_tgt`, the systemd-unit ExecStart) holds.
    let build_link = prefix.join("build");
    match fs::symlink_metadata(&build_link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(&build_link).map_err(NvmeofError::Io)?
        }
        Ok(_) => {
            return Err(NvmeofError::Refused(format!(
                "target install: {} exists and is not the expected symlink — remove it by hand",
                build_link.display()
            )));
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink("src/build", &build_link).map_err(NvmeofError::Io)?;
    write_build_info(paths)?;
    println!(
        "SPDK {SPDK_PINNED_TAG} (commit {SPDK_PINNED_COMMIT}) installed at {}\n  binary:     \
         {}\n  provenance: {}",
        prefix.display(),
        paths.pinned_bin().display(),
        paths.build_info().display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// start / stop (§6.5 pidfile direct mode)
// ---------------------------------------------------------------------------

/// `target start` options (§6.2 grammar).
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub core_mask: Option<String>,
    pub cores: Option<u32>,
    pub dpdk_mem_mb: u64,
    pub accept_version_drift: bool,
}

fn ensure_run_dir(paths: &SpdkPaths) -> io::Result<()> {
    fs::create_dir_all(&paths.run_dir)?;
    fs::set_permissions(&paths.run_dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// A short-budget client for liveness polling.
fn poll_client(sock: &Path) -> SpdkRpcClient {
    SpdkRpcClient::with_timeouts(
        sock,
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
}

/// Find a running spdk_tgt serving OUR socket without a pidfile (systemd
/// mode, or a crash window before the pidfile write): scan /proc for a
/// cmdline carrying `-r <sock>`.
pub fn find_target_pid_by_socket(sock: &Path) -> Option<i32> {
    let want = sock.display().to_string();
    for entry in fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        for pair in args.windows(2) {
            if pair[0] == "-r" && pair[1] == want {
                return Some(pid);
            }
        }
    }
    None
}

fn kill_pid(pid: i32, signal: libc::c_int) {
    // SAFETY: plain kill(2) on a pid we own/track; failure is inspected
    // via pid_alive by the callers.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// `target start` (§6.5 pidfile direct mode): preflight → spawn →
/// RPC-liveness poll → version handshake → `load_config` if the config
/// exists → write pidfile. Every step fails loud with the step named.
pub fn start(paths: &SpdkPaths, opts: &StartOptions) -> Result<(), NvmeofError> {
    // Preflight rung 1: the binary.
    let (bin, warning) = preflight_binary(paths)?;
    if let Some(w) = warning {
        eprintln!("{w}");
    }

    ensure_run_dir(paths).map_err(NvmeofError::Io)?;
    fs::create_dir_all(paths.spdk_state_dir()).map_err(NvmeofError::Io)?;

    // Already running / stale-state rungs.
    match read_pidfile_state(&paths.pidfile()).map_err(NvmeofError::Io)? {
        PidfileState::Running(pid) => {
            return Err(NvmeofError::Refused(format!(
                "target start: spdk_tgt already running (pid {pid}, pidfile {})\n  check:  \
                 sudo squeezefs nvmeof target status\n  stop:   sudo squeezefs nvmeof target \
                 stop",
                paths.pidfile().display()
            )));
        }
        PidfileState::Stale(pid) => {
            println!(
                "target start: removing stale pidfile {} (pid {pid} is dead)",
                paths.pidfile().display()
            );
            fs::remove_file(paths.pidfile()).map_err(NvmeofError::Io)?;
        }
        PidfileState::NotRunning => {}
    }
    let sock = paths.rpc_sock();
    if sock.exists() {
        if poll_client(&sock).version().is_ok() {
            let hint = find_target_pid_by_socket(&sock)
                .map(|pid| format!(" (pid {pid} — systemd-managed or started by hand?)"))
                .unwrap_or_default();
            return Err(NvmeofError::Refused(format!(
                "target start: a live SPDK target already answers on {}{hint} — refusing to \
                 start a second one.\n  check:  sudo squeezefs nvmeof target status",
                sock.display()
            )));
        }
        println!(
            "target start: removing stale RPC socket {} (no listener)",
            sock.display()
        );
        fs::remove_file(&sock).map_err(NvmeofError::Io)?;
    }

    // Hugepage preflight (the G3 no-hugepages rung).
    hugepages::preflight_free_for_dpdk(
        Path::new(hugepages::HUGEPAGES_2M_SYSFS_DIR),
        opts.dpdk_mem_mb,
    )?;

    // Reactor mask.
    let mask = resolve_core_mask(opts.core_mask.clone(), opts.cores)?;

    // Spawn (output to the run-dir log — never Stdio::null, §6.9).
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.tgt_log())
        .map_err(NvmeofError::Io)?;
    let mut banner = log.try_clone().map_err(NvmeofError::Io)?;
    let _ = writeln!(
        banner,
        "\n=== target start @ {} (-m {mask} -s {}) ===",
        utc_now_rfc3339(),
        opts.dpdk_mem_mb
    );
    let mut cmd = Command::new(&bin);
    cmd.arg("-r")
        .arg(&sock)
        .arg("-m")
        .arg(&mask)
        .arg("-s")
        .arg(opts.dpdk_mem_mb.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(NvmeofError::Io)?))
        .stderr(Stdio::from(log));
    // SAFETY: setsid() in the forked child only detaches it from our
    // session/terminal — async-signal-safe, no allocation.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| {
        NvmeofError::Refused(format!(
            "target start: spawning {} failed: {e}",
            bin.display()
        ))
    })?;
    let pid = child.id() as i32;

    let fail_and_reap = |mut child: std::process::Child, step: &str, detail: String| {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(&sock);
        NvmeofError::Refused(format!(
            "target start: step '{step}' failed: {detail}\n--- spdk_tgt log tail ({}) ---\n{}",
            paths.tgt_log().display(),
            tail_of(&paths.tgt_log(), 20)
        ))
    };

    // RPC-liveness poll (the rig's proven loop: 10 s, 200 ms cadence).
    let deadline = Instant::now() + START_RPC_POLL_TIMEOUT;
    let client = poll_client(&sock);
    let version = loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(fail_and_reap(
                child,
                "spawn",
                format!("spdk_tgt exited during startup ({status})"),
            ));
        }
        match client.version() {
            Ok(v) => break v,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(START_RPC_POLL_INTERVAL);
            }
            Err(e) => {
                return Err(fail_and_reap(
                    child,
                    "RPC-liveness poll",
                    format!(
                        "no answer on {} within {:?}: {e}",
                        sock.display(),
                        START_RPC_POLL_TIMEOUT
                    ),
                ));
            }
        }
    };

    // Socket hardening (§Security: dir 0700 at creation, socket 0600).
    if let Err(e) = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)) {
        return Err(fail_and_reap(
            child,
            "socket hardening",
            format!("chmod 0600 {} failed: {e}", sock.display()),
        ));
    }

    // Version handshake (rung 4, mutating policy).
    match gate_version_drift(
        &version,
        DriftPolicy::Mutating {
            accept_version_drift: opts.accept_version_drift,
        },
    ) {
        Ok(None) => {}
        Ok(Some(warning)) => eprintln!("{warning}"),
        Err(e) => {
            return Err(fail_and_reap(
                child,
                "version handshake",
                format!("{e}\n  (the just-started drifted target was stopped again)"),
            ));
        }
    }

    // load_config if the SPDK source of truth exists (§6.4 persistence
    // law: `target start` / the systemd unit run load_config after RPC
    // liveness).
    let tgt_config = paths.tgt_config();
    if tgt_config.exists() {
        let parsed: Result<Value, _> = fs::read(&tgt_config)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
        let config = match parsed {
            Ok(v) => v,
            Err(e) => {
                return Err(fail_and_reap(
                    child,
                    "load_config",
                    format!(
                        "{} is unreadable ({e}) — fix or move the config aside, then start \
                         again",
                        tgt_config.display()
                    ),
                ));
            }
        };
        match load_config(&client, &config) {
            Ok(report) => {
                println!(
                    "target start: load_config applied {} method(s) from {}",
                    report.applied,
                    tgt_config.display()
                );
                for method in &report.skipped {
                    println!(
                        "target start: load_config SKIPPED '{method}' — not callable in the \
                         current RPC state (STARTUP-only entry on a runtime target)"
                    );
                }
            }
            Err(e) => {
                return Err(fail_and_reap(
                    child,
                    "load_config",
                    format!(
                        "{e}\n  the target was stopped again (a half-loaded config must not \
                         serve); fix {} or move it aside, then start again",
                        tgt_config.display()
                    ),
                ));
            }
        }
    }

    // Pidfile LAST (§6.5 step order) — the process now runs detached.
    fs::write(paths.pidfile(), format!("{pid}\n")).map_err(NvmeofError::Io)?;
    println!(
        "SPDK target started: pid {pid} ({})\n  rpc:     {} (0600)\n  mask:    {mask}  \
         dpdk-mem: {} MiB\n  version: {}\n  log:     {}\n  pidfile: {}",
        bin.display(),
        sock.display(),
        opts.dpdk_mem_mb,
        version.raw,
        paths.tgt_log().display(),
        paths.pidfile().display()
    );
    Ok(())
}

/// Live-consumer scan for `target stop`: ledgered SPDK shares with live
/// controller associations refuse the stop (unless `--force`); foreign
/// live subsystems with consumers are warned about but never block.
fn live_consumers(
    client: &SpdkRpcClient,
    ledgered: &HashSet<String>,
) -> Result<(Vec<(String, usize)>, Vec<(String, usize)>), NvmeofError> {
    let subsystems = client
        .call("nvmf_get_subsystems", None)
        .map_err(rpc_err)?
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut ours = Vec::new();
    let mut foreign = Vec::new();
    for sub in subsystems {
        let nqn = sub
            .get("nqn")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if sub.get("subtype").and_then(Value::as_str) == Some("Discovery") {
            continue;
        }
        let controllers = client
            .call(
                "nvmf_subsystem_get_controllers",
                Some(json!({ "nqn": nqn })),
            )
            .map_err(rpc_err)?
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        if controllers == 0 {
            continue;
        }
        if ledgered.contains(&nqn) {
            ours.push((nqn, controllers));
        } else {
            foreign.push((nqn, controllers));
        }
    }
    Ok((ours, foreign))
}

/// `target stop` (§6.5): refuse-with-live-consumers (unless `force`) →
/// drift warn → best-effort `save_config` → SIGTERM → grace → SIGKILL →
/// pidfile/socket cleanup.
pub fn stop(paths: &SpdkPaths, force: bool) -> Result<(), NvmeofError> {
    let pidfile = paths.pidfile();
    let sock = paths.rpc_sock();
    let pid = match read_pidfile_state(&pidfile).map_err(NvmeofError::Io)? {
        PidfileState::Running(pid) => pid,
        PidfileState::Stale(pid) => {
            println!(
                "target stop: pidfile {} named dead pid {pid} — cleaning the leftovers; \
                 nothing to stop.",
                pidfile.display()
            );
            fs::remove_file(&pidfile).map_err(NvmeofError::Io)?;
            if sock.exists() && poll_client(&sock).version().is_err() {
                let _ = fs::remove_file(&sock);
            }
            return Ok(());
        }
        PidfileState::NotRunning => {
            if let Some(pid) = find_target_pid_by_socket(&sock) {
                return Err(NvmeofError::Refused(format!(
                    "target stop: an SPDK target serves {} (pid {pid}) but was not started \
                     by 'target start' (no pidfile at {}) — refusing to signal a process \
                     this verb does not own.\n  systemd-managed?  sudo systemctl stop \
                     <unit>\n  started by hand?  stop it by its own pid",
                    sock.display(),
                    pidfile.display()
                )));
            }
            println!("target stop: not running — nothing to stop.");
            return Ok(());
        }
    };

    // With a live RPC: consumer refusal + drift warn + best-effort save.
    let client = SpdkRpcClient::new(&sock);
    match client.version() {
        Ok(version) => {
            if let Ok(Some(warning)) = gate_version_drift(&version, DriftPolicy::WarnAndProceed) {
                eprintln!("{warning}");
            }
            let ledgered: HashSet<String> = Ledger::new(&paths.state_dir)
                .load()
                .map_err(NvmeofError::Io)?
                .into_iter()
                .filter(|r| r.stack == StackKind::Spdk)
                .map(|r| r.subnqn)
                .collect();
            let (ours, foreign) = live_consumers(&client, &ledgered)?;
            for (nqn, n) in &foreign {
                eprintln!(
                    "warning: unmanaged live subsystem '{nqn}' has {n} connected \
                     controller(s) — stopping the target drops them (reconnect storms run \
                     ~10 min; design R3)"
                );
            }
            if !ours.is_empty() && !force {
                let listing = ours
                    .iter()
                    .map(|(nqn, n)| format!("    {nqn}  ({n} live controller(s))"))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(NvmeofError::Refused(format!(
                    "target stop: refusing — ledgered SPDK shares have live initiator \
                     connections:\n{listing}\n  sequence: unmount consumers → squeezefs \
                     nvmeof disconnect <subnqn> → retry stop\n  or force the stop \
                     (initiators enter reconnect storms until restore): sudo squeezefs \
                     nvmeof target stop --force"
                )));
            }
            match save_config(&client, &paths.tgt_config()) {
                Ok(()) => println!(
                    "target stop: config saved to {} (load_config restores it on the next \
                     start)",
                    paths.tgt_config().display()
                ),
                Err(e) => eprintln!(
                    "warning: best-effort save_config failed ({e}) — proceeding to stop; \
                     the previous saved config (if any) stays authoritative"
                ),
            }
        }
        Err(e) => {
            eprintln!(
                "warning: RPC not answering ({e}) — save_config skipped; proceeding to \
                 signal pid {pid} by pidfile"
            );
        }
    }

    // TERM → grace → KILL.
    kill_pid(pid, libc::SIGTERM);
    let deadline = Instant::now() + STOP_GRACE;
    let mut escalated = false;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            if escalated {
                return Err(NvmeofError::Refused(format!(
                    "target stop: pid {pid} survived SIGTERM + SIGKILL — inspect it by hand"
                )));
            }
            eprintln!(
                "warning: spdk_tgt (pid {pid}) survived the {STOP_GRACE:?} SIGTERM grace — \
                 escalating to SIGKILL"
            );
            kill_pid(pid, libc::SIGKILL);
            escalated = true;
            std::thread::sleep(Duration::from_secs(2));
            if pid_alive(pid) {
                return Err(NvmeofError::Refused(format!(
                    "target stop: pid {pid} survived SIGKILL — inspect it by hand"
                )));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    fs::remove_file(&pidfile).map_err(NvmeofError::Io)?;
    if sock.exists() {
        // spdk_tgt unlinks its socket on clean exit; a SIGKILL leaves it.
        let _ = fs::remove_file(&sock);
    }
    println!(
        "SPDK target stopped (pid {pid}, {}).",
        if escalated { "SIGKILL" } else { "SIGTERM" }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// status (§6.9)
// ---------------------------------------------------------------------------

fn pid_uptime_secs(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized comm; starttime is overall field 22.
    let after = stat.rsplit_once(')')?.1;
    let starttime_ticks: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf on a constant — no memory involved.
    let tick_hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if tick_hz <= 0 {
        return None;
    }
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let started = starttime_ticks as f64 / tick_hz as f64;
    Some((uptime - started).max(0.0) as u64)
}

fn dpdk_mem_from_cmdline(pid: i32) -> Option<u64> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    args.windows(2)
        .find(|pair| pair[0] == "-s")
        .and_then(|pair| pair[1].parse().ok())
}

/// Reactor busy % from `framework_get_reactors` tick deltas — two samples
/// 500 ms apart (§6.9: the "is the poller core actually burning" answer).
fn sample_reactor_busy(client: &SpdkRpcClient) -> Vec<(u64, f64)> {
    let read = |v: &Value| -> Vec<(u64, u64, u64)> {
        v.get("reactors")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some((
                            r.get("lcore").and_then(Value::as_u64)?,
                            r.get("busy").and_then(Value::as_u64)?,
                            r.get("idle").and_then(Value::as_u64)?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let Ok(a) = client.call("framework_get_reactors", None) else {
        return Vec::new();
    };
    std::thread::sleep(Duration::from_millis(500));
    let Ok(b) = client.call("framework_get_reactors", None) else {
        return Vec::new();
    };
    let first = read(&a);
    read(&b)
        .into_iter()
        .filter_map(|(core, busy_b, idle_b)| {
            let (_, busy_a, idle_a) = first.iter().find(|(c, _, _)| *c == core)?;
            let d_busy = busy_b.saturating_sub(*busy_a) as f64;
            let d_idle = idle_b.saturating_sub(*idle_a) as f64;
            let denom = d_busy + d_idle;
            Some((
                core,
                if denom > 0.0 {
                    (d_busy / denom * 100.0 * 10.0).round() / 10.0
                } else {
                    0.0
                },
            ))
        })
        .collect()
}

/// The §6.9 status payload (JSON shape; the human renderer walks it).
/// Gathering never refuses — every unobservable probe renders null/0.
pub fn status_payload(paths: &SpdkPaths) -> Value {
    let mut payload = json!({
        "stack": "spdk",
        "pinned": {"tag": SPDK_PINNED_TAG, "commit": SPDK_PINNED_COMMIT},
        "running": Value::Null,
        "rpc": {"live": false, "version": Value::Null, "drift": Value::Null,
                 "latency_us": Value::Null},
        "reactors": [],
        "hugepages": {"free_2m": Value::Null, "total_2m": Value::Null,
                       "dpdk_mem_mb": Value::Null},
        "subsystems": 0, "namespaces": 0, "listeners": [],
        "ptpl_files": {"present": 0, "missing": 0},
        "ledger": {"managed": 0, "down": 0, "pending": 0, "removing": 0,
                    "foreign_live": 0},
    });

    // Binary posture (diagnostic sugar beyond the §6.9 core fields).
    if let Ok((bin, _)) = preflight_binary(paths) {
        payload["binary"] = json!(bin.display().to_string());
    }

    // Running discovery: pidfile first, socket-serving process second
    // ("systemd" covers units and by-hand strays — anything not ours).
    let sock = paths.rpc_sock();
    let running = match read_pidfile_state(&paths.pidfile()) {
        Ok(PidfileState::Running(pid)) => Some((pid, "pidfile")),
        _ => find_target_pid_by_socket(&sock).map(|pid| (pid, "systemd")),
    };
    if let Some((pid, mode)) = running {
        payload["running"] = json!({
            "pid": pid,
            "mode": mode,
            "uptime_s": pid_uptime_secs(pid),
        });
        if let Some(mb) = dpdk_mem_from_cmdline(pid) {
            payload["hugepages"]["dpdk_mem_mb"] = json!(mb);
        }
    }

    // Hugepage pool.
    if let Ok(snap) = hugepages::read_snapshot(Path::new(hugepages::HUGEPAGES_2M_SYSFS_DIR)) {
        payload["hugepages"]["free_2m"] = json!(snap.free);
        payload["hugepages"]["total_2m"] = json!(snap.total);
    }

    // RPC probe: liveness, latency, version, drift.
    let client = SpdkRpcClient::new(&sock);
    let started = Instant::now();
    let live_nqns: HashSet<String> = match client.version() {
        Ok(version) => {
            let latency_us = started.elapsed().as_micros() as u64;
            payload["rpc"] = json!({
                "live": true,
                "version": version.raw,
                "drift": version.drift().is_some(),
                "latency_us": latency_us,
            });
            payload["reactors"] = json!(sample_reactor_busy(&client)
                .into_iter()
                .map(|(core, busy)| json!({"core": core, "busy_pct": busy}))
                .collect::<Vec<_>>());
            match client.call("nvmf_get_subsystems", None) {
                Ok(subs) => {
                    let subs = subs.as_array().cloned().unwrap_or_default();
                    let mut nqns = HashSet::new();
                    let mut namespaces = 0usize;
                    let mut listeners: Vec<String> = Vec::new();
                    for sub in &subs {
                        if sub.get("subtype").and_then(Value::as_str) == Some("Discovery") {
                            continue;
                        }
                        if let Some(nqn) = sub.get("nqn").and_then(Value::as_str) {
                            nqns.insert(nqn.to_string());
                        }
                        namespaces += sub
                            .get("namespaces")
                            .and_then(Value::as_array)
                            .map(Vec::len)
                            .unwrap_or(0);
                        for l in sub
                            .get("listen_addresses")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                        {
                            let addr = format!(
                                "{}:{}",
                                l.get("traddr").and_then(Value::as_str).unwrap_or("?"),
                                l.get("trsvcid").and_then(Value::as_str).unwrap_or("?")
                            );
                            if !listeners.contains(&addr) {
                                listeners.push(addr);
                            }
                        }
                    }
                    payload["subsystems"] = json!(nqns.len());
                    payload["namespaces"] = json!(namespaces);
                    payload["listeners"] = json!(listeners);
                    nqns
                }
                Err(_) => HashSet::new(),
            }
        }
        Err(e) => {
            payload["rpc"]["detail"] = json!(e.to_string());
            HashSet::new()
        }
    };

    // Ledger reconciliation (SPDK-stack records only) + ptpl inventory.
    match Ledger::new(&paths.state_dir).load() {
        Ok(records) => {
            let (mut managed, mut down, mut pending, mut removing) = (0u64, 0u64, 0u64, 0u64);
            let (mut ptpl_present, mut ptpl_missing) = (0u64, 0u64);
            let mut ledgered = HashSet::new();
            for rec in records.iter().filter(|r| r.stack == StackKind::Spdk) {
                ledgered.insert(rec.subnqn.clone());
                match rec.state {
                    ShareState::Pending => pending += 1,
                    ShareState::Removing => removing += 1,
                    ShareState::Active => {
                        if live_nqns.contains(&rec.subnqn) {
                            managed += 1;
                        } else {
                            down += 1;
                        }
                    }
                }
                if let Some(rel) = &rec.ptpl_file {
                    if paths.state_dir.join(rel).exists() {
                        ptpl_present += 1;
                    } else {
                        ptpl_missing += 1;
                    }
                }
            }
            let foreign_live = live_nqns.iter().filter(|n| !ledgered.contains(*n)).count();
            payload["ledger"] = json!({
                "managed": managed, "down": down, "pending": pending,
                "removing": removing, "foreign_live": foreign_live,
            });
            payload["ptpl_files"] = json!({"present": ptpl_present, "missing": ptpl_missing});
        }
        Err(e) => {
            payload["ledger"] = json!({"error": e.to_string()});
        }
    }
    payload
}

/// `target status` (§6.9): never refuses — reports what it can observe.
pub fn status(paths: &SpdkPaths, json_mode: bool) -> Result<(), NvmeofError> {
    let payload = status_payload(paths);
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| NvmeofError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?
        );
        return Ok(());
    }
    println!("=== SPDK NVMe-oF Target Status ===");
    println!(
        "  pinned:    {} ({})",
        payload["pinned"]["tag"].as_str().unwrap_or("?"),
        payload["pinned"]["commit"].as_str().unwrap_or("?")
    );
    if let Some(bin) = payload.get("binary").and_then(Value::as_str) {
        println!("  binary:    {bin}");
    } else {
        println!("  binary:    NOT INSTALLED — sudo squeezefs nvmeof target install");
    }
    match payload["running"].as_object() {
        Some(run) => println!(
            "  running:   pid {} ({} mode, up {} s)",
            run["pid"],
            run["mode"].as_str().unwrap_or("?"),
            run["uptime_s"]
        ),
        None => println!("  running:   no — sudo squeezefs nvmeof target start"),
    }
    let rpc = &payload["rpc"];
    if rpc["live"].as_bool() == Some(true) {
        println!(
            "  rpc:       live ({} µs) — {}{}",
            rpc["latency_us"],
            rpc["version"].as_str().unwrap_or("?"),
            if rpc["drift"].as_bool() == Some(true) {
                "  [VERSION DRIFT vs pin]"
            } else {
                ""
            }
        );
    } else {
        println!(
            "  rpc:       DEAD{}",
            rpc.get("detail")
                .and_then(Value::as_str)
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        );
    }
    for r in payload["reactors"].as_array().into_iter().flatten() {
        println!("  reactor:   core {} busy {} %", r["core"], r["busy_pct"]);
    }
    println!(
        "  hugepages: free {} / total {} × 2 MiB (dpdk -s: {} MiB)",
        payload["hugepages"]["free_2m"],
        payload["hugepages"]["total_2m"],
        payload["hugepages"]["dpdk_mem_mb"]
    );
    println!(
        "  serving:   {} subsystem(s), {} namespace(s), listeners: {}",
        payload["subsystems"],
        payload["namespaces"],
        payload["listeners"]
            .as_array()
            .map(|a| a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "  ptpl:      {} present, {} missing{}",
        payload["ptpl_files"]["present"],
        payload["ptpl_files"]["missing"],
        if payload["ptpl_files"]["missing"].as_u64().unwrap_or(0) > 0 {
            "  [PTPL-regression pre-alarm]"
        } else {
            ""
        }
    );
    match payload["ledger"].get("error") {
        Some(e) => println!("  ledger:    UNREADABLE — {e}"),
        None => println!(
            "  ledger:    managed {}, down {}, pending {}, removing {}, foreign live {}",
            payload["ledger"]["managed"],
            payload["ledger"]["down"],
            payload["ledger"]["pending"],
            payload["ledger"]["removing"],
            payload["ledger"]["foreign_live"]
        ),
    }
    Ok(())
}

/// `target systemd-unit` (SPDK arm): resolve + bake values, return the
/// unit text (the CLI prints it to stdout, never installs it). The
/// binary must exist — a unit whose ExecStart cannot run is refused,
/// not emitted; override warnings go to stderr so stdout stays a valid
/// unit.
pub fn systemd_unit(
    paths: &SpdkPaths,
    core_mask: Option<String>,
    cores: Option<u32>,
    dpdk_mem_mb: u64,
) -> Result<String, NvmeofError> {
    let (bin, warning) = preflight_binary(paths)?;
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    let mask = resolve_core_mask(core_mask, cores)?;
    let exe = std::env::current_exe().map_err(NvmeofError::Io)?;
    Ok(render_spdk_unit(
        &bin,
        &paths.rpc_sock(),
        &mask,
        dpdk_mem_mb,
        &exe,
    ))
}
