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

use super::ledger::Ledger;
use super::stack::{
    LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreReport, ShareRecord, ShareRequest,
    TargetStack, TargetStatus,
};
use super::StackKind;

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

// ---------------------------------------------------------------------------
// SpdkStack — the SPDK share path behind the TargetStack trait (§6.5, N4)
// ---------------------------------------------------------------------------

/// The SPDK target stack (§6.1/§6.5, PR 4/N4): `save_config`-backed
/// share/unshare/restore over the RPC client v2, every share pinning
/// `nsid` + ns UUID + `ptpl_file` (the scoping §4 pt 1 fix), riding the
/// shared ledger intent protocol (§6.4 law 6) and the live-state
/// duplicate guard (`bdev_get_bdevs` filename scan — the one duplicate
/// check the old module got right, kept).
pub struct SpdkStack {
    paths: SpdkPaths,
    ledger: Ledger,
    /// §6.2 flag placement: gates preflight rung 4 on the mutating verbs.
    accept_version_drift: bool,
    /// `unshare --force`: overrides the live-consumer refusal (design R7
    /// posture, mirrors `target stop`).
    unshare_force: bool,
}

impl SpdkStack {
    /// Stack rooted at explicit paths + ledger (the sanctioned unit-tier
    /// injection seam: every code path executed is the production path
    /// pointed at a different location — §6.8).
    pub fn new(
        paths: SpdkPaths,
        ledger: Ledger,
        accept_version_drift: bool,
        unshare_force: bool,
    ) -> Self {
        SpdkStack {
            paths,
            ledger,
            accept_version_drift,
            unshare_force,
        }
    }

    /// Production constructor (env-aware paths, default ledger).
    pub fn open_default(accept_version_drift: bool, unshare_force: bool) -> Self {
        SpdkStack::new(
            SpdkPaths::resolve(),
            Ledger::open_default(),
            accept_version_drift,
            unshare_force,
        )
    }

    /// Tolerant live-state gather for `list` and the cross-stack
    /// duplicate guard's nvmet-share direction: a dead/unreachable
    /// target serves nothing (SPDK state is process-resident), so the
    /// walk degrades to empty **with the loud note returned** — never a
    /// silent hole, never a hard failure on a box that simply does not
    /// run SPDK.
    pub fn live_shares_tolerant(&self) -> (Vec<LiveShare>, Option<String>) {
        let _ = (
            &self.paths,
            &self.ledger,
            self.accept_version_drift,
            self.unshare_force,
        );
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }
}

impl TargetStack for SpdkStack {
    fn kind(&self) -> StackKind {
        StackKind::Spdk
    }

    fn preflight(&self, op: PreflightOp) -> Result<(), PreflightError> {
        let _ = op;
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }

    fn share(&self, req: &ShareRequest) -> Result<ShareRecord, NvmeofError> {
        let _ = req;
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }

    fn unshare(&self, rec: &ShareRecord) -> Result<(), NvmeofError> {
        let _ = rec;
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }

    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }

    fn restore(&self, recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError> {
        let _ = recs;
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }

    fn target_status(&self) -> Result<TargetStatus, NvmeofError> {
        unimplemented!("N4 skeleton — implemented by the feat commit")
    }
}
