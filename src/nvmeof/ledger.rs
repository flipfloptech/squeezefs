//! The share ledger — versioned, forward-only record store for NVMe-oF
//! target shares (`docs/design-nvmeof-target-management.md` §6.4).
//!
//! Replaces the self-truncating `/etc/squeezefs/nvmeof_shares.json`
//! registry (nvmeof.rs:1019–1033 pre-split: the root writability probe
//! `fs::write(&path, "[]")` ran on **every** path resolution, so
//! `load_shares()` destroyed the file before reading it — share
//! persistence never worked in production). The ledger exists for
//! exactly the jobs the SPDK-native `save_config` cannot do: nvmet
//! restore, `unshare` stack dispatch, the cross-stack duplicate-backing
//! guard, `list` ownership metadata, and the crash-window intent
//! records.
//!
//! Laws (§6.4, each pinned by a test in `tests/nvmeof_ledger_tests.rs`):
//!
//! 1. **Load never writes.** `load()` is a pure read: no lock-file
//!    creation, no directory creation, no tmp-file cleanup — it succeeds
//!    on a read-only filesystem and a byte-identical file survives any
//!    number of loads.
//! 2. **Writes are atomic and serialized**: read-modify-write under
//!    `flock` on `shares.json.lock`; write `shares.json.tmp` → `fsync`
//!    → `rename` → `fsync` parent dir. (Control plane: std fs is
//!    sanctioned here per the `reservation.rs` precedent — no uring
//!    requirement.)
//! 3. **Forward-only**: `"format" > 1` refuses loud ("created by a newer
//!    squeezefs; upgrade"). Unknown fields within v1 are an error, not
//!    ignored; absent optional fields are legal per the field-presence
//!    rules (`super::stack`).
//! 4. **Reconciliation, never blind trust** — enforced by the verbs; the
//!    ledger provides the intent states they reconcile.
//! 5. **Bookkeeping lives here, never in configfs** — `loop_device` (and,
//!    from N2, nvmet port ids) are ledger fields. N1 transitional: the
//!    ledger owns `loop_device` from N1 (unshare's loop detach reads the
//!    ledger, not configfs); the old share path's configfs fake-file
//!    *write* survives until N2 deletes that path — nothing reads it.
//! 6. **Write-ahead intent**: `begin_share` (pending) → `finalize_share`
//!    (active) / `mark_removing` → `delete`. Record-before-mutate on both
//!    verbs; mutate-first/record-last ordering is forbidden.

use std::io;
use std::path::{Path, PathBuf};

use super::stack::ShareRecord;

/// Current (and only) on-disk ledger format version.
pub const LEDGER_FORMAT: u64 = 1;
/// Ledger file name under the state dir.
pub const LEDGER_FILE: &str = "shares.json";
/// Serialization lock (law 2). Never taken by `load()` (law 1).
pub const LEDGER_LOCK_FILE: &str = "shares.json.lock";
/// Atomic-replace staging name (law 2). A leftover tmp is the crash
/// shape: `load()` ignores it and reports; the next mutation replaces it.
pub const LEDGER_TMP_FILE: &str = "shares.json.tmp";
/// Production state dir (§6.4 state-home table).
pub const DEFAULT_STATE_DIR: &str = "/var/lib/squeezefs/nvmeof";
/// Relocation seam for tests (§6.8: env overrides that *relocate* real
/// behavior are sanctioned; env forks of behavior are banned).
pub const STATE_DIR_ENV: &str = "SQUEEZEFS_NVMEOF_STATE_DIR";
/// The retired pre-rebuild registry (§6.4 old-registry migration).
pub const OLD_REGISTRY_PATH: &str = "/etc/squeezefs/nvmeof_shares.json";

/// Handle to a share ledger rooted at one state directory.
pub struct Ledger {
    state_dir: PathBuf,
}

impl Ledger {
    /// Ledger rooted at an explicit state directory (tests, tools).
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Ledger {
            state_dir: state_dir.into(),
        }
    }

    /// Production constructor: `SQUEEZEFS_NVMEOF_STATE_DIR` (relocation
    /// seam) or `/var/lib/squeezefs/nvmeof`.
    pub fn open_default() -> Self {
        match std::env::var(STATE_DIR_ENV) {
            Ok(dir) if !dir.is_empty() => Ledger::new(dir),
            _ => Ledger::new(DEFAULT_STATE_DIR),
        }
    }

    /// The ledger file path (for assertions/diagnostics).
    pub fn ledger_path(&self) -> PathBuf {
        self.state_dir.join(LEDGER_FILE)
    }

    /// Law 1: pure read — never creates, writes, locks, or deletes
    /// anything (a missing file/dir is an empty ledger; a leftover
    /// `shares.json.tmp` is ignored and reported, never consumed).
    pub fn load(&self) -> io::Result<Vec<ShareRecord>> {
        unimplemented!(
            "N1 ledger implementation lands in the implementation commit (state dir {})",
            self.state_dir.display()
        )
    }

    /// Convenience lookup by subsystem NQN (any intent state — a
    /// `pending`/`removing` crash-window share is still ours).
    pub fn find(&self, _subnqn: &str) -> io::Result<Option<ShareRecord>> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }

    /// Law 6 intent begin: append the record with `state: pending`
    /// **before the first stack mutation**. Refuses loud (AlreadyExists)
    /// on a duplicate `subnqn` or a duplicate `backing_canonical` — the
    /// ledger half of the cross-stack duplicate-backing guard; the
    /// refusal names the holder.
    pub fn begin_share(&self, _record: &ShareRecord) -> io::Result<()> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }

    /// Law 6 intent finalize: `pending` → `active`, only after the last
    /// stack mutation succeeded. Any other current state is an error.
    pub fn finalize_share(&self, _subnqn: &str) -> io::Result<()> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }

    /// Law 6 teardown intent: flip to `removing` **before the first
    /// teardown write**. Idempotent on an already-`removing` record
    /// (resumed teardown); NotFound if the record does not exist.
    pub fn mark_removing(&self, _subnqn: &str) -> io::Result<()> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }

    /// Law 6 completion: remove the record after teardown completed.
    pub fn delete(&self, _subnqn: &str) -> io::Result<()> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }

    /// Law 5: the loop-device association is ledger bookkeeping (never
    /// configfs). Recorded mid-share (the device is only known after
    /// `losetup`) and refreshed on restore replay.
    pub fn set_loop_device(&self, _subnqn: &str, _loop_device: Option<String>) -> io::Result<()> {
        unimplemented!("N1 ledger implementation lands in the implementation commit")
    }
}

/// §6.4 old-registry migration: the first mutating verb of the new
/// binary renames a pre-existing pre-rebuild registry file to
/// `<path>.retired-by-rebuild` with one loud line. Never reads it (as
/// root the old registry was never readable back — truncated before
/// every read); whatever bytes it holds are preserved by the rename.
/// Failures are logged loud and never fail the verb.
pub fn retire_old_registry(path: &Path) {
    let _ = path;
    unimplemented!("N1 ledger implementation lands in the implementation commit")
}

/// RFC 3339 UTC timestamp for `created_utc` (no chrono dependency —
/// civil-from-days conversion, unit-tested against known vectors).
pub fn utc_now_rfc3339() -> String {
    unimplemented!("N1 ledger implementation lands in the implementation commit")
}
