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

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::stack::{NvmeofError, PreflightError};
use super::rpc::{SpdkRpcClient, SpdkVersion};
use super::SpdkPaths;

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
    let _ = pidfile;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `kill(pid, 0)` liveness.
pub fn pid_alive(pid: i32) -> bool {
    let _ = pid;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

// ---------------------------------------------------------------------------
// reactor core-mask math (§6.5 reactor/core budget)
// ---------------------------------------------------------------------------

/// Parse a kernel cpu-list string (`"0-31"`, `"0-3,8,10-11"`).
pub fn parse_cpu_list(s: &str) -> Result<Vec<u32>, String> {
    let _ = s;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// Hex mask (`"0x…"`) selecting exactly `cores` (arbitrary width — cpu
/// ids beyond 63 render as long hex strings, which SPDK `-m` accepts).
pub fn mask_for_cores(cores: &[u32]) -> Result<String, String> {
    let _ = cores;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// The §6.5 default: one reactor on the **highest-numbered online CPU**
/// (keeps CPU0/IRQ locality alone; scoping used core 24).
pub fn default_core_mask(online: &[u32]) -> Result<String, String> {
    let _ = online;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `--cores N`: N cores from the top down.
pub fn top_cores_mask(online: &[u32], n: u32) -> Result<String, String> {
    let _ = (online, n);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// Online cpu ids from `/sys/devices/system/cpu/online`.
pub fn online_cpus() -> io::Result<Vec<u32>> {
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

// ---------------------------------------------------------------------------
// preflight ladder rungs (§6.2/§6.3 — the G3 loud-fail matrix)
// ---------------------------------------------------------------------------

/// Rung 1: the pinned binary is present, or `SQUEEZEFS_SPDK_TGT_BIN`
/// substitutes it (returned warning = the loud unpinned line). Missing
/// binary names `target install`; a set-but-missing override names the
/// env var — never a silent fallthrough to the pin.
pub fn preflight_binary(paths: &SpdkPaths) -> Result<(PathBuf, Option<String>), PreflightError> {
    let _ = paths;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// Rung 3: the RPC socket answers `spdk_get_version` within the connect
/// timeout. The failure text is the §6.2 designed dead-RPC runbook
/// (check/start verbs, the explicit-nvmet choice, the never-falls-back
/// law).
pub fn preflight_rpc(paths: &SpdkPaths) -> Result<SpdkVersion, PreflightError> {
    let _ = paths;
    unimplemented!("N3 skeleton — implemented by the feat commit")
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

/// Rung 4: reported version vs the pin. `Ok(None)` = no drift;
/// `Ok(Some(line))` = drift allowed under the policy (the loud warning
/// to print); `Err` = the G3 wrong-version refusal.
pub fn gate_version_drift(
    version: &SpdkVersion,
    policy: DriftPolicy,
) -> Result<Option<String>, PreflightError> {
    let _ = (version, policy);
    unimplemented!("N3 skeleton — implemented by the feat commit")
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
    /// Distro package hint printed by the refusal (`debian: … / fedora: …`).
    pub package_hint: &'static str,
}

/// The probed toolchain (binaries + headers the §6.5 configure needs).
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

/// Probe the toolchain; returns the missing deps (empty = buildable).
pub fn probe_missing_toolchain() -> Vec<ToolDep> {
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// The §6.5 consent refusal: lists every missing dep with its package
/// hint, states that the default mutates nothing, and names
/// `--with-pkgdep` as the explicit opt-in.
pub fn toolchain_refusal_message(missing: &[ToolDep]) -> String {
    let _ = missing;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `--version` must equal the pin (pin bumps are deliberate PRs — R1).
/// Pure grammar validation: runs before the root check.
pub fn validate_install_version(version: Option<&str>) -> Result<(), NvmeofError> {
    let _ = version;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

// ---------------------------------------------------------------------------
// systemd-unit emission (§6.5 — values BAKED, emitted never installed)
// ---------------------------------------------------------------------------

/// Render the SPDK unit with every value baked at emission time (no
/// `${VAR}` indirection — systemd expands unset variables to empty and
/// would silently render a malformed spdk_tgt command line).
pub fn render_spdk_unit(
    spdk_tgt_bin: &Path,
    rpc_sock: &Path,
    core_mask: &str,
    dpdk_mem_mb: u64,
    squeezefs_exe: &Path,
) -> String {
    let _ = (
        spdk_tgt_bin,
        rpc_sock,
        core_mask,
        dpdk_mem_mb,
        squeezefs_exe,
    );
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// The nvmet variant (§6.6): configfs is empty at boot by nature —
/// `Type=oneshot` + `RemainAfterExit=yes`, ExecStart = the product's
/// `restore --target-stack nvmet`.
pub fn render_nvmet_unit(squeezefs_exe: &Path) -> String {
    let _ = squeezefs_exe;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

// ---------------------------------------------------------------------------
// save_config / load_config composition (§6.4 SPDK persistence law)
// ---------------------------------------------------------------------------

/// Gather the target's running config (`framework_get_subsystems` +
/// per-subsystem `framework_get_config` — the rpc.py `save_config`
/// composition) and write it atomically to `tgt_config` (tmp → fsync →
/// rename → fsync dir, the ledger law-2 discipline).
pub fn save_config(client: &SpdkRpcClient, tgt_config: &Path) -> Result<(), NvmeofError> {
    let _ = (client, tgt_config);
    unimplemented!("N3 skeleton — implemented by the feat commit")
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

/// Replay a saved config into the running target (the rpc.py
/// `load_config` algorithm: `rpc_get_methods` gate, pass loop,
/// `framework_start_init` handling), collecting a loud report.
pub fn load_config(
    client: &SpdkRpcClient,
    config: &serde_json::Value,
) -> Result<LoadConfigReport, NvmeofError> {
    let _ = (client, config);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

// ---------------------------------------------------------------------------
// the verbs
// ---------------------------------------------------------------------------

/// `target start` options (§6.2 grammar).
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub core_mask: Option<String>,
    pub cores: Option<u32>,
    pub dpdk_mem_mb: u64,
    pub accept_version_drift: bool,
}

/// `target install` (§6.5): pinned clone + sha verify + build into the
/// prefix; `--with-pkgdep` consent; build-info provenance; idempotent on
/// a verified complete install; refuses dirty/unverified checkouts loud.
pub fn install(
    paths: &SpdkPaths,
    version: Option<&str>,
    with_pkgdep: bool,
) -> Result<(), NvmeofError> {
    let _ = (paths, version, with_pkgdep);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `target start` (§6.5 pidfile direct mode): preflight → spawn →
/// RPC-liveness poll → version handshake → `load_config` if the config
/// exists → write pidfile. Every step fails loud with the step named.
pub fn start(paths: &SpdkPaths, opts: &StartOptions) -> Result<(), NvmeofError> {
    let _ = (paths, opts);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `target stop` (§6.5): refuse-with-live-consumers (unless `force`) →
/// drift warn → `save_config` → SIGTERM → grace → SIGKILL → pidfile/socket
/// cleanup.
pub fn stop(paths: &SpdkPaths, force: bool) -> Result<(), NvmeofError> {
    let _ = (paths, force);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `target status` (§6.9): never refuses — reports what it can observe
/// (running/pid/mode/uptime, rpc liveness + version + drift + latency,
/// reactor busy, hugepages, subsystem counts, ptpl files, ledger
/// reconciliation).
pub fn status(paths: &SpdkPaths, json: bool) -> Result<(), NvmeofError> {
    let _ = (paths, json);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// The §6.9 status payload (JSON shape; the human renderer walks it).
pub fn status_payload(paths: &SpdkPaths) -> serde_json::Value {
    let _ = paths;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `target systemd-unit` (SPDK arm): resolve + bake values, return the
/// unit text (the CLI prints it to stdout, never installs it).
pub fn systemd_unit(
    paths: &SpdkPaths,
    core_mask: Option<String>,
    cores: Option<u32>,
    dpdk_mem_mb: u64,
) -> Result<String, NvmeofError> {
    let _ = (paths, core_mask, cores, dpdk_mem_mb);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}
