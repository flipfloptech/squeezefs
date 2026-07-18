//! SPDK target-stack management (`docs/design-nvmeof-target-management.md`
//! §6.5/§6.9, PR 3/N3): pinned-release lifecycle (`target
//! install/setup/start/stop/status/systemd-unit`) and the JSON-RPC
//! client v2. The SPDK **share path** (`SpdkStack` behind the
//! `TargetStack` trait, `save_config`-backed sharing with pinned
//! nsid/UUID/PTPL) lands at N4 — until then SPDK-selected share verbs
//! keep failing loud in `super::stack_for` with the milestone message.
//!
//! Module-graph law (program gate G3): this subtree carries **no**
//! `use`/path reference to `super::nvmet` items and vice versa — stack
//! dispatch lives in `super` (`src/nvmeof/mod.rs`) only, so zero
//! cross-stack fallback paths can exist by construction (pinned by
//! `tests/nvmeof_target_lifecycle_tests.rs`).
//!
//! io_uring note: everything here is one-shot admin control plane
//! (unix-socket JSON-RPC, sysfs writes, git/make shell-outs) — the
//! sanctioned `reservation.rs` precedent; no data path is touched.

pub mod hugepages;
pub mod lifecycle;
pub mod rpc;

use std::path::PathBuf;

/// Env override substituting the pinned `spdk_tgt` binary for the six
/// target verbs' preflights and `target start` (rig/dev seam — always
/// with the loud unpinned warning; `target install` ignores it: install
/// always builds the pin). §6.2 flag/override placement.
pub const SPDK_TGT_BIN_ENV: &str = "SQUEEZEFS_SPDK_TGT_BIN";

/// Env relocating the runtime dir (§6.4 state homes; a §6.8 relocation
/// seam — the production path pointed at a different location).
pub const RUN_DIR_ENV: &str = "SQUEEZEFS_NVMEOF_RUN_DIR";

/// Production runtime dir: RPC socket + pidfile (tmpfs, reboot-cleared).
/// Never `/var/tmp/spdk.sock` — a world-writable-dir default is a
/// local-root-equivalent control surface (§Security).
pub const DEFAULT_RUN_DIR: &str = "/run/squeezefs/nvmeof";

/// Pinned SPDK builds live under `<root>/<tag>/` — never `/opt/spdk`,
/// never system-wide (§6.4).
pub const DEFAULT_INSTALL_ROOT: &str = "/opt/squeezefs/spdk";

/// RPC socket file name under the run dir (socket 0600, dir 0700).
pub const RPC_SOCK_FILENAME: &str = "spdk.sock";
/// Pidfile name under the run dir (pidfile direct mode, §6.5).
pub const PIDFILE_FILENAME: &str = "spdk_tgt.pid";
/// spdk_tgt output capture under the run dir (pidfile mode) — never
/// `Stdio::null()` (§6.9 Logging).
pub const TGT_LOG_FILENAME: &str = "spdk_tgt.log";

/// The resolved path set every SPDK verb operates on (§6.4 state homes).
/// `resolve()` is the production constructor (env-aware for the
/// sanctioned relocation seams); `with_roots` is the unit-tier injection
/// seam — every code path executed is the production path pointed at a
/// different location (§6.8; zero env behavior forks).
#[derive(Debug, Clone)]
pub struct SpdkPaths {
    /// `/opt/squeezefs/spdk/<tag>` — the pinned build prefix.
    pub install_prefix: PathBuf,
    /// `/run/squeezefs/nvmeof` (or `SQUEEZEFS_NVMEOF_RUN_DIR`).
    pub run_dir: PathBuf,
    /// `/var/lib/squeezefs/nvmeof` (or `SQUEEZEFS_NVMEOF_STATE_DIR`) —
    /// shared with the ledger; SPDK-native state lives under `spdk/`.
    pub state_dir: PathBuf,
    /// `SQUEEZEFS_SPDK_TGT_BIN` (loud unpinned warning when used).
    pub bin_override: Option<PathBuf>,
}

impl SpdkPaths {
    /// Production constructor (env-aware).
    pub fn resolve() -> Self {
        let env_dir = |key: &str, default: &str| -> PathBuf {
            match std::env::var(key) {
                Ok(v) if !v.is_empty() => PathBuf::from(v),
                _ => PathBuf::from(default),
            }
        };
        SpdkPaths {
            install_prefix: PathBuf::from(DEFAULT_INSTALL_ROOT).join(lifecycle::SPDK_PINNED_TAG),
            run_dir: env_dir(RUN_DIR_ENV, DEFAULT_RUN_DIR),
            state_dir: env_dir(
                super::ledger::STATE_DIR_ENV,
                super::ledger::DEFAULT_STATE_DIR,
            ),
            bin_override: match std::env::var(SPDK_TGT_BIN_ENV) {
                Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
                _ => None,
            },
        }
    }

    /// Injection seam for the unit tier.
    pub fn with_roots(
        install_prefix: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        SpdkPaths {
            install_prefix: install_prefix.into(),
            run_dir: run_dir.into(),
            state_dir: state_dir.into(),
            bin_override: None,
        }
    }

    /// The pinned `spdk_tgt` binary: `<prefix>/build/bin/spdk_tgt`
    /// (`install` links `<prefix>/build` → `<prefix>/src/build`, so the
    /// §6.5 systemd-unit path holds verbatim).
    pub fn pinned_bin(&self) -> PathBuf {
        self.install_prefix.join("build/bin/spdk_tgt")
    }

    /// The pinned source checkout: `<prefix>/src`.
    pub fn src_dir(&self) -> PathBuf {
        self.install_prefix.join("src")
    }

    /// Build provenance record (§6.5 — the scoping `build-info.txt` shape).
    pub fn build_info(&self) -> PathBuf {
        self.install_prefix.join("build-info.txt")
    }

    /// Full install/build log.
    pub fn build_log(&self) -> PathBuf {
        self.install_prefix.join("build.log")
    }

    pub fn rpc_sock(&self) -> PathBuf {
        self.run_dir.join(RPC_SOCK_FILENAME)
    }

    pub fn pidfile(&self) -> PathBuf {
        self.run_dir.join(PIDFILE_FILENAME)
    }

    pub fn tgt_log(&self) -> PathBuf {
        self.run_dir.join(TGT_LOG_FILENAME)
    }

    /// SPDK-native state home under the shared state dir (§6.4):
    /// `tgt-config.json`, `ptpl/*.json`, the hugepage prior record.
    pub fn spdk_state_dir(&self) -> PathBuf {
        self.state_dir.join("spdk")
    }

    /// The SPDK source of truth (`save_config` output; §6.4).
    pub fn tgt_config(&self) -> PathBuf {
        self.spdk_state_dir().join("tgt-config.json")
    }

    /// Reservation-persistence files (`ptpl_file`s), populated from N4.
    pub fn ptpl_dir(&self) -> PathBuf {
        self.spdk_state_dir().join("ptpl")
    }
}
