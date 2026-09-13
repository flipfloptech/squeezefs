//! The share ledger — versioned, forward-only record store for NVMe-oF
//! target shares (`docs/design-nvmeof-target-management.md` §6.4).
//!
//! Replaces the self-truncating `/etc/squeezefs/nvmeof_shares.json`
//! registry (nvmeof.rs:1019–1033 pre-split: the root writability probe
//! `fs::write(&path, "[]")` ran on **every** path resolution, so
//! `load_shares()` destroyed the file before reading it — share
//! persistence never worked in production). The ledger owns nvmet
//! restore, `unshare` stack dispatch, the duplicate-backing guard's
//! ledger half, `list` ownership metadata, and the crash-window intent
//! records. Records the RETIRED SPDK stack wrote (R-SYM-8) stay
//! decodable — listed with the re-share sequence, never re-presented,
//! removed ledger-only by `unshare`.
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

use std::fs;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::stack::{ShareRecord, ShareState};

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

/// On-disk shape: versioned header + records (§6.4 schema). Unknown
/// fields refuse (law 3) — but only after the format-version gate, so a
/// future format's unknown fields produce the *upgrade* message, not a
/// parse error.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerFile {
    format: u64,
    shares: Vec<ShareRecord>,
}

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

    /// The state directory this ledger is rooted at (§6.4 state homes) —
    /// where the retired SPDK stack's residue is named from.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir.join(LEDGER_LOCK_FILE)
    }

    fn tmp_path(&self) -> PathBuf {
        self.state_dir.join(LEDGER_TMP_FILE)
    }

    /// Law 1: pure read — never creates, writes, locks, or deletes
    /// anything (a missing file/dir is an empty ledger; a leftover
    /// `shares.json.tmp` is ignored and reported, never consumed).
    pub fn load(&self) -> io::Result<Vec<ShareRecord>> {
        if self.tmp_path().exists() {
            log::warn!(
                "share ledger: leftover atomic-replace tmp file {} (crash between write and \
                 rename) — ignored; the durable ledger is authoritative and the next mutation \
                 replaces it",
                self.tmp_path().display()
            );
        }
        let bytes = match fs::read(self.ledger_path()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        parse_ledger(&bytes, &self.ledger_path())
    }

    /// Convenience lookup by subsystem NQN (any intent state — a
    /// `pending`/`removing` crash-window share is still ours).
    pub fn find(&self, subnqn: &str) -> io::Result<Option<ShareRecord>> {
        Ok(self.load()?.into_iter().find(|r| r.subnqn == subnqn))
    }

    /// Law 2: read-modify-write under `flock` on `shares.json.lock`,
    /// then `shares.json.tmp` → fsync → rename → fsync parent dir.
    /// Mutations (unlike loads) may create the state dir and lock file.
    fn mutate<T>(&self, f: impl FnOnce(&mut Vec<ShareRecord>) -> io::Result<T>) -> io::Result<T> {
        fs::create_dir_all(&self.state_dir).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "share ledger: cannot create state dir {}: {e}",
                    self.state_dir.display()
                ),
            )
        })?;
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
        // SAFETY: flock on a valid, owned fd; released on close (drop of
        // `lock_file` at the end of this scope, on every path).
        loop {
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
            // EINTR: retry the blocking lock.
        }

        let mut shares = self.load()?;
        let out = f(&mut shares)?;

        let file = LedgerFile {
            format: LEDGER_FORMAT,
            shares,
        };
        let mut json = serde_json::to_vec_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        json.push(b'\n');

        let tmp = self.tmp_path();
        {
            let mut tmp_file = fs::File::create(&tmp)?;
            tmp_file.write_all(&json)?;
            tmp_file.sync_all()?;
        }
        fs::rename(&tmp, self.ledger_path())?;
        // Make the rename durable: fsync the parent directory.
        fs::File::open(&self.state_dir)?.sync_all()?;
        Ok(out)
    }

    /// Law 6 intent begin: append the record with `state: pending`
    /// **before the first stack mutation**. Refuses loud (AlreadyExists)
    /// on a duplicate `subnqn` or a duplicate `backing_canonical` — the
    /// ledger half of the cross-stack duplicate-backing guard; the
    /// refusal names the holder.
    pub fn begin_share(&self, record: &ShareRecord) -> io::Result<()> {
        record.validate()?;
        if record.state != ShareState::Pending {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "share ledger: begin_share requires a pending intent record (law 6 — record \
                     before mutate), got state '{}' for '{}'",
                    record.state.as_str(),
                    record.subnqn
                ),
            ));
        }
        self.mutate(|shares| {
            if let Some(holder) = shares.iter().find(|r| r.subnqn == record.subnqn) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "subsystem '{}' is already in the share ledger (stack {}, state {}); \
                         unshare it first: squeezefs nvmeof unshare {}",
                        holder.subnqn,
                        holder.stack.as_str(),
                        holder.state.as_str(),
                        holder.subnqn
                    ),
                ));
            }
            if let Some(holder) = shares
                .iter()
                .find(|r| r.backing_canonical == record.backing_canonical)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "backing path '{}' is already shared under subsystem '{}' (stack {}, \
                         state {}) — the same backing must never be double-served, across \
                         stacks included; unshare the holder first: squeezefs nvmeof \
                         unshare {}",
                        record.backing_canonical,
                        holder.subnqn,
                        holder.stack.as_str(),
                        holder.state.as_str(),
                        holder.subnqn
                    ),
                ));
            }
            shares.push(record.clone());
            Ok(())
        })
    }

    /// Law 6 intent finalize: `pending` → `active`, only after the last
    /// stack mutation succeeded. Any other current state is an error.
    pub fn finalize_share(&self, subnqn: &str) -> io::Result<()> {
        self.transition(subnqn, "finalize_share", |state| match state {
            ShareState::Pending => Ok(ShareState::Active),
            other => Err(other),
        })
    }

    /// Law 6 teardown intent: flip to `removing` **before the first
    /// teardown write**. Idempotent on an already-`removing` record
    /// (resumed teardown); NotFound if the record does not exist.
    pub fn mark_removing(&self, subnqn: &str) -> io::Result<()> {
        self.transition(subnqn, "mark_removing", |_| Ok(ShareState::Removing))
    }

    fn transition(
        &self,
        subnqn: &str,
        verb: &str,
        next: impl FnOnce(ShareState) -> Result<ShareState, ShareState>,
    ) -> io::Result<()> {
        self.mutate(|shares| {
            let record = shares
                .iter_mut()
                .find(|r| r.subnqn == subnqn)
                .ok_or_else(|| not_found(subnqn, verb))?;
            match next(record.state) {
                Ok(new_state) => {
                    record.state = new_state;
                    Ok(())
                }
                Err(current) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "share ledger: {verb} on '{subnqn}' is illegal from state '{}' \
                         (law 6 intent state machine)",
                        current.as_str()
                    ),
                )),
            }
        })
    }

    /// Law 6 completion: remove the record after teardown completed.
    pub fn delete(&self, subnqn: &str) -> io::Result<()> {
        self.mutate(|shares| {
            let before = shares.len();
            shares.retain(|r| r.subnqn != subnqn);
            if shares.len() == before {
                return Err(not_found(subnqn, "delete"));
            }
            Ok(())
        })
    }

    /// Law 5: the loop-device association is ledger bookkeeping (never
    /// configfs). Recorded mid-share (the device is only known after
    /// `losetup`) and refreshed on restore replay.
    pub fn set_loop_device(&self, subnqn: &str, loop_device: Option<String>) -> io::Result<()> {
        self.mutate(|shares| {
            let record = shares
                .iter_mut()
                .find(|r| r.subnqn == subnqn)
                .ok_or_else(|| not_found(subnqn, "set_loop_device"))?;
            record.loop_device = loop_device;
            Ok(())
        })
    }

    /// General bookkeeping refresh under the same law-2 atomicity:
    /// restore uses it to stamp an identity onto a pre-rebuild (N1-era)
    /// record exactly once and to record freshly-allocated port ids for
    /// records that predate the reserved-range allocator. The mutator
    /// must never change `subnqn` (the record key) — validated after.
    pub fn update_record(
        &self,
        subnqn: &str,
        mutator: impl FnOnce(&mut ShareRecord),
    ) -> io::Result<()> {
        self.mutate(|shares| {
            let record = shares
                .iter_mut()
                .find(|r| r.subnqn == subnqn)
                .ok_or_else(|| not_found(subnqn, "update_record"))?;
            mutator(record);
            if record.subnqn != subnqn {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update_record must not change the record key (subnqn)",
                ));
            }
            record.validate()
        })
    }
}

fn not_found(subnqn: &str, verb: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("share ledger: {verb}: no record for subsystem '{subnqn}'"),
    )
}

/// Strict schema-v1 parse behind the forward-only version gate (law 3).
fn parse_ledger(bytes: &[u8], path: &Path) -> io::Result<Vec<ShareRecord>> {
    let invalid = |detail: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("share ledger {} is invalid: {detail}", path.display()),
        )
    };
    // Version gate first, on a loose parse: a future format may carry
    // fields v1 has never heard of and must still produce the upgrade
    // message, not a field error.
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| invalid(format!("not valid JSON: {e}")))?;
    let format = value
        .get("format")
        .and_then(|f| f.as_u64())
        .ok_or_else(|| invalid("missing/non-integer \"format\" header".to_string()))?;
    if format > LEDGER_FORMAT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "share ledger {} is format {format}, created by a newer squeezefs (this binary \
                 reads format {LEDGER_FORMAT}); upgrade this squeezefs binary — the ledger is \
                 forward-only and is never rewritten by an older reader",
                path.display()
            ),
        ));
    }
    if format < LEDGER_FORMAT {
        return Err(invalid(format!(
            "format {format} never existed (the ledger started at format {LEDGER_FORMAT})"
        )));
    }
    let file: LedgerFile = serde_json::from_value(value).map_err(|e| {
        invalid(format!(
            "schema-v1 violation (unknown or malformed field): {e}"
        ))
    })?;
    for record in &file.shares {
        record.validate()?;
    }
    Ok(file.shares)
}

/// §6.4 old-registry migration: the first mutating verb of the new
/// binary renames a pre-existing pre-rebuild registry file to
/// `<path>.retired-by-rebuild` with one loud line. Never reads it (as
/// root the old registry was never readable back — truncated before
/// every read); whatever bytes it holds are preserved by the rename.
/// Failures are logged loud and never fail the verb.
pub fn retire_old_registry(path: &Path) {
    if !path.exists() {
        return;
    }
    let mut retired = path.as_os_str().to_owned();
    retired.push(".retired-by-rebuild");
    let retired = PathBuf::from(retired);
    if retired.exists() {
        log::warn!(
            "old NVMe-oF share registry {} still present but {} already exists — leaving both \
             in place (retire it manually)",
            path.display(),
            retired.display()
        );
        return;
    }
    match fs::rename(path, &retired) {
        Ok(()) => log::warn!(
            "retired the pre-rebuild NVMe-oF share registry {} -> {} — it was never readable \
             back (truncated before every read; see docs/design-nvmeof-target-management.md \
             §6.4); the share ledger under {} replaces it",
            path.display(),
            retired.display(),
            DEFAULT_STATE_DIR
        ),
        Err(e) => log::warn!(
            "could not retire the pre-rebuild NVMe-oF share registry {} ({e}) — it is unused \
             either way; remove or rename it manually",
            path.display()
        ),
    }
}

/// RFC 3339 UTC timestamp for `created_utc` (no chrono dependency —
/// civil-from-days conversion, unit-tested against known vectors).
pub fn utc_now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_secs_to_rfc3339(secs)
}

/// Days-to-civil conversion (Howard Hinnant's `civil_from_days`),
/// seconds precision, always Zulu.
fn epoch_secs_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let mut year = yoe as i64 + era * 400;
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::epoch_secs_to_rfc3339;

    #[test]
    fn test_epoch_to_rfc3339_known_vectors() {
        assert_eq!(epoch_secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
        // Leap-year day.
        assert_eq!(epoch_secs_to_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        // The §6.4 schema example timestamp.
        assert_eq!(epoch_secs_to_rfc3339(1_784_296_931), "2026-07-17T14:02:11Z");
        // End-of-year boundary.
        assert_eq!(epoch_secs_to_rfc3339(1_767_225_599), "2025-12-31T23:59:59Z");
    }
}
