//! `KvMetaBackend` — the v3 volume backend, **read side** (PR K6a; design
//! §4.5, §5.2; the §4.4 commit pipeline and checkpoint scheduling are
//! PR K6b's).
//!
//! ## Mount sequence (§3 "mount O(active set)", §4.x)
//!
//! 1. **Superblock**: sector 0 via the K6a version gate — torn/foreign/
//!    unknown-incompat superblocks fail loud (§4.10 durable-coverage
//!    units).
//! 2. **Root ledger**: newest slot whose checksum verifies; a torn newest
//!    slot falls back to its predecessor (K3). **No valid slot on a
//!    v3-superblocked volume is loud** — format always writes one, so
//!    all-slots-invalid is real corruption (§4.1's loud list), the policy
//!    K3's `read_newest_ledger` deferred to this mount wiring.
//! 3. **Allocator bitmap**: newest-valid A/B page per pair + journal
//!    delta replay (K4).
//! 4. **Journal replay — read-only, into the K5 cache**: the K3 scan
//!    recovers entries ≥ `journal_tail_seq`; tree records apply to the
//!    RAM-authoritative node cache **with their original seqs** (per-key
//!    LWW by seq — replay reproduces RAM, the K1 fold theorem). Nothing
//!    is written back; dirty deltas stay in RAM for K6b's checkpoint
//!    task, exactly like live commits will.
//! 5. **Trees**: roots pinned via `KvTree::open`; `next_ino` recovered as
//!    `max(ledger.next_ino, max replayed ino + 1)` (§4.8).
//!
//! Reads (`lookup` / `getattr` / `readdir` / `getxattr` / `listxattr`)
//! serve from the K5 latch-free snapshots + THE K1 fold; the mutating
//! `Metadata` trait impl (K6b) and the routed `routed_*` arms complete
//! the surface [`crate::meta_backend::RoutedMetaBackend`] drives.
//!
//! ## Readdir offset contract (§5.1, backend half)
//!
//! v3 honors `offset`/`max` (v2 ignores them): offsets 0/1/2 resume from
//! the directory start (the FUSE layer owns synthetic `.`/`..` emission —
//! PR K7); an offset `c > 2` resumes at entries whose dentry-key suffix is
//! **strictly greater** than `c − 3`. Cookies are the key itself, so they
//! are stable across concurrent inserts/removals.

use super::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
use super::checkpoint::{read_newest_ledger, LedgerRecord};
use super::conveyor_core::ConveyorCore;
use super::journal::{checkpoint_reserve_bytes, entry_len_for, untag, JournalRing};
use super::journal_core::{AdmissionClass, Reservation};
use super::node::{key_successor, NodeLayout};
use super::node_cache::{
    CachedNode, LiveLookup, NodeCache, NodeCacheConfig, OwnedRec, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use super::record::{
    decode_dentry_key, decode_inode_key, decode_readdir_cookie, decode_xattr_key, dentry_key,
    dentry_name_hash54, encode_readdir_cookie, first_free_coll_seq, inode_key, xattr_key,
    xattr_name_hash56, DentryValue, InodeDelta, InodeValue, ReaddirPos, Record, RecordKind,
    XattrValue, HASH54_MAX, HASH56_MAX, TREE_ALLOC_RESERVED, TREE_DENTRIES, TREE_INODES,
    TREE_XATTRS,
};
use super::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use super::tree::{decode_interior_value, KvTree, RootPtr, SmoContext, SmoJournal};
use super::KvError;
use crate::error::Result;
use crate::meta_backend::atomicity::META_VOLUME_ATOMICITY_COW;
// DLM S3.5 (design-cow-kv-metadata §4.10a): the cross-volume plan
// vocabulary this file's applier consumes.
use crate::meta_backend::crossvol_tx::{self, XvLocalStep, XvRider, XvStepOutcome, XvStepStatus};
use crate::meta_backend::dlm::{DlmGuard, DlmLockManager, LockMode};
use crate::meta_backend::sync_coalescer::SyncCoalescer;
use crate::meta_backend::{DirEntry, Ino, Inode, Metadata};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

/// `SQUEEZEFS_META_NODE_CACHE_MB` (§5.1; absolute MiB, explicit wins
/// verbatim — default derived, see [`resolve_node_cache_budget`]).
pub const NODE_CACHE_MB_ENV: &str = "SQUEEZEFS_META_NODE_CACHE_MB";

/// Pending-free FIFO capacity handed to the K4 allocator at mount. K6b's
/// checkpoint cadence keeps the live count far below it; the replay-window
/// contract (`ExtentAllocator::load`) fails loud if a recovered window
/// exceeds it.
pub const PENDING_FREE_CAP: usize = 65_536;

/// Range-scan page size for chained reads (readdir/listxattr/probes).
const SCAN_PAGE: usize = 512;

/// Mount-scoped replay outcome (§10: `meta_kv_replay_entries`,
/// `meta_kv_replay_dropped_torn`, `meta_kv_replay_ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvReplayStats {
    /// Entries recovered and applied.
    pub entries: u64,
    /// Drop-and-resync events confirmed by a later entry — nonzero after
    /// a crash is working-as-designed; nonzero after a clean unmount is
    /// the corruption alert (§10).
    pub dropped_torn: u64,
    /// Wall-clock replay time.
    pub replay_ms: u64,
}

/// Consecutive journal-write failures that latch the volume failed
/// (§4.4 pt 4 "repeated journal write failures").
const JOURNAL_FAILURE_LATCH: u64 = 3;

/// Test seam (PR M4 D1.b): artificial stall, in milliseconds, injected in
/// `commit_tx` between ring admission and the locked window — the exact
/// await span where a dropped future leaks admitted budget. One relaxed
/// load per commit when zero; the watchdog/no-drop suite arms it to hold
/// a live commit open deterministically (no sleep-based test sync — the
/// stall IS the scenario under test, not a coordination primitive).
pub static TEST_COMMIT_ADMITTED_STALL_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam (PR M7 §5.5 D5): hold the conveyor pass at a protocol stage
/// so arrivals accumulate deterministically (no sleep-based batching in
/// tests — the hold IS the scenario). `0` = off (one relaxed load per
/// pass iteration); [`TEST_CONVEYOR_HOLD_PRE_DRAIN`] parks the pass
/// before it drains a batch (entries queue up behind it);
/// [`TEST_CONVEYOR_HOLD_PRE_FANOUT`] parks it after the batch is written,
/// acked and barriered but before results fan out (the cancel-pre-fanout
/// stage). Release via [`test_conveyor_hold_release`].
pub static TEST_CONVEYOR_HOLD_STAGE: AtomicU64 = AtomicU64::new(0);

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the pass before draining.
pub const TEST_CONVEYOR_HOLD_PRE_DRAIN: u64 = 1;

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the pass after the batch's
/// write/ack/barrier, before per-tx result fan-out.
pub const TEST_CONVEYOR_HOLD_PRE_FANOUT: u64 = 2;

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the pass's EMPTY-drain tail
/// **while its per-iteration backend `Arc` upgrade is still held** — the
/// one place the pass pins a dropped-without-shutdown backend (and its
/// Layer A writer flock) past the committer's wake. Models the OS
/// descheduling the pass's worker thread between the upgrade and the
/// `drop(be)`: the 2026-07-27 torn-claim remount flake window
/// (`mount_writer_guard_tests::test_torn_claim_entry_recovers_and_
/// reclaims` under full-suite load).
pub const TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL: u64 = 3;

/// Monotonic count of passes that PARKED on
/// [`TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL`] while holding their backend
/// upgrade — the test-side barrier proving the pin formed (the pass wins
/// the upgrade-vs-Arc-drop race ~always, but the barrier makes the
/// scenario honest instead of timing-lucky).
pub static TEST_CONVEYOR_EMPTY_TAIL_PARKED: AtomicU64 = AtomicU64::new(0);

/// The parked-pass wake for [`TEST_CONVEYOR_HOLD_STAGE`] (register-recheck
/// discipline — a stale release can never strand a pass).
static TEST_CONVEYOR_HOLD_NOTIFY: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Release every pass parked on [`TEST_CONVEYOR_HOLD_STAGE`] (callers
/// store `0` first; the notify wakes the register-recheck loop).
pub fn test_conveyor_hold_release() {
    TEST_CONVEYOR_HOLD_NOTIFY.notify_waiters();
}

/// Test seam (PR M7 §5.5 D5, the per-tx isolation leg): a conveyor batch
/// member staging any record on `inode_key(ino)` fails at RAM-apply time
/// with a synthetic error — the "poisoned tx" the isolation contract
/// says must fail ALONE while the rest of its batch commits. `0` = off
/// (one relaxed load per applied tx).
pub static TEST_CONVEYOR_POISON_APPLY_INO: AtomicU64 = AtomicU64::new(0);

/// Test seam (docs/design-smo-replay-currency.md §6 PR 4, the at-cap
/// rows): overrides [`PENDING_FREE_CAP`] at `open` so the §4.7 at-cap
/// force-cycle protocol (admission headroom / forced `checkpoint_cycle` /
/// bounded-retry-then-loud) is reachable at cargo scale — the production
/// cap is 65,536 SMO retirements. `0` = off (one relaxed load per open —
/// a control-plane path). Values must be ≥ 2 ([`super::alloc_ext_core`]'s
/// Vyukov stamp-aliasing floor, hard-asserted there).
pub static TEST_PENDING_FREE_CAP: AtomicU64 = AtomicU64::new(0);

/// Test seam (rewrite-publish-drain Lever B, 2026-08-01; the
/// [`crate::routing::TEST_PUBLISH_PASS_DELAY_MS`] pattern): artificial
/// delay, in milliseconds, at the head of every layout-merge conveyor
/// pass iteration — lets concurrent-ino saves accumulate
/// deterministically so the one-KvTx aggregation contract is testable
/// on µs-commit sandboxes. One relaxed load per pass; zero-cost unset.
pub static TEST_LAYOUT_MERGE_HOLD_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam (the 2026-08 wedge-crumb fix): disable
/// [`KvMetaBackend::cover_bring_up_residue`] at open. The cover IS "the
/// first post-mount durable checkpoint" the §2-A mount-gate law names,
/// so on the product path the re-parked replayed frees are discharged
/// before `open` returns — correct, but it makes the park/drain
/// progression invisible to the pins that keep §2-A red-stays-red
/// (`tests/kv_smo_crash_completeness_tests.rs`). Those suites arm this
/// to observe the parked window and drive the drain themselves;
/// production always covers. `false` = off (one relaxed load per open —
/// a control-plane path).
pub static TEST_BRING_UP_COVER_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `SQUEEZEFS_TIMEOUT` as the D1.b watchdog/escalation threshold
/// (design-metadata-throughput §6): read per `open` (control-plane —
/// never on an op path), default 30 s. Deliberately NOT process-memoized:
/// each mount/open captures the env it was launched with (and tests can
/// vary it per sandbox).
fn squeezefs_timeout_env() -> std::time::Duration {
    std::env::var("SQUEEZEFS_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(30))
}

/// Commit-path retry budget against SMO revalidation races (§4.6: SMOs
/// are rare and serialized, so the loop is short; exhaustion is a bug).
const COMMIT_RETRY_BUDGET: usize = 256;

/// PR M7 (design-metadata-throughput §5.5 D5): conveyor batch caps —
/// the pass drains whatever is queued, bounded by these (NO timers;
/// jbd2's no-wait batch shape). Env-tunable per mount (`§6 API` row);
/// read at `open` like every backend knob.
pub const COMMIT_BATCH_TXS_ENV: &str = "SQUEEZEFS_META_COMMIT_BATCH_TXS";
pub const COMMIT_BATCH_BYTES_ENV: &str = "SQUEEZEFS_META_COMMIT_BATCH_BYTES";
/// The shipped M7 batch caps — the derived defaults' FLOORS since the
/// 2026-08-04 derivation sweep (never-regress-below-shipped, the
/// `Q_DEPTH_FLOOR` house law).
const COMMIT_BATCH_TXS_FLOOR: usize = 64;
const COMMIT_BATCH_BYTES_FLOOR: u64 = 256 * 1024;

/// `SQUEEZEFS_META_COMMIT_BATCH_TXS` resolution, pure (2026-08-04
/// derivation sweep): env (≥ 1) wins verbatim; derived default =
/// `max(64, cpus × 2)` — committer arrivals scale with handler
/// parallelism (transport queues = kernel possible CPUs), floor 64 =
/// the shipped M7 posture (a 32-CPU box derives exactly 64).
pub fn resolve_commit_batch_txs(env: Option<&str>, cpus: usize) -> usize {
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or_else(|| cpus.saturating_mul(2).max(COMMIT_BATCH_TXS_FLOOR))
}

/// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` resolution, pure (2026-08-04
/// derivation sweep): env (≥ 1) wins verbatim; derived default =
/// `max(256 KiB, ring user-capacity / 16)` — a batch is a fixed
/// fraction of ITS volume's journal ring so ≥ 16 batch reservations
/// always cycle (the liveness-margin shape); floor 256 KiB = the
/// shipped M7 posture. The senior per-volume clamp to the ring's
/// admissible capacity (§4.4 pt 5, applied at open) is unchanged.
pub fn resolve_commit_batch_bytes(env: Option<&str>, ring_user_capacity: u64) -> u64 {
    env.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or_else(|| (ring_user_capacity / 16).max(COMMIT_BATCH_BYTES_FLOOR))
}

/// PR M6: pending-times drain batch — inos per DLM `lock_many` set / per
/// drain transaction (the `destroy_inodes` batching shape).
const PENDING_TIMES_DRAIN_BATCH: usize = 128;

/// PR M6: pending-times population that wakes the drain task ahead of its
/// cadence tick (bounds the map and the crash-loss window by count, not
/// just time).
const PENDING_TIMES_DRAIN_CAP: u64 = 512;

/// VAL-2 (pre-RC engineering spec §3): the **positive allowlist** every
/// externally-reachable xattr name must pass — the FUSE boundary
/// (`src/fuse_client.rs` set/get/list/removexattr) and this backend's
/// generic [`Metadata`] entry points both enforce it.
///
/// Permitted: `user.*` (minus the internal `user.squeezefs.` family),
/// `security.*` (the kernel's own killpriv/LSM namespace) and `trusted.*`
/// (VFS-gated on `CAP_SYS_ADMIN`). **Everything else is refused** — EPERM
/// on set/remove, absent on get, filtered from listings.
///
/// A denylist could not hold this line: SqueezeFS keeps unprefixed
/// internal records in the same keyspace, and the pre-RC denylist
/// (`job:` + `user.squeezefs.`) left four of them writable from any
/// unprivileged shell —
///
/// - `system.symlink` — every symlink's target. The Linux VFS deliberately
///   performs NO permission check for `system.*` (`xattr_permission()`
///   returns 0 early: "Decision on these is left to the underlying
///   filesystem"), so the daemon is the ONLY enforcement point;
/// - `layout` — the per-inode block map, `block_prefix`, `file_id` and
///   wrapped data-key material. A write of another file's decodable value
///   redirects reads to that file's blocks;
/// - [`WRITER_CLAIM_XATTR`] — the D0 single-writer guard. Removing it
///   presents the volume set as unclaimed to another host's takeover
///   ladder;
/// - `client:{id}` — the live-client registrations guarding
///   `config set-cache-paths`.
///
/// An allowlist also refuses internal records nobody has invented yet,
/// which is the property a denylist can never have.
///
/// The daemon's own record writers never come through here: they use the
/// `*_internal` entry points ([`KvMetaBackend::setxattr_internal`],
/// [`KvMetaBackend::removexattr_internal`], and the inherent
/// `getxattr`/`listxattr`).
pub fn xattr_name_allowed(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("user.") {
        // `user.squeezefs.` is the internal family (format config, the L4
        // bootstrap blob, future records); `user.squeezefsX` is not.
        return !rest.starts_with("squeezefs.");
    }
    name.starts_with("security.") || name.starts_with("trusted.")
}

/// The single-writer mount guard's claim record: an xattr on ino 1 beside
/// the `client:{id}` registrations (design-metadata-throughput §5.0 B2).
/// JSON `{"id","ts","pid","boot"}` — plus `"term"` on volumes carrying
/// incompat bit 7 (DLM S2); staleness follows the ONE staleness law
/// ([`crate::fuse_client::CLIENT_STALE_TTL_SECS`]).
pub const WRITER_CLAIM_XATTR: &str = "writer_claim";

/// DLM S2 (spec §6.7 decision 4, §6.9): the volume's **durable writer
/// term** record — JSON `{"term":N}` at local ino 1, beside the claim.
///
/// Separate from the claim because the claim is DELETED at clean
/// unmount (so the volume presents as unclaimed to another host): the
/// era ladder must outlive that deletion, or a mount/unmount cycle would
/// reset the term and re-issue tokens a crashed predecessor already
/// used. Written (and barriered) by the mount gate only on volumes
/// carrying [`super::superblock::FEATURE_INCOMPAT_KV_DURABLE_TERM`];
/// never deleted, never rewritten after the gate, and — like the claim —
/// PER-VOLUME control state that never travels with a migrating slot.
pub const WRITER_TERM_XATTR: &str = "writer_term";

/// A decoded `writer_claim` record (design-metadata-throughput §5.0 B2):
/// the mounted writer's identity, heartbeat timestamp, pid, and boot id —
/// the evidence the mount-time staleness / dead-pid-proof decisions
/// consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterClaim {
    /// The writer's per-mount identity (uuid).
    pub id: String,
    /// Heartbeat timestamp (unix seconds) — the authoritative liveness
    /// signal, refreshed every `CLIENT_HEARTBEAT_INTERVAL_SECS`.
    pub ts: u64,
    /// Holder pid (same-host dead-pid proof: `boot` matches this boot AND
    /// `kill(pid, 0) == ESRCH` ⇒ automatic instant reclaim).
    pub pid: u32,
    /// Holder boot id (`/proc/sys/kernel/random/boot_id`) — scopes the
    /// pid proof to this boot (pid-reuse mitigation).
    pub boot: String,
    /// DLM S2 (spec §6.7 decision 4): the holder's **durable writer
    /// term** — the high-order component of every fencing token it mints
    /// (`(term << 40) | grant_seq`). Bumped past every predecessor's at
    /// claim acquisition and barriered before the guard arms, so a
    /// successor's grants dominate every token a crashed predecessor
    /// left on staging (spec §6.11).
    ///
    /// `0` on volumes without incompat bit 7 and on every pre-S2 record:
    /// term 0 composes to the bare grant sequence — the pre-S2 behavior
    /// byte-for-byte, including this record's JSON (the `term` key is
    /// omitted when 0).
    pub term: u64,
}

impl WriterClaim {
    /// Encode as the compact JSON the record stores. `term == 0` (an
    /// un-stamped volume) omits the key: pre-S2 volumes keep
    /// byte-identical claim records.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = serde_json::json!({
            "id": self.id,
            "ts": self.ts,
            "pid": self.pid,
            "boot": self.boot,
        });
        if self.term != 0 {
            v["term"] = serde_json::json!(self.term);
        }
        v.to_string().into_bytes()
    }

    /// Decode a stored claim. `None` for unparseable values — callers
    /// treat those as a stale *foreign* claim (never auto-taken: a value
    /// we cannot attribute cannot prove anything). A record without
    /// `term` (pre-S2, or an un-stamped volume) decodes as term 0 — the
    /// additive-field law the `client:` records' `job_endpoint` already
    /// established.
    pub fn decode(val: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(val).ok()?;
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            ts: v.get("ts")?.as_u64()?,
            pid: v.get("pid")?.as_u64()? as u32,
            boot: v.get("boot")?.as_str()?.to_string(),
            term: v.get("term").and_then(|t| t.as_u64()).unwrap_or(0),
        })
    }

    /// Claim age in seconds against `now` (unix seconds).
    pub fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.ts)
    }
}

/// The root-ino xattr key prefix of a mount registration
/// (`client:{uuid}`), written/refreshed by the mount heartbeat
/// ([`crate::fuse_client::SqueezefsFilesystem::refresh_client_registration`]).
pub const CLIENT_REGISTRATION_PREFIX: &str = "client:";

/// One mount registration read from a volume's root-ino xattrs — a
/// `client:{id}` heartbeat record or the single-writer guard's
/// [`WRITER_CLAIM_XATTR`] — classified under the ONE staleness law
/// ([`crate::fuse_client::CLIENT_STALE_TTL_SECS`]) that the format
/// preflight and the mount gate already share. This is a *read* of the
/// records the mount heartbeat maintains, surfaced by `squeezefs
/// clients` / `squeezefs status`; it introduces no new liveness protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRegistration {
    /// The root-ino xattr key (`client:{uuid}` or `writer_claim`).
    pub key: String,
    /// `"client"` (mount registration) or `"writer"` (guard claim).
    pub kind: &'static str,
    /// The registration uuid (`client:` suffix) or the claim's writer id.
    pub id: String,
    /// Holder pid from the record (same-host diagnosis; the claim scopes
    /// it with `boot`). `None` when the value is unparseable.
    pub pid: Option<u32>,
    /// Holder boot id (`writer_claim` records only).
    pub boot: Option<String>,
    /// Heartbeat timestamp (unix seconds); `None` = unparseable value,
    /// which every consumer treats as stale.
    pub heartbeat_ts: Option<u64>,
    /// Heartbeat age against the read instant.
    pub age_secs: Option<u64>,
    /// The format preflight's live predicate: parseable timestamp with
    /// `age <= CLIENT_STALE_TTL_SECS`.
    pub heartbeat_fresh: bool,
    /// The mount gate's same-host dead-pid proof (`writer_claim` only —
    /// `boot` scopes the pid to this boot): `kill(pid, 0) == ESRCH`. A
    /// kill -9'd holder classifies reclaimable *before* its heartbeat
    /// expires, exactly like the guard's instant-reclaim decision.
    pub holder_provably_dead: bool,
    /// The coordinator's §5.1.6 job-wire endpoint (`ip:port`) — an
    /// ADDITIVE field on `client:{id}` records (PR VL2b): only the
    /// wire-hosting mount writes it; remote workers discover the live
    /// coordinator through it. `None` on writer claims, legacy records,
    /// and non-coordinator mounts.
    pub job_endpoint: Option<String>,
}

impl MountRegistration {
    /// Operator-facing state under the existing classification ladder:
    /// dead-pid proof ⇒ `"dead"` (reclaimable), fresh heartbeat ⇒
    /// `"live"`, otherwise `"stale"`.
    pub fn state(&self) -> &'static str {
        if self.holder_provably_dead {
            "dead"
        } else if self.heartbeat_fresh {
            "live"
        } else {
            "stale"
        }
    }

    /// The one JSON shape both CLI surfaces (`clients --json`, `status`
    /// `"Clients"`) emit for a registration record.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "key": self.key,
            "id": self.id,
            "pid": self.pid,
            "boot": self.boot,
            "heartbeat_ts": self.heartbeat_ts,
            "age_secs": self.age_secs,
            "state": self.state(),
            "job_endpoint": self.job_endpoint,
        })
    }
}

/// Encode the durable writer-term record ([`WRITER_TERM_XATTR`]).
fn encode_writer_term(term: u64) -> Vec<u8> {
    serde_json::json!({ "term": term }).to_string().into_bytes()
}

/// Decode the durable writer-term record. `None` = unparseable, which
/// the mount gate refuses loud (never a silent era reset).
fn decode_writer_term(val: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(val).ok()?;
    v.get("term")?.as_u64()
}

/// Parse the unix-seconds heartbeat timestamp from a registration value
/// (`{"ts":<secs>,…}` — both `client:{id}` records and the
/// `writer_claim` carry it). `None` for legacy/unparseable values, which
/// every consumer treats as stale.
fn parse_registration_ts(val: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(val).ok()?;
    v.get("ts")?.as_u64()
}

/// Outcome of [`KvMetaBackend::claim_clear`] — the operator-attested
/// `squeezefs claim clear` admin verb (design-metadata-throughput §5.0:
/// the recovery rung for a stale cross-host claim on a non-PR volume).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimClearOutcome {
    /// The volume carries no `writer_claim` — nothing to clear.
    NoClaim,
    /// A stale claim was removed; the holder it named is returned for the
    /// verb's attestation output.
    Cleared(WriterClaim),
}

/// PR VL5b (design-volume-lifecycle §5.5.2 step 3): the conveyor
/// pass-task key tee. Records do not carry journal sequence numbers and
/// the ring wraps many times during a large-slot copy, so "replay since
/// seq X" is unavailable — instead the per-volume pass task (the sole
/// leaf-lock taker for user commits) tees the KEYS of records routed to
/// the migrating keyspace into this bounded in-RAM side log. Keys only —
/// values are re-read from the authoritative tree at delta-apply time.
///
/// **Overflow rule** (§5.5.2): a full side log latches `overflowed` and
/// drops its contents (bounding memory); the engine's next round flips
/// to a fresh full snapshot pass — correct by construction (the snapshot
/// supersedes the lost keys). Three consecutive overflows abort the
/// migration loudly.
pub struct MigrationTee {
    /// The migrating keyspace on THIS volume: owning inos in `[lo, hi)`.
    lo: u64,
    hi: u64,
    /// Key-count cap (≈ 40 B/key; the design default 1 M keys ≈ 40 MiB).
    cap: usize,
    /// The side log: `(tree_id, key)` — deduped (per-key LWW means one
    /// re-read covers any number of commits).
    log: parking_lot::Mutex<std::collections::HashSet<(u8, Vec<u8>)>>,
    overflowed: AtomicBool,
    /// PER-VOLUME control-record exclusion: the source's own
    /// `writer_claim` xattr `(root ino, name hash56)` — its heartbeat
    /// rewrites ride the conveyor and must neither travel with the slot
    /// nor keep the cutover's tee-empty recheck from settling.
    exclude_xattr: std::sync::OnceLock<(u64, u64)>,
}

impl MigrationTee {
    pub(super) fn new(lo: u64, hi: u64, cap: usize) -> Self {
        Self {
            lo,
            hi,
            cap,
            log: parking_lot::Mutex::new(std::collections::HashSet::new()),
            overflowed: AtomicBool::new(false),
            exclude_xattr: std::sync::OnceLock::new(),
        }
    }

    /// Install the writer-claim exclusion (see `exclude_xattr`).
    pub fn exclude_writer_claim(&self, root_ino: u64, name_hash56: u64) {
        let _ = self.exclude_xattr.set((root_ino, name_hash56));
    }

    /// The owning ino of a record key: the first 8 big-endian bytes in
    /// all three §4.2 trees (ino / parent ino / ino).
    fn key_owner(key: &[u8]) -> Option<u64> {
        key.get(..8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }

    /// Pass-task hook: tee the keys of one committed tx's records.
    fn note_committed(&self, recs: &[(u8, Record)]) {
        for (tree_id, r) in recs {
            // Neither of these trees is ino-keyed, so `key_owner` would
            // read a foreign field as an ino: the allocator's extent
            // index, or (spec §6.2 item 1) a block-reference record's
            // data-volume TAG — a random u64 that can fall inside any
            // migrating keyspace and would tee the record to a volume it
            // does not belong to.
            if *tree_id == TREE_ALLOC_RESERVED || *tree_id == super::record::TREE_BLOCK_REFS {
                continue;
            }
            let Some(ino) = Self::key_owner(&r.key) else {
                continue;
            };
            if ino < self.lo || ino >= self.hi {
                continue;
            }
            if *tree_id == TREE_XATTRS {
                if let (Some(&(ex_ino, ex_hash)), Ok((k_ino, k_hash, _))) =
                    (self.exclude_xattr.get(), decode_xattr_key(&r.key))
                {
                    if k_ino == ex_ino && k_hash == ex_hash {
                        continue; // the volume's own writer_claim heartbeat
                    }
                }
            }
            let mut log = self.log.lock();
            if log.len() >= self.cap {
                // Latch the overflow and drop the log — the engine's
                // fresh-snapshot fallback supersedes the lost keys, and
                // an unbounded log would defeat the 40 MiB budget.
                self.overflowed.store(true, Ordering::Release);
                log.clear();
                crate::fuse_client::METRICS
                    .meta_slot_delta_overflows
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            log.insert((*tree_id, r.key.clone()));
        }
    }

    /// Engine round drain: `(keys, overflowed)` — resets both for the
    /// next round.
    pub(crate) fn drain_round(&self) -> (Vec<(u8, Vec<u8>)>, bool) {
        let keys: Vec<(u8, Vec<u8>)> = {
            let mut log = self.log.lock();
            log.drain().collect()
        };
        let overflowed = self.overflowed.swap(false, Ordering::AcqRel);
        (keys, overflowed)
    }

    /// Non-draining size probe (the engine's cutover-threshold check).
    pub(crate) fn pending_len(&self) -> usize {
        self.log.lock().len()
    }

    /// Whether the current round already overflowed.
    pub(crate) fn has_overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }
}

/// One mounted v3 metadata volume.
pub struct KvMetaBackend {
    path: PathBuf,
    sb: SuperblockV3,
    ledger: LedgerRecord,
    /// The live §5.5.1a membership stamp (PR VL5a): seeded from the
    /// mounted ledger record; every checkpoint's ledger record carries
    /// it. `None` forever on legacy volumes. A plain `Mutex` — read once
    /// per checkpoint cycle (background task) and written only by
    /// format-grade admin verbs (`repair-set`), never on the hot path.
    membership_stamp: std::sync::Mutex<Option<super::checkpoint::MembershipStamp>>,
    /// PR VL5b (§5.5.2 / KD-7): per-slot GUEST ino cursors — one
    /// [`SlotCursor`] per hosted guest slot, seeded from the mounted
    /// stamp's `slot_cursors` (+ the replayed per-slot maxima) and
    /// published into every checkpoint's ledger record. Latch-free (scc)
    /// — minting is a hot-path `fetch_add`; the map itself mutates only
    /// on migration flips (control plane).
    guest_cursors: scc::HashMap<u16, Arc<super::slot_cursor_core::SlotCursor>>,
    /// PR VL5b (§5.5.2): the conveyor pass-task key tee — armed by the
    /// slot-migration engine for the duration of a bulk-copy/delta
    /// round, `None` (one arc-swap load per batch) otherwise. The pass
    /// task tees the KEYS of successfully committed records whose owning
    /// ino falls in the migrating keyspace; values are re-read at delta
    /// apply.
    migration_tee: arc_swap::ArcSwapOption<MigrationTee>,
    /// Shared node cache behind the three trees (§4.5). Held for tree
    /// lifetime; the trees clone the `Arc`.
    inodes: KvTree,
    dentries: KvTree,
    xattrs: KvTree,
    /// Pre-RC spec §6.2 item 1 (incompat bit 8): the **durable
    /// block-reference tree** — `Some` exactly when this volume carries
    /// [`super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS`] and the
    /// mount may write (a read-only mount never mints a root). `None`
    /// means derived accounting, i.e. pre-item-1 behavior verbatim.
    ///
    /// Deliberately NOT part of [`Self::trees`]: that array is the three
    /// §4.2 user trees, and the §4.10 digest walk / slot-migration
    /// keyspace / fsck tree walk are all defined over it. Structural
    /// consumers (checkpoint flush, ledger roots, maintenance) use
    /// [`Self::all_trees`], which includes this one.
    block_refs: Option<KvTree>,
    alloc: Arc<ExtentAllocator>,
    /// §4.8 monotonic watermark, recovered at mount; the create path
    /// `fetch_add`s it.
    next_ino: AtomicU64,
    /// **The writer era's ino floor** for this volume's NATIVE keyspace:
    /// the §4.8 watermark as recovered at open, before this mount minted
    /// anything. Inos are monotonic and never reused (§4.8), so
    /// `raw < floor` ⇔ "the record existed when this mount adopted its
    /// writer era" ⇔ minted under an EARLIER durable term (DLM S2). That
    /// is what makes fsck class C9 (unreferenced inodes) false-positive
    /// free: a live create legitimately holds an inode record before its
    /// dentry, and every ino this mount can mint is at or above this
    /// floor. See [`Self::minted_in_prior_era`].
    era_ino_floor_native: u64,
    /// [`Self::era_ino_floor_native`] per hosted GUEST keyspace, snapshotted
    /// once after the cursors are seeded at open. A keyspace with no entry
    /// (virgin, or a cursor that arrived mid-mount with a migrated slot)
    /// has no era floor and its records are never C9 candidates —
    /// fail-closed, the fsck posture (no verdict rather than a guess).
    era_ino_floor_guest: std::sync::OnceLock<std::collections::HashMap<u16, u64>>,
    /// Pre-RC spec §6.2 item 5 (incompat bit 12): **per-writer ino lane
    /// cursors**, keyed `(writer id, space)` where the space is the
    /// volume's native watermark or a hosted guest slot
    /// ([`super::ino_lane::InoSpace`]). One cell per lane the mount
    /// actually mints in.
    ///
    /// EMPTY on every mount today and on every un-stamped volume — solo
    /// minting keeps using `next_ino` / `guest_cursors` verbatim, so the
    /// shipped hot path is untouched (ruling D9: nothing stamps bit 12).
    /// The `lanes_live` latch below is what keeps the read side
    /// ([`Self::next_ino`], [`Self::live_inodes`]) at one relaxed load
    /// instead of an `scc` walk when no lane exists.
    lane_cursors: scc::HashMap<(u16, super::ino_lane::InoSpace), Arc<crate::lane_core::LaneCursor>>,
    /// `true` once any lane cursor exists (see [`Self::lane_cursors`]) —
    /// a relaxed latch, never cleared: a lane's watermark must keep
    /// counting toward the durable one for the rest of the mount.
    lanes_live: AtomicBool,
    /// POSIX-1: inode records this mount has DESTROYED (the `doomed`
    /// count of every committed `destroy_inodes` transaction) — what
    /// turns the monotonic §4.8 watermark into a live-population gauge
    /// for `statfs`. See [`Self::live_inodes`].
    destroyed_inodes: AtomicU64,
    replay: KvReplayStats,

    // ---- PR K6b: the commit pipeline + checkpoint state ----
    /// The journal ring (K6a dropped it after replay; K6b stores it).
    ring: Arc<JournalRing>,
    /// Level 4a of P1-9: the per-volume I/D lock manager.
    dlm: DlmLockManager,
    /// Group-commit fdatasync coalescer (§4.6 pt 4).
    sync: Arc<SyncCoalescer>,
    /// The shared node cache (the trees clone it; the checkpoint task
    /// walks it for dirty floors).
    cache: Arc<NodeCache>,
    /// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` == 0 ⇒ strict: every commit
    /// barriers (coalesced) before acking (§4.6 pt 4).
    strict: bool,
    /// Deferred-mode flush flag: commits set it; the checkpoint task's
    /// tick barrier clears it (the v2 flusher discipline).
    needs_flush: AtomicBool,
    /// The write gate (K6a hand-off): reads serve, every mutation is
    /// withheld. Two causes reach it — see [`ReadOnlyCause`].
    read_only: bool,
    /// WHY this volume is read-only. The write gate and the guarantee-class
    /// row both need to tell a §4.11 forward-compatibility degradation
    /// (unknown `features_ro` bits — an accident of the format) apart from
    /// an operator-requested **reader mount** (`-o ro` / `--read-only`,
    /// DLM S5): the refusal texts differ, the guard classes differ
    /// (`flock` vs `reader`), and conflating them is how a reader would end
    /// up reported as a guarded writer.
    ro_cause: ReadOnlyCause,
    /// §4.4 pt 4 fail-stop latch + its consecutive-failure counter.
    failed: AtomicBool,
    journal_failures: AtomicU64,
    /// `meta_kv_journal_full_stalls` (§4.4 pt 5): ring-admission parks.
    stalls: AtomicU64,
    /// Write-commit-economy (2026-07-30): whether this volume is cleared
    /// to stage layout delta records — `true` once the
    /// `KV_LAYOUT_DELTAS` incompat bit is durably on the superblock
    /// (seeded at open; ratcheted by [`Self::layout_deltas_ready`]
    /// before the FIRST delta record, per the KD-14
    /// bit-before-durable-record ordering).
    layout_deltas_ok: AtomicBool,
    /// Serializes the one-time incompat ratchet (sector-0 RMW must not
    /// race itself); contended at most once per volume lifetime.
    layout_delta_ratchet: crate::sqz_sync::SqzMutex<()>,
    // ---- PR M7: the §5.5 D5 commit conveyor ----
    /// The per-volume conveyor: all user commits enqueue here; a
    /// leader-elect committer spawns the detached pass task that drains
    /// batches (loom-modeled core, `conveyor_core.rs`). Behind its own
    /// `Arc`: the pass task owns the queue/leadership word directly and
    /// holds the *backend* only per batch (Weak between batches), so a
    /// dropped-without-shutdown backend releases its writer flock the
    /// moment the last user `Arc` dies — never parked behind an idling
    /// pass (the drop-then-reopen replay pattern).
    conveyor: Arc<ConveyorCore<QueuedTx>>,
    /// `Weak` self-reference the pass tasks upgrade per batch (set once,
    /// immediately after `Arc::new`, on every open path — the
    /// checkpoint-task `Weak` discipline).
    conveyor_self: std::sync::OnceLock<Weak<KvMetaBackend>>,
    /// Rewrite-publish-drain Lever B (2026-08-01): the per-volume
    /// layout-merge conveyor — delta-class layout saves aggregate into
    /// ONE multi-ino KvTx per pass (one journal entry, one ring write,
    /// one fan-out) instead of one commit per ino. Same lifecycle
    /// discipline as `conveyor` (leader-elect, Weak upgrade per batch).
    layout_conveyor: Arc<ConveyorCore<QueuedLayoutMerge>>,
    /// `SQUEEZEFS_META_COMMIT_BATCH_TXS` (default `max(64, cpus × 2)` —
    /// [`resolve_commit_batch_txs`]), read at open.
    batch_max_txs: usize,
    /// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` (default `max(256 KiB,
    /// ring/16)` — [`resolve_commit_batch_bytes`]) clamped to
    /// the ring's user-admissible capacity, so one Σ-admission can
    /// always eventually succeed (a batch larger than the admissible
    /// ring would park forever — the liveness clamp).
    batch_max_bytes: u64,
    /// `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` resolved ONCE at open
    /// (`checkpoint::resolve_max_dirty_nodes` — read at `open` like every
    /// backend knob). The checkpoint task consults this on EVERY cadence
    /// tick (default 50 ms), so per-tick re-resolution (env `CString`s,
    /// cgroup/`sysinfo` probes) is an allocation stream the op-economy
    /// contract forbids — the 2026-08-02 gate red on
    /// `ipc_op_economy_tests::warm_fast_path_serves_are_allocation_free`
    /// was exactly this site re-deriving per tick.
    dirty_node_cap: u64,
    /// PR M4 (design-metadata-throughput §5.1 D1.b): `SQUEEZEFS_TIMEOUT`
    /// read once at open (control-plane; default 30 s). Two consumers:
    /// the ring-admission park-escalation rung (audit row 2 — parked
    /// cumulatively ≥ this trips `note_journal_failure` per crossing) and
    /// the coalesced barrier bound (audit row 1 — `sync_device` waits are
    /// bounded + synthesized-error).
    timeout_threshold: std::time::Duration,
    /// The serialized SMO/checkpoint context (§4.6: "all SMOs run on the
    /// per-volume checkpoint task, one at a time" — K5's `&mut SmoContext`
    /// discipline carried by this mutex; the background task is the
    /// primary holder, `checkpoint_now`/`shutdown` share the exclusion).
    pub(super) smo: crate::sqz_sync::SqzMutex<SmoContext>,
    /// Last ledger seq written by a checkpoint (starts at the mounted
    /// record's seq).
    pub(super) checkpoint_seq: AtomicU64,
    /// The `journal_tail_seq` of the last ledger record written (starts
    /// at the mounted record's tail) — the §4.4 pt 4 hole discipline's
    /// progress observable ([`Self::checkpoint_past`]).
    pub(super) last_ledger_tail: AtomicU64,
    /// §4.7/§4.6 retire tag for SMO frees: always `checkpoint_seq + 1`
    /// (the NEXT ledger record); shared with the SMO hooks.
    pub(super) retire_seq: Arc<AtomicU64>,
    /// Ledger records written but not yet known durable, as
    /// `(journal_tail_seq, push_epoch)` — the §4.6 pt 3 pending-reclaim
    /// watermark and, since the Option-A coverage fix, the pending-free
    /// gate's clock too (design-smo-replay-currency §2-A).
    ///
    /// **DUR-3**: the `push_epoch` is [`Self::barrier_starts`] read at
    /// push time, i.e. after the record's write completed. An entry is
    /// released only by a barrier whose `sync_fn` STARTED after that
    /// (`push_epoch < barrier_durable`) — a barrier already in flight
    /// when the record was written cannot have made it durable, and
    /// releasing on it is the journal-hole / unmountable-volume vector
    /// the spec names. The coalescer is correct; this consumer used to
    /// misread its guarantee (it covers the caller's OWN prior write,
    /// never a third party's push into the window).
    pub(super) pending_reclaim: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Monotonic barrier-start ticket counter: every `sync_fn` invocation
    /// takes the next value as it begins (DUR-3).
    barrier_starts: AtomicU64,
    /// The highest barrier-start ticket whose barrier COMPLETED
    /// successfully (`fetch_max`). Bounded-out barriers never publish
    /// here — their outcome is unknown, so they certify nothing.
    barrier_durable: AtomicU64,
    /// Checkpoint-task lifecycle: shutdown flag + wake + join handle +
    /// liveness probe (`Weak<()>` of the token the task owns).
    shutting_down: AtomicBool,
    ckpt_wake: Arc<squeezefs_ipc::sqz_notify::Notify>,
    ckpt_join: std::sync::Mutex<Option<squeezefs_ipc::sqz_channel::oneshot::Receiver<bool>>>,
    ckpt_alive: std::sync::Mutex<Weak<()>>,

    // ---- PR M6: the SETATTR-echo absorber (design §5.4 D4) ----
    /// Pending times refinements, `ino → (mtime, ctime)`: the kernel's
    /// post-op ctime writeback echoes parked by `setattr_locked`'s absorb
    /// arm instead of committing one journal entry each (the measured
    /// whole second entry per rename/unlink — G4). Latch-free (hot-path
    /// policy); mutations run under the per-ino DLM I-guard (absorb /
    /// commit-retire) or the drain's own guard set, so entries never
    /// race. Folded over every inode read (monotone max — never regresses
    /// a fresher committed write); made durable by batched drain
    /// transactions on the flush cadence / fsync / unmount / cap.
    pending_times: scc::HashMap<Ino, (u64, u64)>,
    /// O(1) element count for the hot-path cap check + the stats gauge
    /// (`scc` `len()` walks buckets).
    pending_times_count: AtomicU64,
    /// Drain-task lifecycle: cap-crossing wake + join handle.
    times_drain_wake: Arc<squeezefs_ipc::sqz_notify::Notify>,
    times_drain_join: std::sync::Mutex<Option<squeezefs_ipc::sqz_channel::oneshot::Receiver<bool>>>,
    /// Mount-probe hardware classification (resolved OQ 2's second
    /// field), set once by the mount path.
    atomicity_physical: std::sync::OnceLock<crate::meta_backend::atomicity::AtomicityClass>,

    // ---- PR M1: the D0 single-writer mount guard (§5.0) ----
    /// Layer A lock carrier: a **dedicated, daemon-lifetime** fd holding
    /// `flock(LOCK_EX)` for the whole mount. Never registered with
    /// `uring_fs` (its `FdCache` evicts by LRU and would silently release
    /// a lock riding a cache fd), never read or written — released only
    /// at clean `shutdown` (after the final checkpoint) or at backend
    /// drop, which is when the kernel releases the lock (instant crash
    /// reclaim, no TTL). `None` on probe backends.
    guard_fd: std::sync::Mutex<Option<std::fs::File>>,
    /// This mount's writer identity (uuid) — the `writer_claim.id` and the
    /// input to the PR reservation key.
    writer_id: String,
    /// This boot's id (`/proc/sys/kernel/random/boot_id`) — scopes the
    /// same-host dead-pid proof.
    boot_id: String,
    /// Whether this backend committed a `writer_claim` (clean unmount
    /// deletes it exactly once).
    claimed: AtomicBool,
    /// DLM S2: this mount's durable writer term on THIS volume — the
    /// value committed to [`WRITER_TERM_XATTR`] + `WriterClaim.term` by
    /// the gate and published to the process fencing mint
    /// ([`crate::dlm::adopt_durable_term`]). `0` = the volume does not
    /// carry incompat bit 7 (era-less, pre-S2 behavior) or this is a
    /// probe / read-only backend that took no claim.
    writer_term: AtomicU64,
    /// Whether this volume carries
    /// [`super::superblock::FEATURE_INCOMPAT_KV_DURABLE_TERM`] (bit 7):
    /// the gate maintains the era ladder only then (mount NEVER stamps
    /// the bit — the batched reformat window does).
    durable_term_enabled: bool,
    /// Layer B1: the reservation client when the volume is a PR-capable
    /// namespace (`RESCAP ≠ 0` — enforcement grade); `None` on everything
    /// else (detection grade).
    reservations: Option<Arc<dyn crate::meta_backend::reservation::ReservationClient>>,
    /// Our 64-bit reservation key: `xxh3_64(writer_id ‖ boot_id)`.
    pr_key: u64,
    /// The host identity recorded at mount (§5.0 B1 pt 6 stability
    /// requirement); set once by the mount gate, compared by the
    /// heartbeat re-check.
    pr_identity: std::sync::OnceLock<crate::meta_backend::reservation::HostIdentity>,
    /// Whether we currently believe we hold the WE reservation (release
    /// exactly once at clean unmount).
    pr_active: AtomicBool,
    /// `writer_guard_fenced` (§9): usurpation-class fail-stops —
    /// reservation-conflict errno at a barrier, foreign holder at the
    /// heartbeat re-check, host-identity mismatch.
    guard_fenced: AtomicU64,
    /// `writer_guard_pr_reacquires` (§9): PTPL-lapse re-acquisitions.
    pr_reacquires: AtomicU64,
    /// Consecutive **barrier** failures (generic class). Deliberately NOT
    /// `journal_failures`: that counter is reset on every entry-write
    /// success (`commit_tx` step 6→7), which on the strict path runs
    /// immediately *before* the barrier — sharing it would erase the
    /// escalation each commit and consecutive failing barriers could
    /// never latch (the Issue-14 ordering trap this field exists for).
    /// Reset on barrier success; latches `failed` at
    /// [`JOURNAL_FAILURE_LATCH`].
    barrier_failures: AtomicU64,
    /// §4.7 wedged-tail audit rung (design-smo-replay-currency PR 4
    /// clause b, the `checkpoint_past` precedent — CENTRALIZED in
    /// `checkpoint_cycle` since the P2 2026-07-26 §9 cycle-break, so
    /// every barriered cycle rides it regardless of caller): bumped per
    /// barriered cycle that leaves retirements parked with neither a
    /// release nor a ledger-tail advance; reset on any progress; at
    /// `PENDING_FREE_FORCE_CYCLES` the volume fails loud — a genuinely
    /// wedged tail (one no flush pass can discharge — e.g. a stuck
    /// in-flight reservation) must present there, never as an unbounded
    /// retry loop. Persists across maintenance passes on purpose.
    pub(super) pending_free_stalled_cycles: AtomicU64,
    /// Guard-event trace of this backend's `open` (test/ops surface): the
    /// pinned order `flock_acquired` → `claim_committed` →
    /// `claim_barriered` → `checkpoint_task_spawned`.
    guard_trace: std::sync::Mutex<Vec<&'static str>>,
}

impl std::fmt::Debug for KvMetaBackend {
    /// Summarizes instead of deriving (the node-layer precedent): a
    /// backend embeds the whole node cache.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvMetaBackend")
            .field("path", &self.path)
            .field("ledger_seq", &self.ledger.seq)
            .field("next_ino", &self.next_ino.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Node-cache budget resolution, pure (2026-08-04 derivation sweep):
/// **absolute > percentage > derived** — the ipc-arena-cap precedence
/// pattern, a bad env string never fails a mount.
///
/// - `mb_env` (`SQUEEZEFS_META_NODE_CACHE_MB`, MiB): explicit wins
///   verbatim (the A0 lever — `512` restores the pre-sweep flat
///   default). Garbage warns and falls through.
/// - `pct_env` (`SQUEEZEFS_META_NODE_CACHE_PCT`, percent of the resolved
///   R5 budget): clamped into (0, 100]; garbage/non-positive warns and
///   falls through.
/// - Neither: `max(budget/16, 512 MiB)` — the fraction is scale-free
///   (the budget itself is machine-derived, §5.7 resolution order); the
///   floor is the shipped
///   [`super::node_cache::DEFAULT_CACHE_BUDGET_BYTES`] posture
///   (never-regress: every box ran 512 MiB before the sweep). Per
///   VOLUME, like the flat default it replaces — multi-meta-volume sets
///   multiply it (the pre-sweep behavior; eviction is the cache's own
///   clock).
pub fn resolve_node_cache_budget(
    budget_bytes: u64,
    mb_env: Option<&str>,
    pct_env: Option<&str>,
) -> u64 {
    if let Some(raw) = mb_env {
        match raw.trim().parse::<u64>() {
            Ok(mib) => return mib.saturating_mul(1024 * 1024),
            Err(e) => {
                log::warn!("{NODE_CACHE_MB_ENV}={raw:?} is not a MiB integer ({e}) — ignored")
            }
        }
    }
    if let Some(raw) = pct_env {
        match raw.trim().parse::<f64>() {
            Ok(p) if p.is_finite() && p > 0.0 => {
                let pct = if p > 100.0 {
                    log::warn!("SQUEEZEFS_META_NODE_CACHE_PCT={raw:?} > 100 — clamped to 100");
                    100.0
                } else {
                    p
                };
                return (budget_bytes as f64 * (pct / 100.0)) as u64;
            }
            Ok(p) => log::warn!(
                "SQUEEZEFS_META_NODE_CACHE_PCT={raw:?} must be a percent in (0, 100] \
                 (got {p}) — ignored"
            ),
            Err(e) => {
                log::warn!("SQUEEZEFS_META_NODE_CACHE_PCT={raw:?} is not a number ({e}) — ignored")
            }
        }
    }
    (budget_bytes / 16).max(super::node_cache::DEFAULT_CACHE_BUDGET_BYTES)
}

/// Resolve the node-cache budget knob (bytes) against the live R5
/// budget.
fn node_cache_budget_bytes() -> u64 {
    resolve_node_cache_budget(
        crate::mem_budget::MEM_BUDGET.resolve_budget_now(),
        std::env::var(NODE_CACHE_MB_ENV).ok().as_deref(),
        std::env::var("SQUEEZEFS_META_NODE_CACHE_PCT")
            .ok()
            .as_deref(),
    )
}

impl KvMetaBackend {
    /// Mount `path` per the module-docs sequence. Loud failures: bad/torn
    /// superblock, unknown incompat feature bits, no valid ledger slot,
    /// stale/corrupt tree roots, real device I/O errors. Journal-window
    /// tears recover and are counted, never loud (§4.1).
    ///
    /// **PR M1 — the D0 single-writer mount guard runs here**
    /// (design-metadata-throughput §5.0), in the pinned order:
    ///
    /// 1. **Layer A**: `flock(LOCK_EX | LOCK_NB)` on a dedicated
    ///    daemon-lifetime guard fd — refuse loud on `EWOULDBLOCK`
    ///    (same-host exclusivity; kernel-instant crash reclaim).
    /// 2. Bootstrap replay (the read-only, torn-tolerant `open_probe`
    ///    sequence) — the `writer_claim` evidence comes from this state.
    /// 3. **Layer B2** decision: fresh-foreign ⇒ refuse; same-host
    ///    dead-pid proof / own residue ⇒ reclaim; TTL-stale foreign on a
    ///    non-PR volume ⇒ refuse naming `squeezefs claim clear` (NO
    ///    automatic cross-host takeover without device enforcement).
    /// 4. **Layer B1** on `RESCAP`-capable namespaces: register + acquire
    ///    Write Exclusive; conflict arbitration (fresh ⇒ refuse,
    ///    TTL-stale ⇒ PREEMPT — safe because the device fences).
    /// 5. The claim tx is committed **and barriered** — the volume's
    ///    first post-replay mutation by construction — and only then is
    ///    `spawn_checkpoint_task` called (no maintenance record can
    ///    precede the claim) and the backend returned for FUSE arm.
    ///
    /// Returns `Arc<Self>` (PR K6b): the per-volume checkpoint/writeback
    /// task holds a `Weak` back-reference to the backend, so construction
    /// and task spawn are one step. The task rides the existing flusher
    /// cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS`, §4.6) and exits on
    /// [`Self::shutdown`] or when the backend is dropped (the v2 flusher's
    /// sentinel discipline — no leaked tasks).
    pub async fn open(path: &Path) -> std::result::Result<Arc<Self>, KvError> {
        // (1) Layer A first: kernel-arbitrated, cheapest, and the refusal
        // the measured incident (two same-host daemons) needs.
        let guard_fd = match Self::acquire_writer_flock(path) {
            Ok(fd) => fd,
            Err(FlockOutcome::Held) => {
                let holder = Self::probe_claim_best_effort(path).await;
                // Teardown-race absorption (2026-07-26): `drop`-without-
                // `shutdown` releases the flock only when the LAST Arc
                // dies, and the detached checkpoint / times-drain /
                // conveyor pass tasks each pin an upgraded Arc for the
                // duration of one pass — so an instant SAME-PROCESS
                // reopen (remount paths, crash-equivalent drop→reopen)
                // can find the flock held by a holder that is provably
                // in teardown. When the on-volume claim names THIS
                // process (pid + boot), wait the release out bounded; a
                // genuinely live same-process double mount never
                // releases and still refuses at the bound. Foreign
                // holders (any other pid/boot) never wait.
                match Self::await_same_process_teardown_flock(path, &holder).await {
                    Some(fd) => fd,
                    None => {
                        return Err(KvError::Busy(format!(
                            "{}: another squeezefs process holds the writer lock{} — concurrent \
                             mounts of one metadata volume are refused (single-writer guard)",
                            path.display(),
                            holder_suffix(&holder),
                        )));
                    }
                }
            }
            Err(FlockOutcome::Io(e)) => {
                return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
            }
        };

        // (1b) DUR-5 self-heal: a volume that mounts off the redundant
        // superblock copy gets its sector 0 rewritten HERE — under the
        // writer flock, on the write-mount path only (a read-only probe
        // must never write). A no-op (one 4 KiB read) when sector 0 is
        // healthy; loud when it is not.
        if let Err(e) = super::superblock::repair_primary_superblock(path).await {
            log::error!(
                "{}: superblock repair pass failed: {e} (continuing — the mount below \
                 refuses if the superblock is genuinely unusable)",
                path.display()
            );
        }

        // (2) Bootstrap replay (sets `boot_id` — shared with probes).
        let mut inner = Self::open_inner(path).await?;
        *inner.guard_fd.get_mut().unwrap() = Some(guard_fd);
        inner.writer_id = uuid::Uuid::new_v4().to_string();
        // Layer B1 resolution: test override first, then the real RESCAP
        // probe (control-plane ioctl — off the async runtime).
        let probe_path = path.to_path_buf();
        inner.reservations = squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::meta_backend::reservation::resolve_for_mount(&probe_path)
        })
        .await;
        inner.pr_key = xxhash_rust::xxh3::xxh3_64(
            format!("{}\u{0}{}", inner.writer_id, inner.boot_id).as_bytes(),
        );
        let be = Arc::new(inner);
        // PR M7: pass-task spawn identity — set before the FIRST commit
        // (the writer-claim tx below already rides the conveyor).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        be.trace_guard_event("flock_acquired");

        // (3)+(4)+(5) The claim gate: decision, PR acquisition, claim
        // commit + barrier. Any refusal returns Err with nothing spawned
        // except (possibly) a conveyor pass task for the claim commit —
        // no checkpoint/times-drain task exists, and PR registrations
        // are rolled back best-effort inside the gate.
        if let Err(e) = be.writer_guard_gate().await {
            be.release_reservation().await;
            // Layer A releases DETERMINISTICALLY before the error
            // returns (2026-07-27 torn-claim remount flake): dropping
            // the Arc is not enough — a failed claim COMMIT already
            // spawned the conveyor pass task, whose per-iteration
            // backend upgrade can outlive this scope by a scheduler
            // quantum and pin the flock against our caller's instant
            // remount, on a volume whose torn claim replays to nothing
            // (unattributable — the same-process absorption refuses).
            // Nothing of this mount writes after the gate refusal (the
            // batch failure's rollback + hole checkpoint ran inside the
            // pass, before its fan-out woke us), so releasing the lock
            // here is exactly the drop-order release, made synchronous.
            drop(be.guard_fd.lock().unwrap().take());
            return Err(e);
        }

        // (6) Cover the bring-up residue BEFORE the volume serves (and
        // before the cadence task exists — same inline-guarded-cycles
        // posture as `preclaim_ring_recovery`): the claim tx just
        // committed above must not sit committed-but-uncovered, or its
        // bytes become the D1.b wedge-crumb (see
        // [`Self::cover_bring_up_residue`]). A refusal tears down like a
        // gate refusal: the claim record stays (crash-equivalent — the
        // same-host dead-pid proof reclaims it instantly), the PR and
        // flock release deterministically.
        if let Err(e) = be.cover_bring_up_residue().await {
            be.release_reservation().await;
            drop(be.guard_fd.lock().unwrap().take());
            return Err(e);
        }

        super::checkpoint::spawn_checkpoint_task(&be);
        super::checkpoint::spawn_times_drain_task(&be);
        Ok(be)
    }

    /// Open for a **read-only probe** (the format-preflight guard and
    /// volume-status reads): the full mount bootstrap — SB → ledger →
    /// bitmap → RAM journal replay — but NO checkpoint/writeback task is
    /// spawned, so nothing is ever written. Probing a volume another
    /// process has live-mounted therefore cannot corrupt it. Dropping the
    /// returned backend releases everything (there is no task to join).
    pub async fn open_probe(path: &Path) -> std::result::Result<Arc<Self>, KvError> {
        let be = Arc::new(Self::open_inner(path).await?);
        // PR M7: probes never mutate, but the conveyor identity is part
        // of construction (a commit without it fails loud, never UB).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        Ok(be)
    }

    /// **DLM stage S5 — open one volume as a READER** (`-o ro` /
    /// `--read-only`; pre-RC engineering spec §6.8 item 1, §6.9 S5).
    ///
    /// §6.4 verified why this could not exist before: [`Self::open`] takes
    /// `flock(LOCK_EX | LOCK_NB)` **unconditionally, before any read/write
    /// classification**, and the only thing that ever set `read_only` was
    /// `sb.unknown_ro() != 0` — a forward-compatibility degradation, not a
    /// mount option. Cross-host, a would-be reader was additionally refused
    /// `FreshForeign` by the D0 Layer-B2 gate.
    ///
    /// What this open does, and what it deliberately does NOT do:
    ///
    /// | Step of [`Self::open`] | Reader |
    /// |---|---|
    /// | Layer A `flock(LOCK_EX)` | **not taken.** `flock(LOCK_SH)` runs as a released probe (classification only — `probe_shared_lock`) |
    /// | DUR-5 primary-superblock repair | **skipped** — it WRITES sector 0 |
    /// | Bootstrap replay (SB → ledger → bitmap → RAM journal replay) | same, verbatim (torn-tolerant, read-only by construction) |
    /// | Layer B2 claim classification (`FreshForeign` refusal) | **bypassed** — a reader cannot conflict with a writer it never writes against |
    /// | Layer B1 PR register + Write-Exclusive acquire | **not performed** — no registrant appears on the namespace |
    /// | `writer_claim` commit + barrier | **never written** |
    /// | checkpoint + times-drain tasks | **not spawned** (both write) |
    ///
    /// The result is a mount that is *orthogonal* to D0 rather than a hole
    /// in it: the writer's ladder is byte-identical, a second writer is
    /// still refused, and N readers coexist with each other and with the
    /// writer. Its guarantee class is its own row —
    /// [`Self::writer_guard_mode`] returns `"reader"`.
    ///
    /// **Consistency model** (stated, never implied — §6.12): this open
    /// arms nothing by itself — it bootstraps the mount's snapshot. The
    /// mount path then DECLARES the volume a coherent reader
    /// (`ro_coherence::arm_reader_coherence` →
    /// [`Self::arm_reader_revalidation`], installing the R-6 purge sink) and
    /// drives the derived cadence, after which the mount serves the state of
    /// the most recent checkpoint it has polled: bounded, monotone staleness
    /// with the bound published as `reader_staleness_bound_ms`. A consumer
    /// that opens read-only WITHOUT arming (an offline probe-shaped caller)
    /// keeps a frozen mount-time view, and its polls refuse loudly rather
    /// than pretending — [`Self::revalidate_reader`] requires the
    /// declaration. See `docs/operations.md` §Read-only coherent mounts.
    pub async fn open_read_only(path: &Path) -> std::result::Result<Arc<Self>, KvError> {
        match Self::probe_shared_lock(path) {
            SharedProbe::LocalExclusiveHolder => log::info!(
                "meta volume {}: read-only mount — a LOCAL exclusive holder (write mount or \
                 guarded offline verb) holds this volume; the reader takes no lock and \
                 refuses nothing",
                path.display()
            ),
            SharedProbe::NoLocalExclusiveHolder => log::info!(
                "meta volume {}: read-only mount — no local exclusive holder (the writer, if \
                 any, is on another host)",
                path.display()
            ),
            SharedProbe::Unknown => {}
        }
        let mut inner = Self::open_inner(path).await?;
        // The mount-option cause OVERRIDES the §4.11 one only in its
        // reporting: `read_only` is already true when unknown-ro bits are
        // present, and a reader is read-only either way.
        inner.read_only = true;
        inner.ro_cause = ReadOnlyCause::ReaderMount;
        let be = Arc::new(inner);
        // PR M7: no commit can ever run here, but the conveyor identity is
        // part of construction (a commit without it fails loud, never UB).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        be.trace_guard_event("reader_admitted");
        log::warn!(
            "meta volume {}: mounted READ-ONLY (DLM S5). No writer_claim, no NVMe \
             reservation, no checkpoint task — this mount cannot and will not write. \
             Guarantee class: {}",
            path.display(),
            be.writer_guard_mode()
        );
        Ok(be)
    }

    /// **DLM stage S9 — open one volume as a CO-WRITER**
    /// (`SQUEEZEFS_MW_ROLE=co-writer`; spec §6.2 item 7's consumer half,
    /// §6.9 S9; the ladder is [`crate::cowriter::classify_admission`]).
    ///
    /// This is the entry point S9 said was missing: *"the co-writer posture
    /// is unreachable from `main` until that gate admits a co-member of an
    /// ENGAGED claim set."* It is a **second door**, not a hole in the
    /// first: [`Self::open`]'s Layer-A flock, its `FreshForeign` refusal
    /// and its whole B1/B2 ladder are **byte-identical** to what they were,
    /// and a mount that has not passed the five-rung ladder cannot reach
    /// this function at all (a [`crate::cowriter::CoWriterAdmission`] is unforgeable —
    /// `classify_admission` is its only constructor).
    ///
    /// | Step of [`Self::open`] | Co-writer |
    /// |---|---|
    /// | Layer A `flock(LOCK_EX)` | **not taken.** `flock(LOCK_SH)` runs as a released probe (classification only) — a retained lock would deny the authority its `LOCK_EX` on a shared host, and two co-writer mounts on one host are legitimate |
    /// | DUR-5 primary-superblock repair | **skipped** — it WRITES sector 0, which is the authority's business |
    /// | Bootstrap replay (SB → ledger → bitmap → RAM journal replay) | same, verbatim (read-only by construction) |
    /// | Layer B2 claim classification | **not evaluated.** The authority's claim is EXPECTED to be there and live; the co-writer's own gate (rungs 2–4) is what decided that, off the durable claim SET and a live membership lease |
    /// | Layer B1 PR register + Write Exclusive acquire | **not performed** on the metadata namespace. The co-writer's device presence is a *registrant* under the DATA namespaces' standing WERO hold (rung 5) — never a metadata-namespace reservation, which would preempt the authority |
    /// | `writer_claim` commit + barrier | **never written** |
    /// | checkpoint + times-drain tasks | **not spawned** (both write) |
    ///
    /// What the mount path must still do (and does — `cowriter::arm`): arm
    /// the §6.8 item-2 revalidation cadence so this snapshot tracks the
    /// authority's checkpoints, install the ownership + publish + custody
    /// clients so mutations ship, and latch the accounting plane closed.
    ///
    /// A co-writer's staleness bound is a READER's bound
    /// (`reader_staleness_bound_ms`) for exactly the same reason: it serves
    /// the state of the most recent checkpoint it has polled. What it adds
    /// is that its own mutations are never stale, because they execute on
    /// the authority.
    pub async fn open_co_writer(
        path: &Path,
        admission: &crate::cowriter::CoWriterAdmission,
    ) -> std::result::Result<Arc<Self>, KvError> {
        if !admission.covers(path) {
            return Err(KvError::Corrupt(format!(
                "{}: refusing a co-writer open under an admission decided over a DIFFERENT \
                 volume set ({:?}). The ladder's evidence is per-set — bit 14 on every volume, \
                 one durable claim set naming this node, one authority — so an admission may \
                 never be carried across sets",
                path.display(),
                admission.volumes()
            )));
        }
        match Self::probe_shared_lock(path) {
            SharedProbe::LocalExclusiveHolder => log::info!(
                "meta volume {}: co-writer mount — a LOCAL exclusive holder (the authority's \
                 write mount, or a guarded offline verb) holds this volume on this host; the \
                 co-writer takes no lock and refuses nothing",
                path.display()
            ),
            SharedProbe::NoLocalExclusiveHolder => log::info!(
                "meta volume {}: co-writer mount — no local exclusive holder (the authority is \
                 on another host)",
                path.display()
            ),
            SharedProbe::Unknown => {}
        }
        let mut inner = Self::open_inner(path).await?;
        inner.read_only = true;
        inner.ro_cause = ReadOnlyCause::CoWriterMount;
        let be = Arc::new(inner);
        // PR M7: no local commit can ever run here, but the conveyor
        // identity is part of construction (a commit without it fails loud,
        // never UB).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        be.trace_guard_event("co_writer_admitted");
        log::warn!(
            "meta volume {}: mounted CO-WRITER (DLM S9) under authority claim '{}' (era {}). No \
             writer_claim, no metadata-namespace reservation, no checkpoint task — this mount \
             appends to none of this volume's single-appender structures and ships every \
             metadata mutation. Guarantee class: {}",
            path.display(),
            admission.authority_claim_id(),
            admission.authority_term(),
            be.writer_guard_mode()
        );
        Ok(be)
    }

    /// **Per-volume claim admission — open one volume a PEER authority of
    /// this set appends to** (`docs/design-per-volume-claim-admission.md`
    /// §5.1, PR 4; the decision is
    /// [`crate::partial_authority::classify_set_admission`]).
    ///
    /// The THIRD door beside [`Self::open`] and [`Self::open_co_writer`],
    /// and like the second it is a door rather than a hole: `open`'s
    /// Layer-A flock, its `FreshForeign` refusal and its whole B1/B2
    /// ladder are byte-identical to what they were, and a mount that has
    /// not passed the seven-rung ladder cannot reach this function at all
    /// (a [`crate::partial_authority::SetAdmission`] is unforgeable).
    ///
    /// | Step of [`Self::open`] | Peer-owned |
    /// |---|---|
    /// | Layer A `flock(LOCK_EX)` | **not taken** — a retained lock would deny the volume's OWNER its `LOCK_EX` on a shared host. The released `LOCK_SH` probe runs for classification only |
    /// | DUR-5 primary-superblock repair | **skipped** — it WRITES sector 0, which is the owner's business |
    /// | Bootstrap replay | same, verbatim (read-only by construction) |
    /// | Layer B2 claim classification | **evaluated, through the `PeerAuthority` arm**: the claim must be the ADMITTED holder's, resolved to a durable member id by the volume's own attestation. A fresh claim from anyone else, no claim at all, or a TTL-stale one all refuse (§5.1.1's `Peer` column) |
    /// | Layer B1 PR register + WEX acquire | **not performed** — a WEX acquire would preempt the owner |
    /// | `writer_claim` commit + barrier | **never written** |
    /// | checkpoint + times-drain tasks | **not spawned** (both write) |
    ///
    /// What the mount path must still do (PR 5): derive the ownership map
    /// so this volume's verbs SHIP, and arm revalidation over it so the
    /// snapshot tracks its owner's checkpoints (sweep row 15).
    pub async fn open_peer_owned(
        path: &Path,
        admission: &crate::partial_authority::SetAdmission,
        vol_id: &str,
    ) -> std::result::Result<Arc<Self>, KvError> {
        if !admission.covers_path(path) {
            return Err(KvError::Corrupt(format!(
                "{}: refusing a peer-owned open under an admission decided over a DIFFERENT \
                 volume set ({:?}). The ladder's evidence is per-set — bit 14 and a durable \
                 claim set on EVERY volume, one assignment map, one set authority — so an \
                 admission may never be carried across sets",
                path.display(),
                admission.volumes()
            )));
        }
        let admitted_holder = match admission.mode_for(vol_id) {
            Some(crate::partial_authority::VolumeMode::Peer { owner_id, .. }) => owner_id.clone(),
            Some(crate::partial_authority::VolumeMode::Own) => {
                return Err(KvError::Corrupt(format!(
                    "{}: the admission decided this volume is this node's OWN — it must run \
                     the full D0 ladder (Layer A + B1 + the claim commit + the checkpoint \
                     task), never the peer door, or the set would have a volume no node \
                     appends to",
                    path.display()
                )));
            }
            None => {
                return Err(KvError::Corrupt(format!(
                    "{}: the admission does not name volume {vol_id}. A volume a decision \
                     does not cover is a REFUSAL, never a default: opening it either way \
                     would be an ownership guess",
                    path.display()
                )));
            }
        };
        match Self::probe_shared_lock(path) {
            SharedProbe::LocalExclusiveHolder => log::info!(
                "meta volume {}: peer-owned open — a LOCAL exclusive holder (its owner's write \
                 mount on this host, or a guarded offline verb) holds this volume; this mount \
                 takes no lock and refuses nothing",
                path.display()
            ),
            SharedProbe::NoLocalExclusiveHolder => log::info!(
                "meta volume {}: peer-owned open — no local exclusive holder (its owner is on \
                 another host)",
                path.display()
            ),
            SharedProbe::Unknown => {}
        }
        let mut inner = Self::open_inner(path).await?;
        inner.read_only = true;
        inner.ro_cause = ReadOnlyCause::PeerOwnedVolume;
        let be = Arc::new(inner);
        // PR M7: no local commit can ever run here, but the conveyor
        // identity is part of construction (a commit without it fails
        // loud, never UB).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));

        // Layer B2, through the per-volume arm: the claim on this volume
        // must belong to the durable identity the admission admitted.
        let raw = be
            .getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .map_err(KvError::Io)?;
        let set = crate::membership::ClaimSet::load(&be).await;
        let witness = PeerAuthorityWitness {
            admitted_holder: &admitted_holder,
            claim_set: set.as_ref().filter(|s| s.durable),
        };
        match be.classify_claim(raw, unix_now_secs(), Some(&witness)) {
            ClaimEvidence::PeerAuthority(claim) => {
                be.trace_guard_event("peer_owned_admitted");
                log::warn!(
                    "meta volume {} ({vol_id}): mounted PEER-OWNED — '{admitted_holder}' \
                     appends to it under writer_claim '{}' (era {}). No flock, no \
                     writer_claim, no metadata-namespace reservation, no checkpoint task: \
                     this mount holds metadata authority over the volumes it OWNS and ships \
                     every mutation of this one. Guarantee class: {}",
                    path.display(),
                    claim.id,
                    claim.term,
                    be.writer_guard_mode()
                );
                Ok(be)
            }
            ClaimEvidence::FreshForeign(claim) => {
                let seen = set
                    .as_ref()
                    .and_then(|s| s.resolve_holder(&claim))
                    .map(str::to_string);
                Err(KvError::Busy(match seen {
                    Some(seen) => format!(
                        "{} ({vol_id}): the live writer_claim is held by '{seen}', but this \
                         mount's admission decided '{admitted_holder}' appends here. \
                         Assignment and evidence DISAGREE, so the ownership map fails closed \
                         (§5.10): shipping this volume's verbs to a node the record does not \
                         entitle would make it an appender nobody assigned. Re-assign offline \
                         (`squeezefs volume set-owners`), or stop the holder",
                        path.display()
                    ),
                    None => format!(
                        "{} ({vol_id}): the live writer_claim (id={}, pid={}, boot={}) could \
                         not be resolved to a durable member id — the claim's own id is a \
                         per-mount uuid and this volume's claim_set carries no matching \
                         holder attestation. An unresolvable holder is SILENCE, and ownership \
                         never moves on silence (KD-PV-3): this mount would be shipping every \
                         verb for the volume to a node it cannot name. Remount its owner \
                         (which attests itself at its own open), or re-assign offline",
                        path.display(),
                        claim.id,
                        claim.pid,
                        claim.boot
                    ),
                }))
            }
            ClaimEvidence::Reclaimable => {
                crate::fuse_client::METRICS
                    .peer_volume_unclaimed_refusals
                    .fetch_add(1, Ordering::Relaxed);
                Err(KvError::Busy(format!(
                    "{} ({vol_id}): the assignment names peer '{admitted_holder}' as this \
                     volume's owner, but NOTHING claims it — that owner is dead, was never \
                     started, or the assignment is stale. This mount is not the assignee, so \
                     it must not take the claim; and it must not serve a set with a volume no \
                     node appends to. Start the owner, or re-assign offline with `squeezefs \
                     volume set-owners` (`squeezefs volume get-owners` prints assignment \
                     beside evidence)",
                    path.display()
                )))
            }
            ClaimEvidence::StaleForeign(claim) => {
                crate::fuse_client::METRICS
                    .peer_volume_unclaimed_refusals
                    .fetch_add(1, Ordering::Relaxed);
                Err(KvError::Busy(format!(
                    "{} ({vol_id}): the assigned owner '{admitted_holder}' left a TTL-stale \
                     writer_claim{}. A partial writer must NEVER preempt a peer's claim — \
                     preempting would make it the appender of a volume it is not assigned, \
                     which is the one thing per-volume admission exists to prevent. Start \
                     that owner, or re-assign the volume offline (`squeezefs volume \
                     set-owners`)",
                    path.display(),
                    holder_suffix(&claim.map(|c| (c, unix_now_secs())))
                )))
            }
        }
    }

    /// The durable per-volume identity every per-volume admission mode is
    /// keyed on (KD-5: *never a path, an ordinal, or a set position*).
    ///
    /// Derived from this volume's **superblock uuid** — the sole durable
    /// per-volume identity a mount can read before any tree is routed
    /// (AGENTS.md §Filesystem generation identity; the `meta_volumes`
    /// config record is a documented MIRROR and is absent or synthesized
    /// on sets the lifecycle verbs never touched). Rendered in the house
    /// `vol-{16 hex}` style so operator output is one shape.
    pub fn durable_volume_id(&self) -> String {
        format!("vol-{:016x}", xxhash_rust::xxh3::xxh3_64(&self.sb.uuid))
    }

    /// Whether this volume withholds mutations, and why.
    pub fn read_only_cause(&self) -> ReadOnlyCause {
        self.ro_cause
    }

    /// Whether this volume withholds every mutation (a reader mount, or the
    /// §4.11 unknown-ro-feature-bits degradation).
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn open_inner(path: &Path) -> std::result::Result<Self, KvError> {
        let t0 = std::time::Instant::now();

        // 1. Superblock (the version gate is the loud unit).
        let sb = match classify_volume(path).await? {
            VolumeFormat::V3(sb) => sb,
            VolumeFormat::Blank => {
                return Err(KvError::Corrupt(format!(
                    "{} is not formatted (zeroed superblock) — run `squeezefs format` first",
                    path.display()
                )))
            }
            VolumeFormat::V2Legacy => {
                return Err(KvError::Corrupt(format!(
                    "{} is a legacy format-v2 volume — v2 support was removed; reformat required",
                    path.display()
                )))
            }
        };
        if sb.unknown_ro() != 0 {
            // §4.11: read-only feature bits from a future format. K6a's
            // surface is read-only by construction; K6b's write path must
            // re-check this mask and withhold mutations.
            log::warn!(
                "meta volume {}: unknown read-only feature bits {:#x} — mounting read-only",
                path.display(),
                sb.unknown_ro()
            );
        }

        // 2. Root ledger: newest valid slot; all-slots-invalid is LOUD on
        // a v3-superblocked volume (§4.1's loud list — format always
        // writes a bootstrap record, so nothing-valid is corruption, not
        // freshness).
        let ledger = read_newest_ledger(path, sb.root_ledger.start)
            .await?
            .ok_or_else(|| {
                KvError::Corrupt(format!(
                    "{}: no valid root-ledger record in any of the 32 slots — the volume \
                     carries a v3 superblock, so this is corruption, not a fresh format",
                    path.display()
                ))
            })?;

        // 3+4a. Journal recovery: one sequential ring read, §4.1 tear
        // semantics (never loud for ring contents). K6b stores the ring:
        // it is the §4.4 admission/reservation core for every commit and
        // the checkpoint task's reclamation watermark.
        let (ring, recovery) = JournalRing::recover(
            path,
            sb.journal.start,
            sb.journal_pages(),
            checkpoint_reserve_bytes(sb.journal.len),
            ledger.journal_tail_seq,
        )
        .await?;
        let ring = Arc::new(ring);

        // 4b. Allocator: newest-valid A/B pages + replayed deltas (§4.7).
        let total_extents = sb.total_extents();
        let pending_cap = match TEST_PENDING_FREE_CAP.load(Ordering::Relaxed) {
            0 => PENDING_FREE_CAP,
            n => n as usize,
        };
        let alloc = Arc::new(
            ExtentAllocator::load(
                path,
                sb.alloc_bitmap.start,
                total_extents,
                compaction_reserve_extents(total_extents),
                pending_cap,
                ledger.journal_tail_seq,
                &recovery.entries,
            )
            .await?,
        );

        // 5a. Node cache + trees from the ledger roots.
        let layout = NodeLayout::new(sb.node_size as usize)?;
        let cache = NodeCache::new(NodeCacheConfig {
            path: path.to_path_buf(),
            layout,
            heap_base: sb.heap.start,
            budget_bytes: node_cache_budget_bytes(),
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        cache.set_durable_tail(ledger.journal_tail_seq);
        // Node-seq mint floor: the persisted watermark keeps mints
        // strictly above every seq ever stamped into a frame this
        // generation (Finding A — re-minted seqs made recycled-extent
        // residue admissible). The root/replay fetch_max floors below
        // stay as the crash-window belt-and-braces.
        let seq = Arc::new(AtomicU64::new(ledger.seq.max(ledger.node_seq_watermark)));
        let mut opened: Vec<KvTree> = Vec::with_capacity(3);
        for tree_id in [TREE_INODES, TREE_DENTRIES, TREE_XATTRS] {
            let root = ledger
                .tree_roots
                .iter()
                .find(|r| r.tree_id == tree_id)
                .ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "{}: ledger record seq {} names no root for tree {tree_id}",
                        path.display(),
                        ledger.seq
                    ))
                })?;
            let tree = KvTree::open(
                cache.clone(),
                tree_id,
                RootPtr {
                    addr: root.node_addr,
                    seq: root.node_seq,
                },
                seq.clone(),
            )
            .await?;
            // Post-replay seq assignment stays above every node seq the
            // roots carry.
            seq.fetch_max(root.node_seq, Ordering::AcqRel);
            opened.push(tree);
        }
        let mut opened = opened.into_iter();
        let (inodes, dentries, xattrs) = (
            opened.next().expect("three trees"),
            opened.next().expect("three trees"),
            opened.next().expect("three trees"),
        );

        // 5a′. Spec §6.2 item 1 (incompat bit 8): the durable
        // block-reference tree. Fresh formats carry its (empty) root from
        // the builder; a volume STAMPED later (the Phase-8 reformat
        // window) has the bit and no root, so the first writable mount
        // mints one. Minting is idempotent across a crash: the claimed
        // extent's bitmap bit only becomes durable at a checkpoint, so a
        // mount that dies before its first checkpoint leaves nothing
        // behind and the next one mints again. The mint happens BEFORE
        // replay, so any accounting record still in the journal window
        // folds into the fresh root by key, exactly like every other
        // content record.
        //
        // A read-only mount (unknown-ro feature bits) never mints and
        // never accounts — it degrades to the derived walk, which is the
        // honest behavior for a mount that may not write.
        let block_refs = if sb.features_incompat
            & super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS
            != 0
            // The `read_only` latch (unknown-ro feature bits) is resolved
            // further down; its input is the superblock, so read it here.
            && sb.unknown_ro() == 0
        {
            match ledger
                .tree_roots
                .iter()
                .find(|r| r.tree_id == super::record::TREE_BLOCK_REFS)
            {
                Some(root) => {
                    let tree = KvTree::open(
                        cache.clone(),
                        super::record::TREE_BLOCK_REFS,
                        RootPtr {
                            addr: root.node_addr,
                            seq: root.node_seq,
                        },
                        seq.clone(),
                    )
                    .await?;
                    seq.fetch_max(root.node_seq, Ordering::AcqRel);
                    Some(tree)
                }
                None => {
                    log::info!(
                        "meta volume {}: incompat bit 8 (durable block refcounts) is \
                         stamped but the ledger names no block-reference root — minting \
                         an empty one (the post-stamp first mount)",
                        path.display()
                    );
                    let mut mint_ctx = SmoContext::new(alloc.clone());
                    Some(
                        KvTree::create(
                            cache.clone(),
                            &mut mint_ctx,
                            super::record::TREE_BLOCK_REFS,
                            seq.clone(),
                        )
                        .await?,
                    )
                }
            }
        } else {
            None
        };

        // 5b. Read-only replay into the cache, TWO-PHASE (Option C′,
        // docs/design-smo-replay-currency.md §2/§4): routing must not
        // evolve UNDER the content walk. Single-pass seq-order replay
        // routed each content record through the structure *as it stood
        // at that entry* — a record whose seq races an SMO's build window
        // (reserved before the in-lock flip reservation) descended the
        // pre-flip route into the predecessor, and the higher-seq flip
        // then abandoned that lineage: acked, in-window, replayed
        // "cleanly", and lost (sub-mechanism (i) stranding — the
        // FIND-VS-A 0.4–0.9 % acked-create residual).
        //
        // Phase 1 — every interior/routing record first, ordered by
        // (level DESC, then seq): upper flips route lower ones (a leaf-
        // SMO flip is itself "content" to the interior it applies to —
        // pure-seq phase 1 would strand it exactly as (i) strands leaf
        // content). Per-key LWW by seq is unchanged, so a flip already
        // folded into a successor interior's image re-applies idempotent;
        // an unroutable pointer (the mounted ledger predates a root
        // growth) still drops sound-and-silent. Root swaps journal no
        // pointer records — those windows replay through the old
        // structure by design (the C′ carve-out; Option A owns them).
        //
        // Phase 2 — content records (original seqs, unchanged per-key
        // LWW gate) through the now-FINAL routing: every record folds
        // into the node covering its key in the final structure, whose
        // durable image the SMO barriered before its flip could exist.
        //
        // §4.4 pt 4 holes stay dropped in both phases by construction:
        // each phase walks the SAME `recovery.entries` the checksummed
        // chain scan materialized — a rolled-back tx's reserved-but-
        // unwritten range never parses into it, so neither phase can see
        // half a tx (one tx = one checksummed entry, §4.10). Replay-twice
        // digest equality holds over the phased order: replay is
        // read-only into the cache and (level DESC, seq) over the same
        // materialized entries is a deterministic total order. Allocator
        // records were already consumed by the K4 load, untouched here.
        //
        // Stranded predecessor *objects* can remain mapped-but-unrouted
        // in the cache until clock eviction — bytes only, bounded by the
        // window's SMO count (design §2 C′ residuals).
        // Each replayed record's floor contribution is its ENTRY start
        // (`ReplayedEntry::seq` — FIND-SMO-TAIL §1b rounding): replay
        // reproduces record seqs, so it must reproduce the floor
        // discipline too, or a post-replay checkpoint could re-mint a
        // mid-entry tail from the replayed window's own records.
        let mut interior: Vec<(u8, u8, u64, &Record)> = Vec::new();
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                let mounted = matches!(tree_id, TREE_INODES | TREE_DENTRIES | TREE_XATTRS)
                    || (tree_id == super::record::TREE_BLOCK_REFS && block_refs.is_some());
                if level > 0 && mounted {
                    interior.push((tree_id, level, entry.seq, rec));
                }
            }
        }
        interior.sort_by(|a, b| b.1.cmp(&a.1).then(a.3.seq.cmp(&b.3.seq)));
        for (tree_id, level, entry_start, rec) in interior {
            let tree = match tree_id {
                TREE_INODES => &inodes,
                TREE_DENTRIES => &dentries,
                TREE_XATTRS => &xattrs,
                super::record::TREE_BLOCK_REFS => block_refs
                    .as_ref()
                    .expect("phase 1 collects the block-ref tree only when it is mounted"),
                _ => unreachable!("phase 1 collects only the mounted trees"),
            };
            // Keep post-mount node-seq mints above every child
            // incarnation a replayed pointer names.
            if rec.kind == RecordKind::Put {
                if let Ok((_addr, child_seq)) = decode_interior_value(&rec.value) {
                    seq.fetch_max(child_seq, Ordering::AcqRel);
                }
            }
            tree.apply_replayed_interior(
                &rec.key,
                level,
                rec.seq,
                rec.kind,
                Bytes::copy_from_slice(&rec.value),
                entry_start,
            )
            .await?;
        }
        let mut max_replayed_ino: u64 = 0;
        // PR VL5b: per-guest-slot replayed ino maxima — the same §4.8
        // recovery fold, one cursor per guest namespace.
        let mut max_replayed_guest: std::collections::HashMap<u16, u64> =
            std::collections::HashMap::new();
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if level > 0 {
                    continue; // phase 1 applied it
                }
                let tree = match tree_id {
                    TREE_INODES => &inodes,
                    TREE_DENTRIES => &dentries,
                    TREE_XATTRS => &xattrs,
                    // Spec §6.2 item 1: accounting records replay like
                    // any other content record — routed by key into the
                    // (possibly freshly minted) block-ref root. An
                    // un-engaged volume cannot have them; a stamped
                    // volume whose mint raced a crash folds them into the
                    // new root by key.
                    super::record::TREE_BLOCK_REFS => match block_refs.as_ref() {
                        Some(t) => t,
                        None => continue,
                    },
                    TREE_ALLOC_RESERVED => continue,
                    _ => continue,
                };
                // §4.8 recovery fold — **DUR-8c**: every ino the replay
                // window MENTIONS raises the watermark, not just the ones
                // with a surviving inode record. A torn-dropped create
                // whose dentry (child ino in the VALUE) or xattr (ino in
                // the KEY) survived would otherwise let `next_ino` fall
                // back and RE-MINT that ino over the survivor.
                let mentioned: Option<u64> = match tree_id {
                    TREE_INODES => decode_inode_key(&rec.key).ok(),
                    TREE_DENTRIES => super::record::DentryValue::decode(&rec.value)
                        .ok()
                        .map(|d| d.child_ino),
                    TREE_XATTRS => super::record::decode_xattr_key(&rec.key)
                        .ok()
                        .map(|(ino, _, _)| ino),
                    _ => None,
                };
                if let Some(ino) = mentioned {
                    match crate::meta_backend::split_guest_local(ino) {
                        Some((slot, raw)) => {
                            let e = max_replayed_guest.entry(slot).or_insert(0);
                            *e = (*e).max(raw);
                        }
                        None => max_replayed_ino = max_replayed_ino.max(ino),
                    }
                }
                tree.apply_replayed(
                    &rec.key,
                    rec.seq,
                    rec.kind,
                    Bytes::copy_from_slice(&rec.value),
                    entry.seq,
                )
                .await?;
            }
        }

        // 5c. §4.8: next_ino = max(ledger watermark, replayed inos + 1).
        let next_ino = ledger.next_ino.max(max_replayed_ino + 1);

        let replay = KvReplayStats {
            entries: recovery.entries.len() as u64,
            dropped_torn: recovery.dropped_torn,
            replay_ms: t0.elapsed().as_millis() as u64,
        };

        // K6b wiring: strict/deferred mode, the SMO context with the
        // production journal hooks, and the checkpoint-state scaffolding
        // the background task drives.
        //
        // PR M7 liveness clamp (§4.4 pt 5 shape, batch edition): a batch's
        // one Σ-admission must always be satisfiable once the drain
        // catches up, so the byte cap never exceeds the ring's
        // user-admissible capacity (every individual entry ≤ the 128 KiB
        // whole-entry cap already fits by the ring-size floor).
        let batch_max_bytes = {
            let user_capacity = (sb.journal_pages() * super::journal::JOURNAL_PAGE_DATA_LEN)
                .saturating_sub(checkpoint_reserve_bytes(sb.journal.len));
            resolve_commit_batch_bytes(
                std::env::var(COMMIT_BATCH_BYTES_ENV).ok().as_deref(),
                user_capacity,
            )
            .min(user_capacity.max(1))
        };
        let strict = crate::meta_backend::resolve_flush_interval_ms() == 0;
        let read_only = sb.unknown_ro() != 0;
        // DLM S5: `open_read_only` re-stamps this to `ReaderMount`. Here it
        // can only be the §4.11 degradation (or writable).
        let ro_cause = if read_only {
            ReadOnlyCause::UnknownRoFeatureBits
        } else {
            ReadOnlyCause::Writable
        };
        let layout_deltas_stamped =
            sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_LAYOUT_DELTAS != 0;
        // DLM S2 (bit 7, presence OPTIONAL): un-stamped volumes keep the
        // pre-S2 era-less behavior verbatim.
        let durable_term_enabled =
            sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_DURABLE_TERM != 0;
        let sync = Arc::new(SyncCoalescer::new());
        let retire_seq = Arc::new(AtomicU64::new(ledger.seq + 1));
        // Resolved once here (before `sb` moves into the struct): the
        // checkpoint task reads this on every cadence tick.
        let dirty_node_cap = super::checkpoint::resolve_max_dirty_nodes(
            crate::mem_budget::MEM_BUDGET.resolve_budget_now(),
            u64::from(sb.node_size),
            std::env::var(super::checkpoint::CHECKPOINT_MAX_DIRTY_NODES_ENV)
                .ok()
                .as_deref(),
        );
        let smo = crate::sqz_sync::SqzMutex::new(SmoContext::with_journal(
            alloc.clone(),
            SmoJournal {
                ring: ring.clone(),
                retire_seq: retire_seq.clone(),
                sync: sync.clone(),
                path: path.to_path_buf(),
            },
        ));
        let be = Self {
            path: path.to_path_buf(),
            sb,
            checkpoint_seq: AtomicU64::new(ledger.seq),
            last_ledger_tail: AtomicU64::new(ledger.journal_tail_seq),
            // PR VL5a (§5.5.1a): seed the live stamp from the mounted
            // record — every checkpoint re-writes it, so a slot-mapped
            // volume's newest ledger slot always carries its membership.
            membership_stamp: std::sync::Mutex::new(ledger.membership_stamp.clone()),
            guest_cursors: scc::HashMap::new(),
            migration_tee: arc_swap::ArcSwapOption::empty(),
            ledger,
            inodes,
            dentries,
            xattrs,
            block_refs,
            alloc,
            next_ino: AtomicU64::new(next_ino),
            era_ino_floor_native: next_ino,
            era_ino_floor_guest: std::sync::OnceLock::new(),
            lane_cursors: scc::HashMap::new(),
            lanes_live: AtomicBool::new(false),
            destroyed_inodes: AtomicU64::new(0),
            replay,
            ring,
            dlm: DlmLockManager::new(),
            sync,
            cache,
            strict,
            needs_flush: AtomicBool::new(false),
            read_only,
            ro_cause,
            failed: AtomicBool::new(false),
            journal_failures: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            layout_deltas_ok: AtomicBool::new(layout_deltas_stamped),
            layout_delta_ratchet: crate::sqz_sync::SqzMutex::new(()),
            conveyor: Arc::new(ConveyorCore::new()),
            conveyor_self: std::sync::OnceLock::new(),
            layout_conveyor: Arc::new(ConveyorCore::new()),
            batch_max_txs: resolve_commit_batch_txs(
                std::env::var(COMMIT_BATCH_TXS_ENV).ok().as_deref(),
                crate::cpu::process_parallelism(),
            ),
            batch_max_bytes,
            dirty_node_cap,
            timeout_threshold: squeezefs_timeout_env(),
            smo,
            retire_seq,
            pending_reclaim: std::sync::Mutex::new(Vec::new()),
            barrier_starts: AtomicU64::new(0),
            barrier_durable: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            ckpt_wake: Arc::new(squeezefs_ipc::sqz_notify::Notify::new()),
            ckpt_join: std::sync::Mutex::new(None),
            ckpt_alive: std::sync::Mutex::new(Weak::new()),
            pending_times: scc::HashMap::new(),
            pending_times_count: AtomicU64::new(0),
            times_drain_wake: Arc::new(squeezefs_ipc::sqz_notify::Notify::new()),
            times_drain_join: std::sync::Mutex::new(None),
            atomicity_physical: std::sync::OnceLock::new(),
            guard_fd: std::sync::Mutex::new(None),
            writer_id: String::new(),
            // This boot's id — needed by write mounts (the claim gate's
            // same-host dead-pid proof) AND probes (`mount_registrations`
            // classifies claim records with the same proof).
            boot_id: read_boot_id(),
            claimed: AtomicBool::new(false),
            writer_term: AtomicU64::new(0),
            durable_term_enabled,
            reservations: None,
            pr_key: 0,
            pr_identity: std::sync::OnceLock::new(),
            pr_active: AtomicBool::new(false),
            guard_fenced: AtomicU64::new(0),
            pr_reacquires: AtomicU64::new(0),
            barrier_failures: AtomicU64::new(0),
            pending_free_stalled_cycles: AtomicU64::new(0),
            guard_trace: std::sync::Mutex::new(Vec::new()),
        };

        // PR VL5b: seed the per-slot guest cursors — the mounted stamp's
        // travelling cursors folded with the replayed per-slot maxima
        // (§4.8's `max(ledger watermark, replayed + 1)` rule, per slot).
        if let Some(stamp) = be.membership_stamp.lock().unwrap().as_ref() {
            for (slot, next) in &stamp.slot_cursors {
                let floor = (*next).max(max_replayed_guest.get(slot).map(|m| m + 1).unwrap_or(2));
                let _ = be.guest_cursors.insert_sync(
                    *slot,
                    Arc::new(super::slot_cursor_core::SlotCursor::new(floor)),
                );
            }
        }
        // Replayed guest records for a slot the stamp carries no cursor
        // for (crash between the guest commit and the next checkpoint's
        // extended stamp): the replay fold is authoritative.
        for (slot, max_raw) in &max_replayed_guest {
            match be.guest_cursors.read_sync(slot, |_, v| v.clone()) {
                Some(c) => c.install_floor(max_raw + 1),
                None => {
                    let _ = be.guest_cursors.insert_sync(
                        *slot,
                        Arc::new(super::slot_cursor_core::SlotCursor::new(max_raw + 1)),
                    );
                }
            }
        }
        // The writer era's per-guest-keyspace ino floors — captured HERE,
        // after seeding and before this mount can mint, so every record
        // that already exists is strictly below its keyspace's floor (see
        // the field docs + [`Self::minted_in_prior_era`]).
        let mut guest_floors = std::collections::HashMap::new();
        be.guest_cursors.iter_sync(|slot, cursor| {
            guest_floors.insert(*slot, cursor.snapshot());
            true
        });
        let _ = be.era_ino_floor_guest.set(guest_floors);
        Ok(be)
    }

    /// The mounted superblock.
    pub fn superblock(&self) -> &SuperblockV3 {
        &self.sb
    }

    /// The dirty-node checkpoint cap resolved at open (see the field
    /// docs: per-tick re-resolution is an op-economy violation).
    pub(crate) fn dirty_node_cap(&self) -> u64 {
        self.dirty_node_cap
    }

    /// The ledger record this mount selected (newest valid).
    pub fn mounted_ledger(&self) -> &LedgerRecord {
        &self.ledger
    }

    /// The live §5.5.1a membership stamp this volume's checkpoints carry
    /// (PR VL5a); `None` on legacy volumes.
    pub fn membership_stamp(&self) -> Option<super::checkpoint::MembershipStamp> {
        self.membership_stamp.lock().unwrap().clone()
    }

    /// Install/replace the §5.5.1a membership stamp — the `repair-set`
    /// re-stamp surface. Callers must have barriered
    /// [`super::superblock::FEATURE_INCOMPAT_KV_GUEST_SLOTS`] onto this
    /// volume FIRST (the bit-before-first-stamp invariant); the stamp
    /// lands durably with the next checkpoint's ledger write (shutdown's
    /// final cycle at the latest).
    pub fn set_membership_stamp(&self, stamp: super::checkpoint::MembershipStamp) {
        // Cursors named by the stamp become live cells (idempotent —
        // install_floor never regresses a fresher mint).
        for (slot, next) in &stamp.slot_cursors {
            match self.guest_cursors.read_sync(slot, |_, v| v.clone()) {
                Some(c) => c.install_floor(*next),
                None => {
                    let _ = self.guest_cursors.insert_sync(
                        *slot,
                        Arc::new(super::slot_cursor_core::SlotCursor::new(*next)),
                    );
                }
            }
        }
        *self.membership_stamp.lock().unwrap() = Some(stamp);
    }

    /// PR VL5b: the stamp image a checkpoint's ledger record carries —
    /// the stored stamp with the LIVE per-slot guest cursors folded in
    /// (the loom-modeled `slot_cursor_core` publication edge: every mint
    /// whose record the flush pass covered is strictly below its
    /// published cursor).
    pub(super) fn membership_stamp_for_ledger(&self) -> Option<super::checkpoint::MembershipStamp> {
        let mut stamp = self.membership_stamp.lock().unwrap().clone()?;
        let mut cursors: Vec<(u16, u64)> = Vec::new();
        self.guest_cursors.iter_sync(|slot, cursor| {
            cursors.push((*slot, cursor.snapshot()));
            true
        });
        cursors.sort_unstable_by_key(|(s, _)| *s);
        if !cursors.is_empty() {
            stamp.slot_cursors = cursors;
        }
        Some(stamp)
    }

    /// PR VL5b: mint one GUEST local ino for hosted slot `slot` (raw —
    /// the caller namespaces it with `guest_local_ino`). Fails loud on a
    /// slot this volume carries no cursor for (routing bug, never UB).
    pub fn allocate_guest_ino(&self, slot: u16) -> Result<Ino> {
        if let Some(c) = self.guest_cursors.read_sync(&slot, |_, v| v.clone()) {
            return Ok(c.mint());
        }
        // VIRGIN guest slot (hosted since format, never migrated, never
        // minted — the identity distribution's over-provisioned slots):
        // its keyspace is empty by construction (only travelling cursors
        // or replay maxima make records; both install cursors), so a
        // fresh cursor at 2 is exact. Lazy-created here; the next
        // checkpoint's extended stamp publishes it.
        let fresh = Arc::new(super::slot_cursor_core::SlotCursor::new(2));
        let c = match self.guest_cursors.insert_sync(slot, fresh.clone()) {
            Ok(()) => fresh,
            Err(_) => self
                .guest_cursors
                .read_sync(&slot, |_, v| v.clone())
                .ok_or_else(|| self.eio("guest cursor raced out (impossible)"))?,
        };
        Ok(c.mint())
    }

    /// Reserve `n` consecutive GUEST locals for hosted slot `slot` (the
    /// [`Self::reserve_ino_range`] twin — rung 13's intent supply; same
    /// lazy virgin-slot creation as [`Self::allocate_guest_ino`]).
    pub fn reserve_guest_ino_range(&self, slot: u16, n: u64) -> Result<Ino> {
        if let Some(c) = self.guest_cursors.read_sync(&slot, |_, v| v.clone()) {
            return Ok(c.mint_range(n));
        }
        let fresh = Arc::new(super::slot_cursor_core::SlotCursor::new(2));
        let c = match self.guest_cursors.insert_sync(slot, fresh.clone()) {
            Ok(()) => fresh,
            Err(_) => self
                .guest_cursors
                .read_sync(&slot, |_, v| v.clone())
                .ok_or_else(|| self.eio("guest cursor raced out (impossible)"))?,
        };
        Ok(c.mint_range(n))
    }

    /// LIVE guest-cursor count (mint-spread + travelled cursors — what
    /// the next checkpoint's stamp will carry): the
    /// `meta_slot_stamp_cursors_max` encoding-budget pressure gauge's
    /// input (design-dynamic-meta-routing §5.8).
    pub fn guest_cursor_count(&self) -> usize {
        self.guest_cursors.len()
    }

    /// PR VL5b: the current cursor snapshot for `slot` (the migration
    /// engine reads the SOURCE's travelling cursor at cutover). `None` =
    /// no guest cursor — the slot's keyspace is this volume's legacy one
    /// and the volume `next_ino` watermark is its cursor.
    pub fn guest_cursor_snapshot(&self, slot: u16) -> Option<u64> {
        self.guest_cursors
            .read_sync(&slot, |_, v| v.clone())
            .map(|c| c.snapshot())
    }

    /// PR VL5b: install (or raise) the travelling cursor for `slot` —
    /// the migration flip's target-side step. Idempotent and monotonic.
    pub fn install_guest_cursor(&self, slot: u16, next: u64) {
        match self.guest_cursors.read_sync(&slot, |_, v| v.clone()) {
            Some(c) => c.install_floor(next),
            None => {
                let _ = self.guest_cursors.insert_sync(
                    slot,
                    Arc::new(super::slot_cursor_core::SlotCursor::new(next)),
                );
            }
        }
    }

    /// PR VL5b: drop a migrated-away slot's cursor (source side, after
    /// the flip — the cursor travelled to the target).
    pub fn remove_guest_cursor(&self, slot: u16) {
        let _ = self.guest_cursors.remove_sync(&slot);
    }

    /// PR VL5b (§5.5.2 step 3): arm the conveyor pass-task key tee for
    /// the keyspace `[lo, hi)`. One tee per volume at a time (one flip
    /// per slot at a time is the coordinator's law); returns the handle
    /// the engine drains rounds from.
    pub fn arm_migration_tee(&self, lo: u64, hi: u64, cap: usize) -> Arc<MigrationTee> {
        let tee = Arc::new(MigrationTee::new(lo, hi, cap));
        self.migration_tee.store(Some(tee.clone()));
        tee
    }

    /// Disarm the tee (cutover complete or migration aborted).
    pub fn disarm_migration_tee(&self) {
        self.migration_tee.store(None);
    }

    /// PR VL5b: one migration batch on this volume — bulk-copy puts and
    /// teardown deletes staged as ONE ordinary conveyor transaction.
    /// Guard-set EMPTY by design (the KvTx doc's architectural-exclusion
    /// class): the migrating keyspace has no other writer — bulk-copy
    /// targets an unpublished guest keyspace, and teardown runs strictly
    /// after the flip re-routed every op away from the source.
    pub async fn migration_apply(
        &self,
        puts: Vec<(u8, Vec<u8>, Vec<u8>)>,
        deletes: Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        for (tree_id, key, value) in puts {
            tx.stage_put(tree_id, key, Bytes::from(value));
        }
        for (tree_id, key) in deletes {
            tx.stage_delete(tree_id, key);
        }
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// PR VL5b: latch-free point read of one raw record (`tree_id`,
    /// `key`) — the delta-apply value re-read. `None` = deleted/absent.
    pub async fn migration_read_record(&self, tree_id: u8, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .tree_by_id(tree_id)
            .lookup(key)
            .await?
            .map(|b| b.to_vec()))
    }

    /// PR VL5b: the EFFECTIVE local ino of the filesystem root (global
    /// ino 1) on THIS volume — local 1 when slot 0 is the volume's
    /// legacy keyspace (every pre-migration set), the guest-namespaced
    /// root when the volume hosts slot 0 as a guest. Callers that probe
    /// root-ino records off a single volume (format config, mount
    /// registrations) resolve through this.
    pub fn slot0_root_ino(&self) -> Ino {
        match self.membership_stamp() {
            Some(st) if st.slots_hosted.contains(0) && st.resolved_native_slot() != Some(0) => {
                crate::meta_backend::guest_local_ino(0, 1)
            }
            _ => 1,
        }
    }

    /// Mount replay statistics.
    pub fn replay_stats(&self) -> KvReplayStats {
        self.replay
    }

    /// The §4.8 monotonic ino watermark as recovered by this mount.
    ///
    /// With spec §6.2 item-5 lanes live, this is the watermark that
    /// DOMINATES every lane: the dense watermark folded with each native
    /// lane cursor's snapshot. That is what a ledger record must carry —
    /// a successor recovering from it rounds up into its own lane, so a
    /// dominating watermark can never let a lane re-mint. Solo mounts
    /// (every mount today) pay one relaxed load for the latch and return
    /// the atomic verbatim.
    pub fn next_ino(&self) -> u64 {
        let dense = self.next_ino.load(Ordering::Acquire);
        if !self.lanes_live.load(Ordering::Relaxed) {
            return dense;
        }
        let mut hi = dense;
        self.lane_cursors.iter_sync(|(_, space), cursor| {
            if *space == super::ino_lane::InoSpace::Native {
                hi = hi.max(cursor.snapshot());
            }
            true
        });
        hi
    }

    /// The exclusive ceiling on raw local inos this volume could have
    /// minted: the §4.8 native watermark folded with every hosted guest
    /// keyspace's cursor. Every existing record's raw local is strictly
    /// below it.
    ///
    /// fsck class C9 indexes its ino bitmaps against this, so a dentry
    /// record's `child_ino` can never become an allocation authority — a
    /// corrupt value naming ino `u64::MAX` would otherwise size a bit
    /// vector from it (the `kv_bset` `record_count` lesson, spec §11
    /// TEST-4).
    pub fn max_local_ino_watermark(&self) -> u64 {
        let mut hi = self.next_ino();
        self.guest_cursors.iter_sync(|_slot, cursor| {
            hi = hi.max(cursor.snapshot());
            true
        });
        hi
    }

    /// `true` ⇔ the record at EFFECTIVE local `local_ino` was minted
    /// **before this mount's writer era began** — fsck class C9's
    /// candidate filter (`src/fsck.rs`).
    ///
    /// Inos are monotonic per keyspace and never reused (§4.8), and the
    /// era's floor is the watermark recovered at open, so this is an exact
    /// statement about provenance and not a heuristic: `true` ⇒ the record
    /// survived a prior mount, `false` ⇒ this mount minted it (or its
    /// keyspace's provenance is unknown — see below). It is what makes an
    /// "inode with no dentry" verdict safe against live work: a create
    /// legitimately commits the inode record before the dentry, and every
    /// ino this mount can mint is at or above the floor.
    ///
    /// **Fail-closed** for a guest keyspace this mount had no cursor for at
    /// open (a virgin slot, or one whose cursor arrived mid-mount with a
    /// migrated slot): `false` — no verdict rather than a guess.
    pub fn minted_in_prior_era(&self, local_ino: Ino) -> bool {
        match crate::meta_backend::split_guest_local(local_ino) {
            Some((slot, raw)) => self
                .era_ino_floor_guest
                .get()
                .and_then(|m| m.get(&slot).copied())
                .is_some_and(|floor| raw < floor),
            None => local_ino < self.era_ino_floor_native,
        }
    }

    /// `true` ⇔ this volume's format expresses `offset ‖ incarnation`
    /// block keys (incompat bit 13 — spec §6.2 item 6) AND this mount has a
    /// durable writer era to compose stamps from (incompat bit 7's term,
    /// which the stamping path requires and a read-only/probe mount does
    /// not have). Nothing stamps bit 13 today (ruling D9), so this is
    /// `false` on every production volume and every key stays bare.
    pub fn block_key_incarnation_engaged(&self) -> bool {
        self.sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION
            != 0
            && self.writer_term() > 0
    }

    /// `true` ⇔ this volume's format expresses per-writer ino lanes
    /// (incompat bit 12 — spec §6.2 item 5). Nothing stamps it today
    /// (ruling D9), so this is `false` on every production volume and a
    /// non-solo lane is refused.
    pub fn ino_lanes_stamped(&self) -> bool {
        self.sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_INO_LANES != 0
    }

    /// Spec §6.2 item 5: mint one **native** local ino in appender
    /// `part`'s lane.
    ///
    /// `part == AppendPartition::SOLO` is exactly [`Self::allocate_ino`]
    /// (the shipped path, one `fetch_add`); any other partition requires
    /// incompat bit 12 and mints from the lane cursor, whose floor is the
    /// smallest lane value at or above the recovered dense watermark.
    pub fn allocate_ino_in(
        &self,
        part: super::journal::AppendPartition,
    ) -> std::result::Result<Ino, KvError> {
        if part.is_solo() {
            return Ok(self.allocate_ino());
        }
        Ok(self
            .lane_cursor_for(super::ino_lane::InoSpace::Native, part)?
            .mint())
    }

    /// Spec §6.2 item 5: mint one **guest** local ino for hosted slot
    /// `slot` in appender `part`'s lane (raw — the caller namespaces it
    /// with `guest_local_ino`, exactly as [`Self::allocate_guest_ino`]).
    pub fn allocate_guest_ino_in(
        &self,
        slot: u16,
        part: super::journal::AppendPartition,
    ) -> std::result::Result<Ino, KvError> {
        if part.is_solo() {
            return self.allocate_guest_ino(slot).map_err(KvError::Io);
        }
        Ok(self
            .lane_cursor_for(super::ino_lane::InoSpace::Guest(slot), part)?
            .mint())
    }

    /// Raise appender `part`'s lane cursor for `space` to at least the
    /// smallest lane value at or above `dense_floor` — mount recovery
    /// ([`super::ino_lane::recover_ino_floor`]) and the slot-migration
    /// flip's target side. Monotone and idempotent.
    pub fn install_ino_lane_floor(
        &self,
        space: super::ino_lane::InoSpace,
        part: super::journal::AppendPartition,
        dense_floor: u64,
    ) -> std::result::Result<(), KvError> {
        self.lane_cursor_for(space, part)?
            .install_floor(dense_floor);
        Ok(())
    }

    /// Appender `part`'s current lane-cursor snapshot for `space` —
    /// `None` when this mount has never minted in that lane.
    pub fn ino_lane_snapshot(
        &self,
        space: super::ino_lane::InoSpace,
        part: super::journal::AppendPartition,
    ) -> Option<u64> {
        self.lane_cursors
            .read_sync(&(part.writer_id(), space), |_, v| v.snapshot())
    }

    /// The lane cursor for `(part, space)`, created on first use from the
    /// space's recovered dense watermark. Refuses loud on a volume whose
    /// format does not express lanes (ruling D9's boundary) — the caller
    /// is asking this mount to mint into a partition the volume's ino
    /// namespace is not partitioned for, which is how duplicate inos
    /// arrive.
    fn lane_cursor_for(
        &self,
        space: super::ino_lane::InoSpace,
        part: super::journal::AppendPartition,
    ) -> std::result::Result<Arc<crate::lane_core::LaneCursor>, KvError> {
        if !part.is_solo() && !self.ino_lanes_stamped() {
            return Err(KvError::Corrupt(format!(
                "{}: refusing to mint inos as appender {} of {} — this volume's format does \
                 not express per-writer ino lanes (incompat bit 12 absent, spec §6.2 item 5). \
                 A lane-unaware peer mints DENSE inos across every lane, and duplicate inos \
                 alias files immediately.",
                self.path.display(),
                part.writer_id(),
                part.writers(),
            )));
        }
        let key = (part.writer_id(), space);
        if let Some(c) = self.lane_cursors.read_sync(&key, |_, v| v.clone()) {
            return Ok(c);
        }
        // First mint in this lane: seed from the space's recovered dense
        // watermark (native = the §4.8 watermark, guest = the VL5b
        // per-slot cursor, 2 for a virgin slot), rounded up into the lane.
        let dense = match space {
            super::ino_lane::InoSpace::Native => self.next_ino.load(Ordering::Acquire),
            super::ino_lane::InoSpace::Guest(slot) => self
                .guest_cursor_snapshot(slot)
                .unwrap_or(super::ino_lane::LOCAL_INO_BASE),
        };
        let fresh = Arc::new(super::ino_lane::lane_cursor(part, dense));
        let cursor = match self.lane_cursors.insert_sync(key, fresh.clone()) {
            Ok(()) => fresh,
            Err(_) => self
                .lane_cursors
                .read_sync(&key, |_, v| v.clone())
                .ok_or_else(|| KvError::Corrupt("ino lane cursor raced out".to_string()))?,
        };
        // Publish AFTER the cell exists: a reader that observes the latch
        // must find the cursor (`next_ino`'s dominance fold).
        self.lanes_live.store(true, Ordering::Release);
        Ok(cursor)
    }

    /// POSIX-1: LIVE inode records on this volume — the `statfs`
    /// `f_ffree` source.
    ///
    /// v3 allocates inos monotonically and never reuses them (§4.8), so
    /// the watermark counts inodes ever *allocated*, not inodes that
    /// exist: derived straight, `IUsed` rises forever and a create/delete
    /// loop reports a full filesystem on an empty one (tools gating on
    /// `IUse%` then refuse to write). This subtracts the destroys this
    /// mount has committed — `destroy_inodes` is the one place a record
    /// leaves `TREE_INODES`, and it already knows the count.
    ///
    /// **Honest bound:** the subtrahend is per-mount RAM state, so a
    /// remount re-seeds from the allocation cursors (an over-report,
    /// never an under-report — `statfs` may only ever be pessimistic
    /// about free slots). A durable live count belongs in the root-ledger
    /// payload and is deferred to the batched format window; the cursor
    /// base also (deliberately) keeps counting inos burned by failed
    /// creates, exactly as the §4.8 law describes them.
    ///
    /// **Every** allocation cursor counts, not just the volume
    /// watermark: since the dynamic-routing MINT_SPREAD (64 slots per
    /// volume rotor, `docs/design-dynamic-meta-routing.md` §5.3) a
    /// create mints from the picked slot's GUEST cursor unless the slot
    /// is the volume's legacy keyspace, so a watermark-only derivation
    /// misses ~63 of every 64 creates — `df -i` under-reported IUsed by
    /// that factor on any real (width-2^16) mount, and the in-RAM test
    /// constructor's `W ≤ 1` identity hid it.
    pub fn live_inodes(&self) -> u64 {
        // Cursor progression: every cursor's reserved base is 2 (ino 1 =
        // root, a virgin cursor starts at 2) — the root is added back
        // ONCE by the caller across the volume set.
        let mut allocated = self.next_ino.load(Ordering::Acquire).saturating_sub(2);
        self.guest_cursors.iter_sync(|_slot, cursor| {
            allocated = allocated.saturating_add(cursor.snapshot().saturating_sub(2));
            true
        });
        // Spec §6.2 item 5: a LANE cursor strides by `writers`, so its
        // progression is NOT `cursor − 2` — counting it densely would
        // over-report `IUsed` by that factor (the POSIX-1 failure mode,
        // one level down). `minted_in_ino_lane` is the exact count.
        // Structurally skipped on every mount today (no lane exists).
        if self.lanes_live.load(Ordering::Relaxed) {
            self.lane_cursors.iter_sync(|_key, cursor| {
                allocated = allocated.saturating_add(cursor.minted());
                true
            });
        }
        allocated.saturating_sub(self.destroyed_inodes.load(Ordering::Relaxed))
    }

    /// Free heap extents right now (mount log / stats surface).
    pub fn free_extents(&self) -> u64 {
        self.alloc.free_extents()
    }

    /// Extents awaiting durable-checkpoint retirement (§4.7 pending-free;
    /// `meta_kv_pending_free` on the stats surface, design §10).
    pub fn pending_free_extents(&self) -> u64 {
        self.alloc.pending_count()
    }

    /// The volume path.
    pub fn device_path(&self) -> &Path {
        &self.path
    }

    /// This volume's user-facing xattr VALUE cap `min(65_536, node_size/4)`
    /// (§4.2) — the largest inline xattr value it can store. PR K8 (§5.3):
    /// the data path consults it (via
    /// [`crate::meta_backend::RoutedMetaBackend::xattr_value_cap`]) to
    /// decide the per-volume layout inline-spill boundary, so mixed
    /// mixed-`node_size` v3 sets spill per volume.
    pub fn xattr_value_cap(&self) -> usize {
        self.cache.config().layout.xattr_value_cap()
    }

    /// The three logical trees, tree-id order (inodes, dentries, xattrs)
    /// — the digest walk's input ([`super::builder::digest_walk`]).
    ///
    /// Deliberately excludes the §6.2 item-1 block-reference tree: the
    /// §4.10 digest walk, the VL5 slot-migration keyspace, and fsck's
    /// C1 tree walk are all defined over the three USER trees, and
    /// widening this array would silently redefine all three. Structural
    /// consumers use [`Self::all_trees`].
    pub fn trees(&self) -> [&KvTree; 3] {
        [&self.inodes, &self.dentries, &self.xattrs]
    }

    /// Every tree this volume must checkpoint, flush, and name a root
    /// for: the three §4.2 user trees plus the durable block-reference
    /// tree when incompat bit 8 engaged it (spec §6.2 item 1).
    pub fn all_trees(&self) -> Vec<&KvTree> {
        let mut v: Vec<&KvTree> = self.trees().to_vec();
        if let Some(t) = self.block_refs.as_ref() {
            v.push(t);
        }
        v
    }

    /// `true` ⇔ durable block-reference accounting is engaged on this
    /// volume (incompat bit 8 present and the mount may write). `false`
    /// means every block-ownership answer is derived state, exactly as it
    /// was before the bit existed.
    pub fn block_refs_engaged(&self) -> bool {
        self.block_refs.is_some()
    }

    /// Every durable block reference recorded on this volume for the data
    /// volume tagged `vol_tag` (spec §6.2 item 1) — the mount-recovery
    /// scan that REPLACES the inode-tree walk, and the census the
    /// durable-vs-derived oracle compares.
    ///
    /// Paged range scans (the `recover_active_blocks_v3` walk shape), so
    /// the peak footprint is one page, not one volume. Returns `Ok(vec![])`
    /// on a volume with no engaged tree — a caller that must distinguish
    /// "no accounting" from "no references" asks
    /// [`Self::block_refs_engaged`].
    pub async fn block_ref_scan(
        &self,
        vol_tag: u64,
    ) -> std::result::Result<Vec<super::block_refs::BlockRef>, KvError> {
        let Some(tree) = self.block_refs.as_ref() else {
            return Ok(Vec::new());
        };
        let (mut cursor, end) = super::block_refs::volume_range(vol_tag);
        let mut out = Vec::new();
        loop {
            let page = tree.range(&cursor, &end, 512).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                // Decode both halves: a malformed accounting record is
                // loud corruption, never a silently skipped reference
                // (an under-count is the exact failure this structure
                // exists to prevent).
                let r = super::block_refs::decode_block_ref_key(k)?;
                let _ = super::block_refs::decode_block_ref_value(v)?;
                out.push(r);
            }
        }
        Ok(out)
    }

    /// The durable reference population of **one block** — the ordered
    /// range count over the `(vol_tag, block_idx)` prefix
    /// ([`super::block_refs::block_range`]): `refcount(block) == records
    /// in range`, the tree's own law. DLM S9's shipped-free executor is
    /// the consumer (`crate::cowriter::durable_block_refcount`): the
    /// owner-side validation of a peer's terminal free is the ledger, not
    /// the peer's claim.
    ///
    /// `Ok(0)` on a volume with no engaged tree, exactly as
    /// [`Self::block_ref_scan`] answers empty — a caller that must
    /// distinguish asks [`Self::block_refs_engaged`].
    pub async fn block_ref_count(
        &self,
        vol_tag: u64,
        block_idx: u64,
    ) -> std::result::Result<usize, KvError> {
        let Some(tree) = self.block_refs.as_ref() else {
            return Ok(0);
        };
        let (mut cursor, end) = super::block_refs::block_range(vol_tag, block_idx);
        let mut population = 0usize;
        loop {
            let page = tree.range(&cursor, &end, 512).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                // Decode both halves (the block_ref_scan discipline): a
                // malformed accounting record is loud corruption, never a
                // silently skipped — or silently COUNTED — reference.
                let _ = super::block_refs::decode_block_ref_key(k)?;
                let _ = super::block_refs::decode_block_ref_value(v)?;
                population += 1;
            }
        }
        Ok(population)
    }

    /// The resolved OQ 2 contract class for every v3 volume:
    /// `meta_volume_atomicity = "cow-checksummed"` — satisfied by
    /// construction (§4.10; every unit checksummed, never-overwrite-live),
    /// independent of the physical probe reported alongside.
    pub fn atomicity_contract(&self) -> &'static str {
        META_VOLUME_ATOMICITY_COW
    }

    /// The mount-time physical atomicity probe result (resolved OQ 2:
    /// reported ALONGSIDE the contract class for operator hardware
    /// visibility). Set once by the mount path.
    pub fn set_atomicity_physical(&self, class: crate::meta_backend::atomicity::AtomicityClass) {
        let _ = self.atomicity_physical.set(class);
    }

    /// `meta_volume_atomicity_physical` (stats surface); `"unprobed"`
    /// for harness-built backends.
    pub fn atomicity_physical(&self) -> &'static str {
        self.atomicity_physical
            .get()
            .map(|c| c.as_str())
            .unwrap_or("unprobed")
    }

    // -----------------------------------------------------------------
    // Read ops (the v2 `Metadata` read shapes, served by tree + fold).
    // -----------------------------------------------------------------

    /// Live-record scan over one hash chain window
    /// `(prefix, hash, 0) ..= (prefix, hash, 255)` — holes from deleted
    /// lower `coll_seq`s are naturally skipped because `range` yields only
    /// post-fold live records (§4.2).
    async fn chain_scan(
        &self,
        tree: &KvTree,
        start_key: &[u8],
        end_key: &[u8],
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        // A chain is ≤ 256 records by construction.
        tree.range(start_key, end_key, 256).await
    }

    /// Resolve `name` under `parent` to its dentry value, if live.
    async fn find_dentry(
        &self,
        parent: Ino,
        name: &str,
    ) -> std::result::Result<Option<DentryValue>, KvError> {
        if name.len() > 255 {
            return Ok(None); // unrepresentable ⇒ cannot exist
        }
        let hash = dentry_name_hash54(name.as_bytes(), self.sb.hash_seed);
        let start = dentry_key(parent, hash, 0);
        let end = dentry_key(parent, hash, u8::MAX);
        for (_k, v) in self.chain_scan(&self.dentries, &start, &end).await? {
            let d = DentryValue::decode(&v)?;
            if d.name == name.as_bytes() {
                return Ok(Some(d));
            }
        }
        Ok(None)
    }

    async fn read_inode_value(&self, ino: Ino) -> std::result::Result<Option<InodeValue>, KvError> {
        match self.inodes.lookup(&inode_key(ino)).await? {
            Some(v) => Ok(Some(InodeValue::decode(&v)?)),
            None => Ok(None),
        }
    }

    fn not_found(what: String) -> crate::error::SqueezefsError {
        crate::error::SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, what))
    }

    /// Resolve `name` under `parent` and return the child's attributes.
    /// Seeded-hash chain probe (§4.2): live records of the
    /// `(parent, hash54, *)` window compared by full name.
    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let dentry = self.find_dentry(parent, name).await?.ok_or_else(|| {
            Self::not_found(format!("Dentry {name} not found in parent {parent}"))
        })?;
        self.getattr(dentry.child_ino).await
    }

    /// Reverse dentry resolution: the LOCAL parent ino of the dentry
    /// naming `child` (dentry values carry GLOBAL child inos), or `None`
    /// when no dentry on this volume names it.
    ///
    /// **Cost, stated honestly:** a full dentries-tree range scan —
    /// O(entries) over the RAM-authoritative fold (the fsck census-walk
    /// shape). Its ONLY caller is `LOOKUP(nodeid, "..")` on the
    /// `FUSE_EXPORT_SUPPORT` directory-handle **reconnect** path
    /// (fstests generic/467; `open_by_handle_at` of an evicted directory
    /// after cache drop) — cold and rare by construction. The hot ".."
    /// path never reaches here: the kernel resolves ".." from its own
    /// dcache while the directory is connected. v3 stores no parent
    /// pointer in the inode record; adding one is an inode-value
    /// version bump (a forward-only format change) that this rare path
    /// does not justify.
    pub async fn find_parent_of_child(&self, child_global: Ino) -> Result<Option<Ino>> {
        // POSIX-4: the scan is the instrument. `meta_parent_scans` growing
        // per readdir means the `..` parent memo stopped serving and every
        // ls/find/du/rsync/tar walk is paying O(total dentries) again.
        crate::fuse_client::METRICS
            .meta_parent_scans
            .fetch_add(1, Ordering::Relaxed);
        let end = super::tree::KEY_SPACE_MAX;
        let mut cursor: Vec<u8> = vec![0u8];
        loop {
            let page = self.dentries.range(&cursor, &end, SCAN_PAGE).await?;
            let Some((last_key, _)) = page.last() else {
                return Ok(None);
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                let d = DentryValue::decode(v)?;
                if d.child_ino == child_global {
                    let (parent, _hash54, _coll) = decode_dentry_key(k)?;
                    return Ok(Some(parent));
                }
            }
        }
    }

    /// Attributes of `ino` from the inode tree (K1 fold; Δtime deltas
    /// folded into the base record; PR M6 pending-times refinements
    /// folded on top — absorbed echoes are read-visible before they
    /// drain).
    pub async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        self.fold_pending_times(ino, &mut v);
        Ok(Inode {
            ino,
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            size: v.size,
            nlink: v.nlink,
            atime: v.atime,
            mtime: v.mtime,
            ctime: v.ctime,
            flags: v.flags,
            rdev: v.rdev,
        })
    }

    /// List `dir` per the module-docs offset contract; at most `max`
    /// entries, hash order (legal POSIX readdir order, risk R8). An ino
    /// with no dentries lists empty — the v2 contract (no existence
    /// check on the read path).
    pub async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        Ok(self
            .readdir_page(dir, offset, max)
            .await?
            .into_iter()
            .map(|(_cookie, entry)| entry)
            .collect())
    }

    /// [`Self::readdir`] with each entry's §5.1 resume cookie
    /// (`3 + ((hash54 << 8) | coll_seq)` — the dentry key suffix, biased).
    /// PR K7's FUSE streaming path emits these as the directory offsets:
    /// they are stable across concurrent inserts/removals (they ARE the
    /// key), strictly ascending in emission order, and sign-bit-clear by
    /// the 54-bit hash construction (§4.2).
    pub async fn readdir_page(
        &self,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        // §5.1 resume rule: offsets 0/1/2 ⇒ the directory start (the
        // synthetic ./.. slots belong to the FUSE layer); c > 2 ⇒
        // strictly after the cookie's key suffix.
        let mut cursor: Vec<u8> = match decode_readdir_cookie(offset).map_err(KvError::from)? {
            ReaddirPos::Start | ReaddirPos::AfterDot | ReaddirPos::AfterDotDot => {
                dentry_key(dir, 0, 0).to_vec()
            }
            ReaddirPos::AfterEntry { hash54, coll_seq } => {
                key_successor(&dentry_key(dir, hash54, coll_seq))
            }
        };
        let end = dentry_key(dir, HASH54_MAX, u8::MAX);
        while out.len() < max {
            let want = (max - out.len()).min(SCAN_PAGE);
            let page = self.dentries.range(&cursor, &end, want).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                let (_parent, hash54, coll_seq) = decode_dentry_key(k)?;
                let d = DentryValue::decode(v)?;
                out.push((
                    encode_readdir_cookie(hash54, coll_seq),
                    DirEntry {
                        ino: d.child_ino,
                        name: String::from_utf8_lossy(&d.name).into_owned(),
                        file_type: u32::from(d.file_type) << 12,
                    },
                ));
            }
        }
        Ok(out)
    }

    /// One xattr value; `Ok(None)` for absent names and inos alike (the
    /// historical degrade contract).
    pub async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        if name.len() > 255 {
            return Ok(None);
        }
        let hash = xattr_name_hash56(name.as_bytes(), self.sb.hash_seed);
        let start = xattr_key(ino, hash, 0);
        let end = xattr_key(ino, hash, u8::MAX);
        for (_k, v) in self.chain_scan(&self.xattrs, &start, &end).await? {
            let x = XattrValue::decode(&v)?;
            if x.name == name.as_bytes() {
                return Ok(Some(x.value));
            }
        }
        Ok(None)
    }

    /// All xattr names of `ino`; empty for inos without xattrs (v2
    /// contract).
    pub async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cursor: Vec<u8> = xattr_key(ino, 0, 0).to_vec();
        let end = xattr_key(ino, HASH56_MAX, u8::MAX);
        loop {
            let page = self.xattrs.range(&cursor, &end, SCAN_PAGE).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (_k, v) in &page {
                let x = XattrValue::decode(v)?;
                out.push(String::from_utf8_lossy(&x.name).into_owned());
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // PR K6b — the §4.4 commit pipeline + §4.6 checkpoint surface.
    // -----------------------------------------------------------------

    /// The per-volume metadata lock manager (design §4.9 4a): the same
    /// `DlmLockManager` discipline the v2 backend embeds — I/D stripes
    /// acquired *before* any node lock (level 4b).
    pub fn dlm(&self) -> &DlmLockManager {
        &self.dlm
    }

    /// The mounted journal ring (K6a dropped it after replay; K6b stores
    /// it — the §4.4 admission/reservation core and the checkpoint task's
    /// `reusable_upto` watermark live here). Public for the crash harness,
    /// which computes physical entry offsets to arm write faults.
    pub fn journal_ring(&self) -> &JournalRing {
        &self.ring
    }

    /// PR M7 (§5.5 D5): user transactions enqueued on this volume's
    /// commit conveyor and not yet drained by a pass — the conformance
    /// suite's enqueue-sequencing probe and a stats gauge (a queue that
    /// grows on a quiet mount means the pass wedged).
    pub fn conveyor_pending_len(&self) -> usize {
        self.conveyor.pending()
    }

    /// §4.8 monotonic ino allocation: one `fetch_add`, no reuse, no
    /// free-on-failure (a failed create burns the ino; crash-skipped
    /// ranges waste nothing that matters).
    pub fn allocate_ino(&self) -> Ino {
        self.next_ino.fetch_add(1, Ordering::AcqRel)
    }

    /// Reserve `n` consecutive NATIVE locals (rung 13's intent-supply
    /// grant): one `fetch_add` over the same §4.8 watermark, so the
    /// reserved numbers can never be re-minted by this incarnation and
    /// unused ones burn exactly as a failed create's ino does. Cursor
    /// recovery after a restart may reuse an un-APPLIED reservation's
    /// numbers — which is why a supply is era-bound on the wire
    /// ([`crate::meta_ship::InoSupply`]): a stale-era flush refuses whole
    /// before any such number could reach a record.
    pub fn reserve_ino_range(&self, n: u64) -> Ino {
        self.next_ino.fetch_add(n, Ordering::AcqRel)
    }

    /// §4.4 pt 4 escalation state: repeated journal-write failures latch
    /// the volume failed — every subsequent mutation returns `EIO` until
    /// remount (the `errors=remount-ro` analog). The routed layer mirrors
    /// this into `disabled_volumes`.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Ring-admission parks so far (§4.4 pt 5 `meta_kv_journal_full_stalls`
    /// — counted **before** any node lock is taken).
    pub fn journal_full_stalls(&self) -> u64 {
        self.stalls.load(Ordering::Relaxed)
    }

    /// Coalesced durability barrier for this volume (the v2
    /// retired v2 `sync_device` shape, riding the same
    /// `SyncCoalescer` group-commit discipline — §4.6 pt 4). After the
    /// barrier, ledger records written before it are known durable: the
    /// §4.6 pt 3 pending-reclaim watermark drains here too.
    ///
    /// **PR M1 — barrier-failure escalation lives HERE** (design
    /// §5.0 B1 pt 3, Issue 14), so every journal-durability barrier —
    /// the strict `commit_tx` path, the checkpoint tick's deferred-flush
    /// and cycle barriers, and the fsync path — shares the same two
    /// rungs:
    ///
    /// - **reservation-conflict class** (`EBADE` — the kernel's mapping
    ///   of the reservation-conflict block status): this holder has been
    ///   fenced/usurped at the device ⇒ latch `failed` IMMEDIATELY with
    ///   the guard message (`writer_guard_fenced`). Detection bound:
    ///   one flush cadence + one barrier. Measured caveat (M1 root
    ///   session, kernel nvmet 7.1.3): direct/passthru writes surface
    ///   the conflict status (0x83/EBADE), but the buffered-writeback
    ///   path can normalize it to plain `EIO` by the time `fdatasync`
    ///   reports (`mapping_set_error` collapses AS-mapping errors) — in
    ///   which case the generic rung below still fail-stops the fenced
    ///   holder within `JOURNAL_FAILURE_LATCH` barriers (the design's
    ///   stated fallback: "falling back to generic escalation on plain
    ///   EIO").
    /// - **generic class**: consecutive-barrier-failure rung — reuses the
    ///   `JOURNAL_FAILURE_LATCH` = 3 semantics with success-reset, on a
    ///   counter deliberately separate from `journal_failures` (which the
    ///   entry-write success path resets right *before* the strict
    ///   barrier runs — sharing it would erase the escalation every
    ///   commit).
    ///
    /// Classification happens inside the leader's `sync_fn` because the
    /// coalescer fans failures out as rendered strings (`raw_os_error`
    /// does not survive to the waiters) — exactly one classification per
    /// physical barrier attempt.
    ///
    /// **Scope boundary**: this escalation covers journal-durability
    /// barriers ONLY. The data-path writeback ladder (staging flush /
    /// block upload retries) is elsewhere and stays retry-forever by
    /// design — the never-lossy contract.
    ///
    /// **PR M4 (D1.b audit row 1): the wait is bounded** — `barrier_bounded`
    /// races each batch's `fdatasync` against `timeout_threshold`
    /// (`SQUEEZEFS_TIMEOUT`) and synthesizes an `ETIMEDOUT`-class error
    /// for the batch on expiry, so fsync-path callers keep userspace
    /// liveness on a sick device (the synthesized error the audit calls
    /// load-bearing). A bounded-out barrier's device op is abandoned to
    /// its `uring_fs` worker; its true outcome is unknown, so it counts
    /// toward NEITHER barrier rung (`note_barrier_success/_failure` run
    /// only on real outcomes) — escalation truth stays with real
    /// failures, and a genuinely wedged device keeps producing loud
    /// bounded errors every attempt.
    pub async fn sync_device(&self) -> Result<()> {
        self.sync
            .barrier_bounded(self.timeout_threshold, || async move {
                crate::fuse_client::METRICS
                    .meta_device_syncs
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // DUR-3: this invocation's start ticket, taken BEFORE the
                // device op. Everything pushed onto `pending_reclaim`
                // after this point carries an epoch ≥ `started_at` and is
                // therefore NOT covered by this barrier.
                let started_at = self.barrier_starts.fetch_add(1, Ordering::AcqRel) + 1;
                let out = crate::uring_fs::fdatasync(self.path.clone()).await;
                match &out {
                    Ok(()) => {
                        // Publish coverage before the fan-out, so every
                        // waiter this barrier releases (leader and
                        // followers alike) drains against it.
                        self.barrier_durable.fetch_max(started_at, Ordering::AcqRel);
                        self.note_barrier_success();
                    }
                    Err(e) => self.note_barrier_failure(e),
                }
                out
            })
            .await?;
        self.after_durable_barrier();
        Ok(())
    }

    /// The DUR-3 push epoch: barriers already started (hence unable to
    /// cover anything written from now on). Read by `checkpoint_cycle`
    /// AFTER its ledger record's write completed.
    pub(super) fn barrier_push_epoch(&self) -> u64 {
        self.barrier_starts.load(Ordering::Acquire)
    }

    /// Barrier succeeded: reset the consecutive-failure rung.
    fn note_barrier_success(&self) {
        self.barrier_failures.store(0, Ordering::Release);
    }

    /// Protocol-terminal fail-stop latch (the §4.4 pt 4 `failed`
    /// semantics for non-guard, non-barrier terminals — currently the
    /// §4.7 wedged-tail bound, design-smo-replay-currency PR 4 clause b):
    /// mutations return EIO until remount; loud exactly once.
    pub(super) fn fail_stop_loud(&self, why: &str) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: {why}; volume marked FAILED (mutations return EIO \
                 until remount)",
                self.path.display()
            );
        }
    }

    /// Barrier failed: classify and escalate (see [`Self::sync_device`]).
    fn note_barrier_failure(&self, e: &crate::error::SqueezefsError) {
        if let crate::error::SqueezefsError::Io(ioe) = e {
            if crate::meta_backend::reservation::is_reservation_conflict(ioe) {
                self.guard_fail_stop(
                    "reservation-conflict errno at a durability barrier — this holder's \
                     writes are fenced at the device (usurped or paused-then-preempted)",
                );
                return;
            }
        }
        let n = self.barrier_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= JOURNAL_FAILURE_LATCH && !self.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: {n} consecutive durability-barrier failures — volume \
                 marked FAILED (mutations return EIO until remount; §4.4 pt 4 semantics \
                 extended to barriers, design-metadata-throughput §5.0)",
                self.path.display()
            );
        }
    }

    /// Post-barrier bookkeeping (§4.6 pt 3, §4.7): every ledger record
    /// written before the barrier that just completed is now durable —
    /// advance `reusable_upto` to its tail (waking admission parkers),
    /// release the pending frees **that tail covers**, and advance the
    /// cache's durable tail (torn-tail classifier + tombstone-elision
    /// floor). The pending-free gate rides the TAIL, not the record's
    /// checkpoint seq (design-smo-replay-currency §2-A): record
    /// durability certified the wrong thing — a record whose flush pass
    /// skipped the flip-carrying interior (or whose tail a root swap's
    /// dying floor clamped) is durable while the freeing swap/flips still
    /// ride the replay window, and releasing on its generation is the
    /// recycled-extent stale-route mechanism (child-seq mount refusals).
    /// All three §4.6-pt-3 watermarks now advance on one clock.
    pub(super) fn after_durable_barrier(&self) {
        // DUR-3: release only what a COMPLETED barrier covers. A record
        // pushed at epoch `e` was written while barriers 1..=e were
        // already started, so only a barrier that started at `e + 1` or
        // later can have flushed it. Push epochs are non-decreasing, so
        // the covered set is always a prefix.
        let covered = self.barrier_durable.load(Ordering::Acquire);
        let drained: Vec<(u64, u64)> = {
            let mut g = self.pending_reclaim.lock().unwrap();
            let split = g.partition_point(|&(_, epoch)| epoch < covered);
            g.drain(..split).collect()
        };
        for (tail, _) in drained {
            self.alloc.advance_durable(tail);
            self.cache.set_durable_tail(tail);
            self.ring.advance_reusable_upto(tail);
        }
    }

    /// Force one full checkpoint cycle now (§4.6 pt 2): flush dirty
    /// nodes (snapshot-then-write), barrier, compute the tail, write the
    /// ledger slot, advance `reusable_upto` once durable. The background
    /// task calls this on cadence; tests and `shutdown` call it directly.
    /// Serialized with the background task through the SMO mutex.
    pub async fn checkpoint_now(&self) -> std::result::Result<(), KvError> {
        let mut smo = self.smo.lock().await;
        self.checkpoint_cycle(&mut smo, true).await
    }

    /// The **cadence** face of [`Self::checkpoint_now`] (`barrier_now =
    /// false`): the steady-state shape the background tick runs once per
    /// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` — write the ledger slot, push
    /// the tail onto `pending_reclaim`, and defer durability to a later
    /// barrier. Exposed so the DUR-3 reclamation-epoch legs can drive
    /// that exact shape deterministically instead of racing the tick.
    pub async fn checkpoint_cadence(&self) -> std::result::Result<(), KvError> {
        let mut smo = self.smo.lock().await;
        self.checkpoint_cycle(&mut smo, false).await
    }

    /// The §4.4 pt 4 hole discipline's checkpoint: cycle until the
    /// written ledger tail reaches at least `pos` (the unwritten hole's
    /// END), so replay's chain walk — which starts AT the tail — can
    /// never enter the hole range behind it. One cycle usually suffices;
    /// the FIND-VS-A dying-floor clamp can legitimately hold the first
    /// cycle's tail below `pos` (a floor that died at an SMO retire /
    /// root swap since the last ledger), in which case that cycle
    /// discharges the floors and the next one clears the hole. Bounded:
    /// floors drain in one cycle and in-flight reservations are finite,
    /// so a stuck tail is a real defect — fail loud.
    /// This volume's **commit watermark**: the journal ring's reservation
    /// frontier — every transaction ever committed here sits strictly
    /// below it. Rung 12 (live finding #4): the S10 delegation grant
    /// carries it as the view-currency token, because the inode's
    /// `(ctime, mtime, size)` triple ALIASES under serial-create rates —
    /// two creates inside one clock tick leave the parent's stamp equal
    /// while its dentry set differs, and the fleet's `tar -x` served a
    /// stale authoritative NEGATIVE from exactly that window. A journal
    /// position cannot alias.
    pub fn commit_watermark(&self) -> u64 {
        self.ring.core().head()
    }

    /// The **view watermark** of this volume's RAM-authoritative state:
    /// the journal position its node cache's adopted ledger record
    /// covers. On a WRITE mount the RAM state is the commit frontier
    /// itself (commits apply to RAM before the journal write — §4.4), so
    /// the answer is [`Self::commit_watermark`]; on a READER/co-writer
    /// view it is the adopted checkpoint's `journal_tail_seq` — the
    /// prefix bound revalidation maintains. `view ≥ grant` is the
    /// delegated serve's currency law: a covered prefix includes every
    /// transaction at or below the grant's mint.
    pub fn view_watermark(&self) -> u64 {
        if self.read_only {
            self.cache.durable_tail()
        } else {
            self.ring.core().head()
        }
    }

    pub async fn checkpoint_past(&self, pos: u64) -> std::result::Result<(), KvError> {
        let mut smo = self.smo.lock().await;
        for _ in 0..8 {
            self.checkpoint_cycle(&mut smo, true).await?;
            if self.last_ledger_tail.load(Ordering::Acquire) >= pos {
                return Ok(());
            }
        }
        Err(KvError::Corrupt(format!(
            "checkpoint tail failed to clear the journal hole ending at {pos} after 8 \
             cycles (tail stuck at {}) — replay would walk into the hole",
            self.last_ledger_tail.load(Ordering::Acquire)
        )))
    }

    /// Cover this volume's **bring-up journal residue**: barriered
    /// checkpoint cycles until `reusable_upto == head`, so the volume
    /// SERVES with an empty replay window and a ZERO reclaimable tail.
    ///
    /// **Why this is load-bearing (the 2026-08 merge-wave wedge-crumb
    /// regression, pinned by `tests/fuse_watchdog_teardown_tests.rs`
    /// `fresh_write_mount_serves_with_zero_reclaimable_journal_residue`):**
    /// every bring-up commit — the D0 `writer_claim` tx (since DLM S2
    /// carrying the `writer_term` record too), and at the routed layer
    /// the S3.5 intent roll-forward + retirement — used to be left
    /// committed-but-uncovered. On a wedged-not-failed ring (the D1.b
    /// audit-row-2 class) the first checkpoint tick then covered that
    /// residue and released exactly its size as admission budget: a
    /// one-shot crumb that let any parked committer with a small enough
    /// entry slip through the park-escalation lattice and commit
    /// silently — and the entry-write success reset `journal_failures`,
    /// erasing the crossings already tripped. S2 grew the claim tx from
    /// 186 B to 249 B, past a 195 B create entry, and the pinned
    /// escalation law fell. Zero residue at serve-start closes the
    /// CLASS (every committer size, every bring-up commit) instead of
    /// re-tuning sizes, and makes bring-up symmetric with shutdown's
    /// `tail == head` law (an empty replay window at both ends).
    ///
    /// Read-only backends never write and never claimed — nothing to
    /// cover, and covering would violate the §4.11 withhold. The
    /// fixpoint loop is the shutdown final-cycle discipline verbatim
    /// (`checkpoint.rs`): a cycle's flush pass may itself journal (SMO
    /// claims/frees land past the tail it was computed from), so iterate
    /// — convergence is bounded by the SMO cascade height; the bound is
    /// defensive and a stuck tail fails the mount loud.
    pub async fn cover_bring_up_residue(&self) -> std::result::Result<(), KvError> {
        // The preclaim-recovery bound, not the shutdown fixpoint's 16: a
        // crash-remount's residue includes the whole replay window (the
        // wedged pinned-floor shapes recover HERE now — remount IS
        // recovery), and every cycle is progress-audited (clause b), so
        // a genuine wedge fails loud long before the bound.
        const BRING_UP_COVER_CYCLES: u32 = 64;
        if self.read_only {
            return Ok(());
        }
        if TEST_BRING_UP_COVER_DISABLED.load(Ordering::Relaxed) {
            return Ok(());
        }
        {
            let core = self.ring.core();
            if core.head() == core.reusable_upto() {
                return Ok(());
            }
        }
        let mut smo = self.smo.lock().await;
        for _ in 0..BRING_UP_COVER_CYCLES {
            self.checkpoint_cycle(&mut smo, true).await?;
            let core = self.ring.core();
            if core.head() == core.reusable_upto() {
                return Ok(());
            }
        }
        Err(KvError::Corrupt(format!(
            "{}: bring-up journal residue did not cover within \
             {BRING_UP_COVER_CYCLES} barriered cycles (head={}, reusable_upto={}) — \
             refusing to serve with a reclaimable tail (the D1.b wedge-crumb class)",
            self.path.display(),
            self.ring.core().head(),
            self.ring.core().reusable_upto(),
        )))
    }

    /// PR M6 (design-metadata-throughput §5.4 D4): live pending-times
    /// refinements parked by the SETATTR-echo absorber — awaiting the
    /// next drain. A stats gauge and the entry-economy tests' probe.
    pub fn pending_times_len(&self) -> usize {
        self.pending_times_count.load(Ordering::Relaxed) as usize
    }

    /// PR M6: fold any pending (absorbed, not-yet-drained) times
    /// refinement over an inode value — **monotone max per field**, so a
    /// stale refinement (the kernel's coarse clock can stamp behind the
    /// daemon's fine-grained in-tx ctime) never regresses a fresher
    /// committed write. One relaxed load when the map is empty — the
    /// read-path common case.
    pub(super) fn fold_pending_times(&self, ino: Ino, v: &mut InodeValue) {
        if self.pending_times_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Some((pm, pc)) = self.pending_times.read_sync(&ino, |_, p| *p) {
            // Times are i64 ns carried in the u64 word (pre-epoch values
            // are representable — fstests generic/258); the newest-wins
            // fold must compare SIGNED or a backdated pre-epoch stamp
            // would out-rank every echo forever.
            if (pm as i64) > (v.mtime as i64) {
                v.mtime = pm;
            }
            if (pc as i64) > (v.ctime as i64) {
                v.ctime = pc;
            }
        }
    }

    /// Park a WRITE op's kernel-domain times stamp as a pending
    /// refinement (generic/003 remount-divergence fix, 2026-07-28): the
    /// FUSE write handler publishes ONE `coarse_realtime_ns` stamp to the
    /// attr cache, and THIS is how the same stamp becomes durable —
    /// fold-visible immediately, journaled by the batched drain. Layout
    /// persistence (`set_layout_and_size`) never authors times, so the
    /// served view and the remount view can never diverge by a clock
    /// tick. Monotone per-field (signed — i64 ns in the u64 word), like
    /// the fold: a parked stamp never regresses a fresher refinement.
    pub fn park_times_refinement(&self, ino: Ino, mtime: u64, ctime: u64) {
        match self.pending_times.entry_sync(ino) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let p = o.get_mut();
                if (mtime as i64) > (p.0 as i64) {
                    p.0 = mtime;
                }
                if (ctime as i64) > (p.1 as i64) {
                    p.1 = ctime;
                }
            }
            scc::hash_map::Entry::Vacant(slot) => {
                slot.insert_entry((mtime, ctime));
                self.pending_times_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.pending_times_count.load(Ordering::Relaxed) >= PENDING_TIMES_DRAIN_CAP {
            self.times_drain_wake.notify_one();
        }
    }

    /// Retire an ino's pending refinement (a committed inode write now
    /// carries — or supersedes — it). Callers hold the ino's DLM I-guard,
    /// so retirement never races an absorb.
    fn retire_pending_times(&self, ino: Ino) {
        if self.pending_times.remove_sync(&ino).is_some() {
            self.pending_times_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// PR M6: drain every parked pending-times refinement into batched
    /// journaled transactions (Δtime merge records for the inos the
    /// refinement still advances — destroyed / superseded inos are GC'd
    /// recordlessly), under per-ino DLM exclusive guards acquired through
    /// `lock_many`'s canonical order. Returns the number of refinements
    /// made durable. Called by the per-volume drain task on the flush
    /// cadence (cap crossings wake it early), by the fsync/unmount
    /// durability paths, and by tests.
    pub async fn drain_pending_times_now(&self) -> Result<u64> {
        let mut total = 0u64;
        loop {
            // Snapshot up to a batch of inos (scan stops at the cap) —
            // BEFORE the write gate, deliberately (rung-10 finding #4): a
            // drain with no work must touch no gate. On a CO-WRITER the
            // set is empty by construction (nothing parks locally — the
            // write path's park SHIPS), and the gate's CoWriterMount arm
            // counts the S8-b falsifier (`cowriter_local_commit_refusals`),
            // so the old gate-first order counted a false un-routed local
            // commit on EVERY co-writer fsync.
            let mut batch: Vec<Ino> = Vec::new();
            self.pending_times.iter_sync(|k, _| {
                batch.push(*k);
                batch.len() < PENDING_TIMES_DRAIN_BATCH
            });
            if batch.is_empty() {
                return Ok(total);
            }
            if self.write_gate().is_err() {
                // Failing / shutting-down / gated volume: refinements are
                // µs-grade time polish — never worth failing a barrier
                // path over. (With WORK pending on a gated co-writer
                // volume this is a REAL un-routed-surface signal, and the
                // gate's own counting arm records it.)
                return Ok(total);
            }
            let saw_full_batch = batch.len() >= PENDING_TIMES_DRAIN_BATCH;
            let lock_plan: Vec<(u64, LockMode)> = batch
                .iter()
                .map(|&ino| (ino, LockMode::Exclusive))
                .collect();
            let guards: Arc<[DlmGuard]> = Arc::from(self.dlm.lock_many(&lock_plan, &[]).await);

            let mut tx = KvTx::new();
            let mut records = 0u64;
            let mut drained: Vec<Ino> = Vec::with_capacity(batch.len());
            for &ino in &batch {
                // Re-read under the guard: a committed setattr may have
                // retired the entry between snapshot and lock.
                let Some((pm, pc)) = self.pending_times.read_sync(&ino, |_, p| *p) else {
                    continue;
                };
                match self.read_inode_value(ino).await? {
                    None => drained.push(ino), // destroyed: GC, no record
                    Some(v) => {
                        // SIGNED per-field advance (i64 ns in the u64
                        // word, matching `fold_pending_times`): an
                        // unsigned compare read a pre-epoch stored stamp
                        // as huge and silently dropped the refinement —
                        // the fold served it, the drain lost it.
                        let adv_m = (pm as i64) > (v.mtime as i64);
                        let adv_c = (pc as i64) > (v.ctime as i64);
                        if adv_m || adv_c {
                            // Stage only the advance (mtime is invariant-
                            // equal to stored for echo-born refinements;
                            // the times form is the belt).
                            let m = if adv_m { pm } else { v.mtime };
                            let c = if adv_c { pc } else { v.ctime };
                            let delta = if adv_m {
                                InodeDelta::times(m, c)
                            } else {
                                InodeDelta::ctime(c)
                            };
                            tx.stage_delta(TREE_INODES, inode_key(ino), &delta);
                            records += 1;
                        }
                        drained.push(ino);
                    }
                }
            }
            if !tx.is_empty() {
                // A drain failure keeps the refinements parked (the next
                // trigger retries); fsync-path callers surface the error.
                tx.hold_guards(guards.clone());
                self.commit_tx(tx).await?;
                super::META_KV_TIMES_ECHO_DRAIN_COMMITS.fetch_add(1, Ordering::Relaxed);
                super::META_KV_TIMES_ECHO_DRAINED.fetch_add(records, Ordering::Relaxed);
            }
            for ino in &drained {
                self.retire_pending_times(*ino);
            }
            total += records;
            if !saw_full_batch {
                return Ok(total);
            }
        }
    }

    /// Clean unmount: reject new mutations, drain in-flight commits, run
    /// a final checkpoint (tail == head ⇒ an empty replay window on the
    /// next mount) and JOIN the checkpoint task (no leaked tasks —
    /// `tests/dismount_teardown_tests.rs`). Idempotent.
    ///
    /// PR M1: guard teardown brackets the drain — the `writer_claim` is
    /// deleted first (while the write gate is still open; the final
    /// checkpoint makes the deletion durable), and the NVMe reservation
    /// is released last (control-plane, after the final barrier). A
    /// kill-9 skips both by construction: the claim is reclaimed by the
    /// dead-pid proof / TTL, the reservation by the successor's preempt.
    pub async fn shutdown(&self) -> std::result::Result<(), KvError> {
        // PR M6: make parked pending-times refinements durable while the
        // write gate is still open (best-effort — they are µs-grade time
        // polish; a failing volume loses them like a kill-9 would).
        if let Err(e) = self.drain_pending_times_now().await {
            log::warn!(
                "meta volume {}: clean unmount could not drain pending times \
                 refinements: {e} (dropped — µs-grade ctime polish only)",
                self.path.display()
            );
        }
        // D0: delete OUR claim exactly once — best-effort (a fenced or
        // failed volume cannot write; the claim then ages out by TTL).
        if self.claimed.swap(false, Ordering::AcqRel) && !self.is_failed() {
            if let Err(e) = self.removexattr_internal(1, WRITER_CLAIM_XATTR).await {
                log::warn!(
                    "meta volume {}: clean unmount could not delete the writer_claim: \
                     {e} (it will age out by TTL)",
                    self.path.display()
                );
            }
        }
        self.shutting_down.store(true, Ordering::Release);
        self.ring.wake_parked();
        self.ckpt_wake.notify_waiters();
        // PR M6: the drain task observes the flag on its wake and exits;
        // joining it keeps the no-leaked-tasks teardown contract.
        self.times_drain_wake.notify_waiters();
        let drain_done = self.times_drain_join.lock().unwrap().take();
        if let Some(done) = drain_done {
            match done.await {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return Err(KvError::Corrupt(
                        "pending-times drain task panicked".to_string(),
                    ));
                }
            }
        }
        let done = self.ckpt_join.lock().unwrap().take();
        if let Some(done) = done {
            // The task observes the flag, runs the final checkpoint, and
            // exits; awaiting its completion signal IS the drain. `false`
            // (or a dropped guard) = the task unwound before its final
            // cycle — same Corrupt surface the JoinHandle join gave.
            match done.await {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return Err(KvError::Corrupt(
                        "checkpoint task panicked during shutdown".to_string(),
                    ));
                }
            }
        } else {
            // Task already gone (second shutdown, or a drop raced): make
            // the final state durable ourselves. Failures latch the
            // volume rather than panic (the §4.4 pt 4 posture).
            self.ring.wait_completed_upto(self.ring.core().head()).await;
            self.checkpoint_now().await?;
        }
        // D0: release the Write Exclusive reservation after the final
        // barrier (nothing of ours writes past this point), then the
        // Layer A flock — a shut-down backend no longer excludes anyone
        // ("lives until shutdown/drop", §5.0; the still-referenced Arc
        // must not block the volume's next mount).
        self.release_reservation().await;
        drop(self.guard_fd.lock().unwrap().take());
        Ok(())
    }

    /// Whether the volume is shutting down (commits refuse; the
    /// checkpoint task exits after its final cycle).
    pub(super) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Whether the background checkpoint task is still alive (a `Weak`
    /// probe for the teardown tests: `upgrade()` fails once the task has
    /// exited and dropped its liveness token).
    pub fn checkpoint_alive_probe(&self) -> Weak<()> {
        self.ckpt_alive.lock().unwrap().clone()
    }

    /// Checkpoint-task plumbing (spawned by [`Self::open`]).
    pub(super) fn install_checkpoint_task(
        &self,
        done: squeezefs_ipc::sqz_channel::oneshot::Receiver<bool>,
        alive: Weak<()>,
    ) {
        *self.ckpt_join.lock().unwrap() = Some(done);
        *self.ckpt_alive.lock().unwrap() = alive;
    }

    /// PR M6: the pending-times drain task's wake (cap crossings + the
    /// shutdown broadcast).
    pub(super) fn times_drain_wake_handle(&self) -> Arc<squeezefs_ipc::sqz_notify::Notify> {
        self.times_drain_wake.clone()
    }

    pub(super) fn install_times_drain_task(
        &self,
        done: squeezefs_ipc::sqz_channel::oneshot::Receiver<bool>,
    ) {
        *self.times_drain_join.lock().unwrap() = Some(done);
    }

    /// R5 defense-in-depth gauge (follow-up C): the node cache's RAM
    /// estimate at its budget-accounting basis (cached nodes × node size)
    /// plus the dirty-node count — registered with the mem-budget
    /// authority so metadata RAM is visible to pressure accounting. The
    /// dhat attribution showed the KV core was NOT the flood, but "no
    /// per-component cap can see the sum" (§5.7) applies to it like every
    /// other consumer.
    pub fn node_cache_gauge(&self) -> (u64, u64) {
        // PR M9 (§5.7): the byte half is the cache's own budget gauge —
        // extents + overlay (records + folded heads) + snapshot memo
        // bytes — so the mem-budget `kv_node_cache` component sees the
        // same accounting `SQUEEZEFS_META_NODE_CACHE_MB` enforces.
        let mut dirty = 0u64;
        self.node_cache().for_each_node(|n| {
            if n.dirty_floor() != u64::MAX {
                dirty += 1;
            }
        });
        (self.node_cache().cached_bytes(), dirty)
    }

    /// R5 shed lever (never-lossy by construction): wake the checkpoint
    /// task NOW — an early flush/checkpoint tick, exactly the drain the
    /// cadence would run anyway. Never drops dirty state.
    pub fn kick_checkpoint(&self) {
        self.checkpoint_wake().notify_one();
    }

    pub(super) fn checkpoint_wake(&self) -> Arc<squeezefs_ipc::sqz_notify::Notify> {
        self.ckpt_wake.clone()
    }

    pub(super) fn take_needs_flush(&self) -> bool {
        self.needs_flush.swap(false, Ordering::AcqRel)
    }

    pub(super) fn node_cache(&self) -> &Arc<NodeCache> {
        &self.cache
    }

    pub(super) fn allocator(&self) -> &Arc<ExtentAllocator> {
        &self.alloc
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D4): the **dead-bset census**
    /// over this volume's RESIDENT leaf population — per leaf, total
    /// serialized records vs distinct keys ([`crate::meta_backend::kv::
    /// node_cache::NodeSnapshot::indexed_record_census`]); the difference
    /// is the superseded-record population a compaction fold reclaims.
    /// RAM-authoritative by design (the demand-paged node cache is the
    /// read surface): callers that need whole-volume coverage — the
    /// `--report-only` engine, the offline harness — page the trees in
    /// with a full range walk first (`squeezefs::defrag::measure` does).
    pub fn dead_bset_census(&self) -> DeadBsetCensus {
        let mut out = DeadBsetCensus::default();
        self.node_cache().for_each_node(|n| {
            if n.level() != 0 || n.state().is_superseded() {
                return;
            }
            let (total, distinct) = n.snapshot().indexed_record_census();
            out.leaves += 1;
            out.records_indexed += total;
            out.records_live += distinct;
            if total > distinct {
                out.candidates.push((n.tree_id(), n.addr()));
            }
        });
        out
    }

    /// PR VL7 (§5.7 D4): the **compaction nudge** — fold each candidate
    /// leaf through the EXISTING SMO compactor
    /// (`KvTree::compact_node_forced` → `smo_replace`), serialized with
    /// the checkpoint task through the per-volume SMO mutex (the same
    /// delegation `checkpoint_now` uses — lattice 4b). Journal-reserve /
    /// pending-free refusals run a checkpoint cycle and retry, exactly
    /// like the maintenance pass; a node that refuses past the bound
    /// fails loud. Returns the number of nodes actually compacted
    /// (vanished/superseded candidates no-op — the census is advisory,
    /// the SMO revalidates).
    pub async fn defrag_compact_nodes(
        &self,
        targets: &[(u8, u64)],
    ) -> std::result::Result<u64, KvError> {
        let mut smo = self.smo.lock().await;
        let trees = self.trees();
        let mut compacted = 0u64;
        for &(tree_id, addr) in targets {
            let Some(tree) = trees.iter().find(|t| t.tree_id() == tree_id) else {
                continue; // unknown tree id: stale/foreign census entry
            };
            let mut out = crate::meta_backend::kv::tree::MaintenanceOutcome::default();
            let mut attempts = 0;
            loop {
                match tree.compact_node_forced(&mut smo, addr, &mut out).await {
                    Ok(did) => {
                        if did {
                            compacted += 1;
                        }
                        break;
                    }
                    Err(KvError::JournalReserveExhausted { .. })
                    | Err(KvError::PendingFreeFull { .. })
                        if attempts < 4 =>
                    {
                        attempts += 1;
                        self.checkpoint_cycle(&mut smo, true).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(compacted)
    }
}

/// PR VL7 (§5.7 D4): one volume's dead-bset census — the measurement half
/// of the D4 axis. `records_indexed − records_live` is the reclaimable
/// dead-record population; `candidates` are the leaves a
/// [`KvMetaBackend::defrag_compact_nodes`] nudge folds.
#[derive(Debug, Default, Clone)]
pub struct DeadBsetCensus {
    /// Resident (non-superseded) leaves censused.
    pub leaves: u64,
    /// Total serialized records across those leaves' bset logs.
    pub records_indexed: u64,
    /// Distinct keys (the post-fold live population).
    pub records_live: u64,
    /// `(tree_id, node_addr)` of every leaf carrying dead records.
    pub candidates: Vec<(u8, u64)>,
}

// ---------------------------------------------------------------------------
// D0 — the single-writer mount guard (design-metadata-throughput §5.0):
// Layer A dedicated-fd flock (same host), Layer B1 NVMe Persistent
// Reservations (cross host, enforcement), Layer B2 `writer_claim` record
// (identity + detection). PR M1.
// ---------------------------------------------------------------------------

/// Layer A refusal classes: the lock is held (another live writer on this
/// host) vs a real I/O error opening/locking the node.
enum FlockOutcome {
    Held,
    Io(std::io::Error),
}

/// Why a mounted volume withholds mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyCause {
    /// Writable (the write gate passes).
    Writable,
    /// §4.11: the superblock carries unknown read-only feature bits — a
    /// forward-compatibility degradation of a WRITE mount (Layer A is
    /// still held; no claim is written because this mount cannot advance
    /// the journal).
    UnknownRoFeatureBits,
    /// DLM S5: an operator-requested **reader mount** (`-o ro` /
    /// `--read-only`). Takes no Layer-A lock, writes no `writer_claim`,
    /// registers no PR key, spawns no checkpoint task — and therefore
    /// neither weakens nor blocks the single-writer guard.
    ReaderMount,
    /// DLM S9: a **co-writer mount** (`SQUEEZEFS_MW_ROLE=co-writer`, past
    /// the five-rung admission ladder). The metadata plane is read-only
    /// *locally* — every mutation ships to the volume's authority — while
    /// the DATA plane is read-write under a granted custody lease. Like a
    /// reader it takes no Layer-A lock, writes no `writer_claim`, registers
    /// no PR key on the metadata namespace and spawns no checkpoint task;
    /// unlike a reader its refusal points at the shipped publish path
    /// rather than at a mount option.
    CoWriterMount,
    /// **Per-volume claim admission** (§5.1.2): a PEER authority of this
    /// same set appends to THIS volume, under an admission decided before
    /// the open.
    ///
    /// Deliberately NOT [`ReadOnlyCause::CoWriterMount`]. A partial
    /// authority *does* hold metadata authority — over the volumes it
    /// owns — so the co-writer refusal's *"holds NO metadata authority
    /// over it"* would be false, and folding the two would rot the
    /// meaning of `cowriter_local_commit_refusals`, whose whole job is to
    /// say that a co-writer's daemon surface is un-routed.
    PeerOwnedVolume,
}

/// The reader's Layer-A classification (DLM S5). `flock(LOCK_SH)` is taken
/// as a PROBE and released immediately — see
/// `KvMetaBackend::probe_shared_lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedProbe {
    /// No exclusive holder on this host: no local write mount, no offline
    /// guarded verb running.
    NoLocalExclusiveHolder,
    /// An exclusive holder (a local write mount, `claim clear`, `format`,
    /// …) holds the volume on this host. Purely informational for a
    /// reader — it changes nothing about what the reader may do.
    LocalExclusiveHolder,
    /// The probe itself could not run (permissions, I/O). Never fatal: a
    /// reader's admission does not depend on it.
    Unknown,
}

/// Compose the `(claim: id=…, pid=…, boot=…, age=…s)` holder suffix for
/// refusal messages (design §6: refusals name the holder).
fn holder_suffix(holder: &Option<(WriterClaim, u64)>) -> String {
    match holder {
        Some((c, now)) => format!(
            " (claim: id={}, pid={}, boot={}, age={}s)",
            c.id,
            c.pid,
            c.boot,
            c.age_secs(*now)
        ),
        None => String::new(),
    }
}

/// This boot's id, empty when unreadable (non-Linux dev shells).
///
/// `pub` since DLM S6: the membership plane's records carry the SAME boot
/// scoping as `writer_claim` (pid proofs are only meaningful inside one
/// boot), and two definitions of "this boot" would be two answers.
pub fn read_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Unix seconds now (the claim heartbeat clock).
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The same-host dead-pid proof's pid half: `kill(pid, 0) == ESRCH`.
/// `EPERM` (alive, foreign uid) and success (alive, ours) are NOT proof.
/// `pub(crate)`: the claim-set prune (rung-8 finding #3) runs the SAME
/// proof — one dead-holder law, never a second spelling.
pub(crate) fn pid_provably_dead(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 probes process existence without delivering.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// The Layer B2 mount-gate classification of the replayed claim evidence.
#[derive(Debug)]
enum ClaimEvidence {
    /// No claim (or our own residue / a dead same-host holder): proceed.
    Reclaimable,
    /// A heartbeat-fresh claim from a holder we cannot prove dead.
    FreshForeign(WriterClaim),
    /// TTL-stale (or unattributable) claim we did not write and cannot
    /// dead-pid-prove: PR volumes preempt it; non-PR volumes refuse
    /// (operator attestation only).
    StaleForeign(Option<WriterClaim>),
    /// **Per-volume claim admission** (§5.1.1): a heartbeat-fresh claim
    /// held by the node this volume's durable `claim_set` names as its
    /// owner, opened under a [`crate::partial_authority::SetAdmission`]
    /// taken BEFORE the open.
    ///
    /// Not a weakening of [`ClaimEvidence::FreshForeign`] — a different
    /// door. It is produced only when a [`PeerAuthorityWitness`] is
    /// passed, which only [`KvMetaBackend::open_peer_owned`] does, so
    /// every other caller's classification is bit-identical.
    PeerAuthority(WriterClaim),
}

/// What [`KvMetaBackend::classify_claim`]'s `PeerAuthority` arm recognizes
/// a live holder by (§5.1.1's `recognizes`, with PR 3's correction).
///
/// `WriterClaim.id` is a per-mount uuid, so the recognition is against the
/// holder's **durable** member id: the volume's own `claim_set` attests it
/// ([`crate::membership::ClaimHolder`]), and the admission says which
/// durable identity it decided was appending here. An unattested claim
/// resolves to nothing and is therefore NOT recognized — silence refuses,
/// it never adopts (KD-PV-3).
struct PeerAuthorityWitness<'a> {
    /// The durable member id the admission's `VolumeMode::Peer` names.
    admitted_holder: &'a str,
    /// The volume's durable claim set, as replayed at this very open.
    claim_set: Option<&'a crate::membership::ClaimSet>,
}

impl PeerAuthorityWitness<'_> {
    /// Is `claim` the claim of the durable identity this admission
    /// admitted? Re-checked against the record actually replayed, never
    /// against the gather's copy.
    fn recognizes(&self, claim: &WriterClaim) -> bool {
        self.claim_set
            .and_then(|s| s.resolve_holder(claim))
            .is_some_and(|holder| {
                crate::membership::member_id_matches(holder, self.admitted_holder)
                    || crate::membership::member_id_matches(self.admitted_holder, holder)
            })
    }
}

impl KvMetaBackend {
    /// Take the Layer A lock: `flock(LOCK_EX | LOCK_NB)` on a dedicated
    /// `std::fs::File` (design §5.0 — a `uring_fs` cache fd would drop
    /// the lock at LRU eviction).
    fn acquire_writer_flock(path: &Path) -> std::result::Result<std::fs::File, FlockOutcome> {
        let fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(FlockOutcome::Io)?;
        // SAFETY: flock on an owned, open fd; NB never blocks.
        let rc = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                FlockOutcome::Held
            } else {
                FlockOutcome::Io(e)
            });
        }
        Ok(fd)
    }

    /// **DLM S5 — the reader's Layer-A probe** (`flock(LOCK_SH | LOCK_NB)`,
    /// spec §6.8 item 1).
    ///
    /// Taken and **released immediately**: the reader retains no lock for
    /// the mount's lifetime. That is the whole design decision, and it is
    /// deliberate — a RETAINED `LOCK_SH` conflicts with the writer's
    /// `LOCK_EX`, so an attached reader would refuse a legitimate write
    /// mount on the same host (and, order-reversed, a write mount would
    /// refuse every local reader). The D0 guarantee is that a second
    /// **writer** is refused; a reader must be ORTHOGONAL to it, not a hole
    /// in it and not a tax on it. A reader mutates no plane, so it needs no
    /// exclusion and grants none — pinned by
    /// `tests/readonly_mount_tests.rs::a_reader_never_refuses_a_writer_mount`.
    ///
    /// What the probe still buys, and why it is not merely skipped: the
    /// shared-mode acquisition is the ONE local question a reader can
    /// answer for free — whether an exclusive holder (a write mount, or an
    /// offline guarded verb like `format` / `claim clear`) is running on
    /// THIS host. That classification goes in the mount log beside the
    /// guarantee class, so an operator reading a reader's log knows whether
    /// the writer it lags behind is local or remote.
    fn probe_shared_lock(path: &Path) -> SharedProbe {
        let fd = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(fd) => fd,
            Err(e) => {
                log::warn!(
                    "{}: read-only mount could not open the volume for the shared-lock \
                     probe ({e}) — proceeding (a reader's admission never depends on it)",
                    path.display()
                );
                return SharedProbe::Unknown;
            }
        };
        // SAFETY: flock on an owned, open fd; NB never blocks. LOCK_UN
        // below releases before the fd is dropped, so nothing of this
        // probe outlives the call — no lock is retained.
        let rc = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                libc::LOCK_SH | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                SharedProbe::LocalExclusiveHolder
            } else {
                SharedProbe::Unknown
            };
        }
        // SAFETY: same owned fd; releasing a lock we hold.
        unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                libc::LOCK_UN | libc::LOCK_NB,
            );
        }
        SharedProbe::NoLocalExclusiveHolder
    }

    /// Layer A teardown-race absorption (2026-07-26; pinned by
    /// `tests/mount_writer_guard_tests.rs::
    /// test_reopen_waits_out_same_process_teardown_pin`): when the flock
    /// holder's claim names THIS process (pid + boot) — a backend whose
    /// last external `Arc` was dropped but whose struct is still pinned
    /// by a mid-pass background task, OR a genuinely live same-process
    /// double mount — poll the flock for a bounded window. A dying
    /// holder frees it within one pass (ms-grade); a live one never does
    /// and the caller falls back to the loud refusal. Returns the
    /// acquired guard fd, or `None` (refuse) on any other holder, an
    /// absent/unreadable claim, or bound expiry.
    async fn await_same_process_teardown_flock(
        path: &Path,
        holder: &Option<(WriterClaim, u64)>,
    ) -> Option<std::fs::File> {
        /// Generous vs. the ms-grade pin (one checkpoint/conveyor pass);
        /// a live same-process holder pays it once before the refusal.
        const TEARDOWN_FLOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
        const POLL: std::time::Duration = std::time::Duration::from_millis(5);

        let (claim, _) = holder.as_ref()?;
        let same_process = claim.pid == std::process::id() && claim.boot == read_boot_id();
        // Rung-9 finding #5 (the S8-b E2 leg, live): a successor mounting
        // over a PROVABLY-DEAD same-host holder can meet a *transient*
        // `LOCK_SH` at its one-shot NB acquire — a reader's / co-writer's
        // released-immediately mount probe (5 rejoining co-writers hammer
        // them while the authority is dark). The dead holder cannot own
        // the flock and SH probes release by contract, so this shape gets
        // the SAME bounded wait-out as the same-process teardown race. A
        // LIVE same-boot holder and every foreign-boot claim still refuse
        // instantly (unchanged posture).
        let dead_same_host = claim.boot == read_boot_id() && pid_provably_dead(claim.pid);
        if !same_process && !dead_same_host {
            return None; // live/foreign holder: refuse instantly (unchanged posture)
        }
        let deadline = std::time::Instant::now() + TEARDOWN_FLOCK_WAIT;
        loop {
            match Self::acquire_writer_flock(path) {
                Ok(fd) => return Some(fd),
                Err(FlockOutcome::Held) if std::time::Instant::now() < deadline => {
                    squeezefs_ipc::sqz_time::sleep(POLL).await;
                }
                Err(_) => return None,
            }
        }
    }

    /// Best-effort claim read for refusal messages only (a read-only probe
    /// of the possibly-live volume — the preflight-sanctioned shape).
    async fn probe_claim_best_effort(path: &Path) -> Option<(WriterClaim, u64)> {
        let probe = Self::open_probe(path).await.ok()?;
        let claim = probe.read_writer_claim().await?;
        Some((claim, unix_now_secs()))
    }

    /// Append a guard event to this backend's open trace (also called by
    /// `spawn_checkpoint_task` — the ordering assertion's spawn hook).
    pub(super) fn trace_guard_event(&self, ev: &'static str) {
        self.guard_trace.lock().unwrap().push(ev);
    }

    /// Classify the replayed claim evidence for the mount gate.
    ///
    /// `witness` is `None` for every caller but the per-volume peer open
    /// (§5.1.1): without it the freshness branch answers
    /// [`ClaimEvidence::FreshForeign`] exactly as it always has, which is
    /// the solo re-gate law (R12) expressed in one parameter.
    fn classify_claim(
        &self,
        raw: Option<Vec<u8>>,
        now: u64,
        witness: Option<&PeerAuthorityWitness<'_>>,
    ) -> ClaimEvidence {
        let Some(raw) = raw else {
            return ClaimEvidence::Reclaimable; // absent: first guard-aware mount claims it
        };
        let Some(claim) = WriterClaim::decode(&raw) else {
            // Unattributable bytes prove nothing — never auto-taken.
            return ClaimEvidence::StaleForeign(None);
        };
        let same_host = !self.boot_id.is_empty() && claim.boot == self.boot_id;
        if same_host && claim.pid == std::process::id() {
            // Our own residue (drop-without-shutdown in this process).
            return ClaimEvidence::Reclaimable;
        }
        if same_host && pid_provably_dead(claim.pid) {
            // The dead-pid proof: boot matches this boot AND the pid is
            // provably gone AND the flock was free (we hold it) — instant
            // reclaim regardless of heartbeat age (kill -9 leaves a fresh
            // claim; "no wait ever", §5.0 R6).
            log::info!(
                "meta volume {}: reclaiming writer_claim from dead same-host holder \
                 (id={}, pid={} ESRCH, age={}s)",
                self.path.display(),
                claim.id,
                claim.pid,
                claim.age_secs(now)
            );
            return ClaimEvidence::Reclaimable;
        }
        if claim.age_secs(now) <= crate::fuse_client::CLIENT_STALE_TTL_SECS {
            match witness {
                Some(w) if w.recognizes(&claim) => ClaimEvidence::PeerAuthority(claim),
                // Byte-identical to the pre-program arm — including for a
                // witness that does NOT recognize the holder, which is
                // §5.10's fail-closed direction rather than a new class.
                _ => ClaimEvidence::FreshForeign(claim),
            }
        } else {
            ClaimEvidence::StaleForeign(Some(claim))
        }
    }

    /// The §5.0 mount gate: claim decision (B2), PR acquisition (B1), and
    /// the claim commit + barrier — all **before** the checkpoint task
    /// exists and before any FUSE arm.
    async fn writer_guard_gate(self: &Arc<Self>) -> std::result::Result<(), KvError> {
        if self.read_only {
            // §4.11 read-only mounts withhold every mutation — including
            // the claim. Layer A still guards; B1/B2 do not apply (the
            // volume cannot advance its journal from this mount).
            log::warn!(
                "meta volume {}: read-only mount — single-writer guard is flock-only \
                 (no writer_claim is written)",
                self.path.display()
            );
            return Ok(());
        }
        let now = unix_now_secs();
        let raw = self
            .getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .map_err(KvError::Io)?;
        // DLM S2: the predecessor's era, read from the same replayed
        // record the classification consumes (an unparseable claim
        // proves nothing — including nothing about the era, so it
        // contributes 0 and the never-deleted term record carries the
        // ladder).
        let prior_claim_term = raw
            .as_deref()
            .and_then(WriterClaim::decode)
            .map(|c| c.term)
            .unwrap_or(0);
        // The D0 write mount never passes a witness: `PeerAuthority` is
        // structurally unreachable from this gate (R12).
        let evidence = self.classify_claim(raw, now, None);

        match (&evidence, &self.reservations) {
            // Fresh foreign holders refuse on every substrate — the PR
            // acquire would also conflict, but the claim names the holder.
            (ClaimEvidence::FreshForeign(c), _) => {
                return Err(KvError::Busy(format!(
                    "{}: metadata volume is claimed by a live writer{} — concurrent \
                     mounts of one metadata volume are refused (single-writer guard). \
                     A crashed holder on THIS host is reclaimed automatically once its \
                     pid is provably dead; otherwise stop that writer or wait for its \
                     claim to expire (ttl {}s)",
                    self.path.display(),
                    holder_suffix(&Some((c.clone(), now))),
                    crate::fuse_client::CLIENT_STALE_TTL_SECS,
                )));
            }
            // Stale/unattributable foreign WITHOUT device enforcement:
            // automatic cross-host takeover is disabled (a paused holder
            // cannot be detected on a RAM-authoritative backend) —
            // operator attestation only.
            (ClaimEvidence::StaleForeign(c), None) => {
                let named = match c {
                    Some(c) => holder_suffix(&Some((c.clone(), now))),
                    None => " (unparseable claim value)".to_string(),
                };
                return Err(KvError::Busy(format!(
                    "{}: stale writer claim{} from another holder — automatic cross-host \
                     takeover is disabled on volumes without NVMe Persistent Reservations \
                     (a paused holder cannot be detected); verify the holder is down, then \
                     run `squeezefs claim clear <sqmeta-uri>` (operator attestation). The \
                     claim is never auto-taken",
                    self.path.display(),
                    named,
                )));
            }
            // Unreachable without a witness, and a witness is only ever
            // built by the peer-owned open, which does not run this gate.
            (ClaimEvidence::PeerAuthority(c), _) => {
                return Err(KvError::Corrupt(format!(
                    "{}: the D0 write gate classified a claim held by '{}' as a PEER                      AUTHORITY's — the per-volume admission arm reached the single-writer                      ladder, which must never happen (R12: it is a different door, not a                      hole in this one)",
                    self.path.display(),
                    c.id
                )));
            }
            // Stale foreign WITH enforcement: B1's preempt arbitrates
            // below (safe — the device fences the victim).
            (ClaimEvidence::StaleForeign(_), Some(_)) | (ClaimEvidence::Reclaimable, _) => {}
        }

        // Layer B1: register + acquire Write Exclusive on PR volumes.
        if let Some(rsv) = self.reservations.clone() {
            match rsv_call(&rsv, |c| c.host_identity()).await {
                Ok(id) => {
                    // Recorded once; the heartbeat re-check verifies
                    // stability against exactly this value.
                    let _ = self.pr_identity.set(id);
                }
                Err(e) => {
                    log::warn!(
                        "meta volume {}: host identity unreadable ({e}) — PR guard \
                         proceeds; identity stability cannot be verified this session",
                        self.path.display()
                    );
                }
            }
            let key = self.pr_key;
            // The register ladder (spec-strict targets, SPDK P0 finding):
            // plain register is the fast path; a conflict with OUR OWN
            // stale registration (kill-9'd incarnation, same host) is
            // recovered report→unregister-own→register; foreign
            // registrations are never touched (fail-closed into the
            // acquire-conflict arbitration below).
            let registered = rsv_call(&rsv, move |c| {
                crate::meta_backend::reservation::register_ladder(c, key)
            })
            .await
            .map_err(|e| self.pr_error("reservation register", e))?;
            if let crate::meta_backend::reservation::RegisterOutcome::RecoveredOwnStale {
                unregistered,
            } = &registered
            {
                log::warn!(
                    "meta volume {}: reservation register conflicted with our own stale \
                     registration(s) {unregistered:#018x?} — a crashed incarnation's \
                     residue on a spec-strict target (SPDK-class Register semantics); \
                     unregistered them and registered fresh (single-writer guard \
                     register ladder)",
                    self.path.display()
                );
            }
            let acquire = rsv_call(&rsv, move |c| c.acquire_write_exclusive(key)).await;
            match acquire {
                Ok(()) => {}
                Err(e) if crate::meta_backend::reservation::is_reservation_conflict(&e) => {
                    // Arbitration: the evidence here is Reclaimable or
                    // StaleForeign (fresh refused above) — preempt the
                    // holder key (device-fenced takeover).
                    let report = rsv_call(&rsv, |c| c.report())
                        .await
                        .map_err(|e| self.pr_error("reservation report", e))?;
                    let Some(victim) = report.holder_key else {
                        return Err(KvError::Busy(format!(
                            "{}: reservation acquire conflicted but the report names no \
                             holder — refusing to arbitrate blind (single-writer guard)",
                            self.path.display()
                        )));
                    };
                    rsv_call(&rsv, move |c| c.preempt(key, victim))
                        .await
                        .map_err(|e| self.pr_error("reservation preempt", e))?;
                    log::warn!(
                        "meta volume {}: preempted stale reservation holder key {victim:#018x} \
                         (device-fenced takeover; writer_claim evidence was stale/absent)",
                        self.path.display()
                    );
                }
                Err(e) => return Err(self.pr_error("reservation acquire", e)),
            }
            self.pr_active.store(true, Ordering::Release);

            // §5.4 (design-full-multi-writer) — the shared-identity
            // gauge, computed from ACTUAL identities (the device's
            // report + this association's wire host id, never configured
            // strings): another registration under OUR host identifier
            // means the device sees ONE host for two holders, so fencing
            // between those mounts is process-local, not
            // device-enforced. Loud warning + `pr_registrant_shared`.
            let shared_probe = async {
                let report = rsv_call(&rsv, |c| c.report()).await.ok()?;
                let wire = rsv_call(&rsv, |c| c.wire_host_id()).await.ok()?;
                Some(
                    crate::meta_backend::reservation::registrant_identity_shared(
                        &report, key, &wire,
                    ),
                )
            }
            .await;
            if let Some(true) = shared_probe {
                log::warn!(
                    "meta volume {}: the device reports ANOTHER registration under this \
                     association's host identifier — a co-located mount is sharing this \
                     mount's hostnqn/hostid pair, so fencing between the two is \
                     PROCESS-LOCAL, not device-enforced (design-full-multi-writer §5.4). \
                     Give each mount its own identity (SQUEEZEFS_HOSTNQN/SQUEEZEFS_HOSTID \
                     or -o hostnqn=/hostid=) to restore the device-enforced class",
                    self.path.display()
                );
                crate::fuse_client::METRICS
                    .pr_registrant_shared
                    .store(1, Ordering::Relaxed);
            }
        }

        // Ring-recovery preflight (the preserved 2026-07-26 md-storm
        // image's mount-refusal face, P2 §9): a crash with a pinned tail
        // can leave a replay window that exhausts the user-admissible
        // ring slice. The claim commit below is the volume's FIRST
        // post-replay mutation and the checkpoint task does not exist
        // yet — a parked claim has NO drain source and can only escalate
        // through the journal-failure lattice into a mount refusal, on a
        // volume that is perfectly healthy on disk. We hold the flock,
        // the PR (where capable), and the B2 decision — we ARE the
        // writer — so run bounded barriered recovery cycles inline until
        // one max-size user entry admits. Ordering note (M1 §5.0 B2):
        // "no maintenance record can precede the claim" pins the
        // *checkpoint task spawn* after the claim barrier; these inline
        // cycles are the mount's own guarded writes (checkpoint-class,
        // DLM-exempt by architecture) and a fenced holder still
        // fail-stops at their barriers — the guard ladder is unchanged.
        self.preclaim_ring_recovery().await?;

        // DLM S2 (spec §6.7 decision 4 / §6.9): bump the durable writer
        // term past every predecessor's. Resolution only — the record
        // rides the claim's own transaction below, so the gate still
        // makes exactly ONE pre-barrier mutation and the two records can
        // never diverge across a crash.
        let term = self.resolve_writer_term(prior_claim_term).await?;

        // Layer B2: commit our claim and make it durable — the volume's
        // first post-replay mutation, BEFORE the checkpoint task exists.
        let claim = WriterClaim {
            id: self.writer_id.clone(),
            ts: unix_now_secs(),
            pid: std::process::id(),
            boot: self.boot_id.clone(),
            term,
        };
        self.commit_claim_tx(&claim).await?;
        self.claimed.store(true, Ordering::Release);
        self.trace_guard_event("claim_committed");
        self.sync_device().await.map_err(KvError::Io)?;
        self.trace_guard_event("claim_barriered");
        // Publish AFTER the barrier: the process mint may only compose
        // tokens from an era that is on disk.
        self.writer_term.store(term, Ordering::Release);
        if term != 0 {
            crate::dlm::adopt_durable_term(term);
        }
        log::info!(
            "meta volume {}: writer claim taken (id={}, term={}, mode={})",
            self.path.display(),
            claim.id,
            term,
            self.writer_guard_mode()
        );
        Ok(())
    }

    /// The Layer B2 claim commit: the `writer_claim` record and — on
    /// bit-7 volumes — the DLM S2 durable term record, staged into ONE
    /// `KvTx` so the gate keeps making exactly one pre-barrier mutation
    /// (design §5.0 B2: "the claim tx is the volume's first post-replay
    /// mutation, and no maintenance record can precede it") and the era
    /// ladder can never diverge from the claim that names it — one
    /// checksummed journal entry, whole-tx atomic, torn-immune.
    async fn commit_claim_tx(&self, claim: &WriterClaim) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(1).await]);
        let tx0 = KvTx::new();
        let (_existing, claim_key) = self.xattr_slot(&tx0, 1, WRITER_CLAIM_XATTR).await?;
        let mut tx = tx0;
        tx.stage_put(
            TREE_XATTRS,
            claim_key,
            XattrValue::encode_parts(WRITER_CLAIM_XATTR.as_bytes(), &claim.encode())?,
        );
        if claim.term != 0 {
            // Read-your-own-writes: the slot probe sees the staged claim
            // (different name ⇒ different slot; the overlay keeps the
            // collision chain honest).
            let (_existing, term_key) = self.xattr_slot(&tx, 1, WRITER_TERM_XATTR).await?;
            tx.stage_put(
                TREE_XATTRS,
                term_key,
                XattrValue::encode_parts(
                    WRITER_TERM_XATTR.as_bytes(),
                    &encode_writer_term(claim.term),
                )?,
            );
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// DLM S2: resolve this mount's writer term. `prior_claim_term` is
    /// the era carried by the replayed `writer_claim` the gate already
    /// read; the resolved term is COMMITTED (and barriered) by
    /// [`Self::commit_claim_tx`] before the guard arms.
    ///
    /// The successor's term is `max(claim.term, writer_term record) + 1`.
    /// Both sources are consulted because they have different lifetimes:
    /// the claim is DELETED at clean unmount (the volume must present as
    /// unclaimed), while [`WRITER_TERM_XATTR`] is never deleted — so the
    /// era ladder survives clean cycles as well as crashes.
    ///
    /// Returns `0` on volumes without incompat bit 7: no record is
    /// written, the claim carries no `term` key, and every token this
    /// mount mints is the bare grant sequence (pre-S2 behavior).
    async fn resolve_writer_term(
        &self,
        prior_claim_term: u64,
    ) -> std::result::Result<u64, KvError> {
        if !self.durable_term_enabled {
            return Ok(0);
        }
        let stored = match self
            .getxattr(1, WRITER_TERM_XATTR)
            .await
            .map_err(KvError::Io)?
        {
            None => 0,
            Some(val) => decode_writer_term(&val).ok_or_else(|| {
                KvError::Corrupt(format!(
                    "{}: the durable writer-term record ({WRITER_TERM_XATTR}) is unparseable \
                     ({}) — refusing to mount rather than reset the fencing era, which would \
                     re-issue tokens a predecessor already used (DLM S2, spec §6.11)",
                    self.path.display(),
                    String::from_utf8_lossy(&val),
                ))
            })?,
        };
        let prior = prior_claim_term.max(stored);
        if prior >= crate::dlm::TERM_MAX {
            return Err(KvError::Corrupt(format!(
                "{}: writer term space exhausted (prior term {prior} of a {}-bit budget, max \
                 {}) — refusing to mount rather than roll the era over, which would alias this \
                 mount's fencing tokens with a retired era's; reformat required (DLM S2)",
                self.path.display(),
                64 - crate::dlm::GRANT_SEQ_BITS,
                crate::dlm::TERM_MAX,
            )));
        }
        Ok(prior + 1)
    }

    /// This mount's durable writer term on this volume (0 = un-stamped
    /// volume, probe, or read-only mount — see [`WRITER_TERM_XATTR`]).
    pub fn writer_term(&self) -> u64 {
        self.writer_term.load(Ordering::Acquire)
    }

    /// The mount gate's ring-recovery preflight (see the call site in
    /// [`Self::writer_guard_gate`]): when the recovered replay window
    /// leaves less than one max-size user entry admissible, run bounded
    /// barriered checkpoint cycles inline — each flushes replayed dirt
    /// (forced retirements break the §4.7 pinned-floor cycle), advances
    /// the tail, and reclaims ring pages — until the preflight admission
    /// succeeds. The probe admission is released immediately (nothing is
    /// reserved); the bound is generous because each barriered cycle is
    /// audited for progress (`checkpoint_cycle`'s clause-b rung), so a
    /// genuinely wedged tail fails loud long before the bound with a
    /// named cause — never a 30 s-per-rung silent park.
    async fn preclaim_ring_recovery(&self) -> std::result::Result<(), KvError> {
        const PRECLAIM_RECOVERY_CYCLES: u32 = 64;
        // One max-size user entry, clamped to what this ring can EVER
        // admit (a floor-size ring's user slice is slightly under
        // MAX_ENTRY_LEN once page-header slots are excluded — the clamp
        // keeps the preflight satisfiable-by-drained-ring on every legal
        // geometry, so a healthy volume can never be refused here).
        let geo = self.ring.core().geometry();
        let need =
            super::journal::MAX_ENTRY_LEN.min(geo.logical_len().saturating_sub(geo.reserve_bytes));
        let preflight = || self.ring.try_admit(need, AdmissionClass::User);
        if let Some(adm) = preflight() {
            self.ring.core().release(adm);
            return Ok(());
        }
        let core = self.ring.core();
        log::warn!(
            "meta volume {}: recovered replay window exhausts the journal ring \
             (head={}, reusable_upto={}) — running pre-claim recovery checkpoint \
             cycles (the mount-refusal wedge face, P2 2026-07-26 §9)",
            self.path.display(),
            core.head(),
            core.reusable_upto(),
        );
        let mut smo = self.smo.lock().await;
        for cycle in 0..PRECLAIM_RECOVERY_CYCLES {
            self.checkpoint_cycle(&mut smo, true).await?;
            if let Some(adm) = preflight() {
                self.ring.core().release(adm);
                log::info!(
                    "meta volume {}: pre-claim ring recovery converged after {} cycle(s)",
                    self.path.display(),
                    cycle + 1
                );
                return Ok(());
            }
        }
        Err(KvError::Corrupt(format!(
            "{}: pre-claim ring recovery did not reclaim admissible space within \
             {PRECLAIM_RECOVERY_CYCLES} barriered cycles (head={}, reusable_upto={}) — \
             the durable tail is wedged below the replay window",
            self.path.display(),
            self.ring.core().head(),
            self.ring.core().reusable_upto(),
        )))
    }

    fn pr_error(&self, what: &str, e: std::io::Error) -> KvError {
        KvError::Busy(format!(
            "{}: {what} failed on the PR-capable namespace: {e} (single-writer guard)",
            self.path.display()
        ))
    }

    /// Release the Write Exclusive reservation exactly once (clean
    /// unmount / failed mount teardown). Best-effort control-plane call.
    async fn release_reservation(&self) {
        if !self.pr_active.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(rsv) = self.reservations.clone() {
            let key = self.pr_key;
            match rsv_call(&rsv, move |c| c.release(key)).await {
                Ok(()) => log::debug!("meta volume {}: reservation released", self.path.display()),
                Err(e) => log::warn!(
                    "meta volume {}: reservation release failed: {e} (a successor \
                     preempts it via the TTL-stale rule)",
                    self.path.display()
                ),
            }
        }
    }

    /// The guarantee class this volume actually mounted with
    /// (`writer_guard_mode` on the stats surface, design §9):
    /// `"flock+pr"` (PR-capable namespace — enforcement-grade cross-host),
    /// `"flock+claim"` (detection-grade cross-host), `"flock"` (a WRITE
    /// mount degraded read-only by unknown-ro feature bits: Layer A held,
    /// no claim written), **`"reader"`** (DLM S5 `-o ro`: no lock retained,
    /// no claim, no PR registrant — orthogonal to the writer's exclusion,
    /// see `docs/operations.md` §Read-only coherent mounts), or
    /// `"unguarded"` (probe backends — never mounted, never in stats).
    pub fn writer_guard_mode(&self) -> &'static str {
        // The reader row is decided by CAUSE, not by lock state: it holds
        // no lock by design, and reporting it as `unguarded` (the probe
        // class) would hide a live, FUSE-serving mount inside a class that
        // means "not a mount at all".
        if self.ro_cause == ReadOnlyCause::ReaderMount {
            return "reader";
        }
        // DLM S9: same reasoning as the reader row — decided by CAUSE, not
        // by lock state. A co-writer is a live, FUSE-serving, DATA-WRITING
        // mount that holds no lock and no claim on this volume, so it is
        // its own guarantee class in docs/operations.md: exclusion of a
        // second *appender* is still the authority's `LOCK_EX` + claim +
        // PR, and this mount's write access is the authority's custody
        // grant plus the data namespaces' WERO registration.
        if self.ro_cause == ReadOnlyCause::CoWriterMount {
            return "co-writer";
        }
        // Per-volume claim admission (§11.1): the same cause-not-lock-state
        // reasoning again. A partial authority shows a MIX of rows across
        // the set — uniform `peer-owned` means the node owns nothing and
        // should be mounted as a co-writer, which the ladder's rung 3
        // refuses on its behalf.
        if self.ro_cause == ReadOnlyCause::PeerOwnedVolume {
            return "peer-owned";
        }
        let guarded = self.guard_fd.lock().unwrap().is_some();
        match (guarded, &self.reservations) {
            (false, _) => "unguarded",
            (true, Some(_)) => "flock+pr",
            (true, None) if self.read_only => "flock",
            (true, None) => "flock+claim",
        }
    }

    /// Reservation-conflict-class barrier failures and usurpation-class
    /// re-check outcomes mapped to guard fail-stop (`writer_guard_fenced`,
    /// design §9): a fenced/usurped holder — working as designed, always
    /// investigate.
    pub fn writer_guard_fenced(&self) -> u64 {
        self.guard_fenced.load(Ordering::Relaxed)
    }

    /// Heartbeat-cadence Reservation Report re-checks that found
    /// holdership lapsed with no foreign holder and re-acquired
    /// (`writer_guard_pr_reacquires`, design §9 — a PTPL-less target
    /// power-cycled; audit the fabric).
    pub fn writer_guard_pr_reacquires(&self) -> u64 {
        self.pr_reacquires.load(Ordering::Relaxed)
    }

    /// Mount-sequence event trace (guard-harness surface): the ordered
    /// guard events of this backend's `open` — pinned order
    /// `flock_acquired` → `claim_committed` → `claim_barriered` →
    /// `checkpoint_task_spawned` (the claim is the volume's first
    /// post-replay mutation *by construction*, design §5.0 B2).
    pub fn open_trace(&self) -> Vec<&'static str> {
        self.guard_trace.lock().unwrap().clone()
    }

    /// Read and decode the volume's `writer_claim` record (from the
    /// replayed RAM state — the staleness evidence the mount gate and
    /// the `claim clear` verb consume). `None` when absent or
    /// unparseable.
    pub async fn read_writer_claim(&self) -> Option<WriterClaim> {
        match self.getxattr(1, WRITER_CLAIM_XATTR).await {
            Ok(Some(val)) => WriterClaim::decode(&val),
            _ => None,
        }
    }

    /// Read every mount registration on this volume's root ino — the
    /// `client:{id}` heartbeat records plus the guard's `writer_claim` —
    /// classified under the ONE staleness law
    /// ([`crate::fuse_client::CLIENT_STALE_TTL_SECS`]). The format
    /// preflight refuses on [`MountRegistration::heartbeat_fresh`]; the
    /// `squeezefs clients` / `status` surfaces additionally report the
    /// guard's same-host dead-pid proof for claim records (a kill -9'd
    /// holder shows reclaimable instantly, matching the mount gate's own
    /// classification). Read-only; safe on probe backends against a
    /// live-mounted volume.
    pub async fn mount_registrations(&self) -> Vec<MountRegistration> {
        let now = unix_now_secs();
        let ttl = crate::fuse_client::CLIENT_STALE_TTL_SECS;
        let mut out = Vec::new();
        // PR VL5b: `client:` heartbeats are ROUTED records on global
        // ino 1 — after a slot-0 migration they live in this volume's
        // GUEST slot-0 keyspace (`guest_local_ino(0, 1)`), not at local
        // ino 1 (which keeps only the per-volume `writer_claim`). Scan
        // both roots.
        let mut roots: Vec<Ino> = vec![1];
        if let Some(stamp) = self.membership_stamp() {
            if stamp.slots_hosted.contains(0) && stamp.resolved_native_slot() != Some(0) {
                roots.push(crate::meta_backend::guest_local_ino(0, 1));
            }
        }
        let mut keys: Vec<(Ino, String)> = Vec::new();
        for root in roots {
            if let Ok(names) = self.listxattr(root).await {
                keys.extend(names.into_iter().map(|n| (root, n)));
            }
        }
        for (root, key) in keys {
            let (kind, is_writer) = if key.starts_with(CLIENT_REGISTRATION_PREFIX) {
                ("client", false)
            } else if key == WRITER_CLAIM_XATTR {
                ("writer", true)
            } else {
                continue;
            };
            if is_writer && root != 1 {
                // A `writer_claim` is PER-VOLUME state at local ino 1 —
                // one in a migrated guest keyspace is a foreign volume's
                // stale residue, never this volume's claim.
                continue;
            }
            let Ok(Some(val)) = self.getxattr(root, &key).await else {
                continue;
            };
            let heartbeat_ts = parse_registration_ts(&val);
            let age_secs = heartbeat_ts.map(|ts| now.saturating_sub(ts));
            let heartbeat_fresh = age_secs.map(|age| age <= ttl).unwrap_or(false);
            let (id, pid, boot, holder_provably_dead, job_endpoint) = if is_writer {
                match WriterClaim::decode(&val) {
                    Some(c) => {
                        let same_host = !self.boot_id.is_empty() && c.boot == self.boot_id;
                        let dead = same_host && pid_provably_dead(c.pid);
                        (c.id.clone(), Some(c.pid), Some(c.boot), dead, None)
                    }
                    None => (String::new(), None, None, false, None),
                }
            } else {
                let id = key
                    .strip_prefix(CLIENT_REGISTRATION_PREFIX)
                    .unwrap_or(&key)
                    .to_string();
                let v = serde_json::from_slice::<serde_json::Value>(&val).ok();
                let pid = v
                    .as_ref()
                    .and_then(|v| v.get("pid")?.as_u64())
                    .map(|p| p as u32);
                // Additive §5.1.6 discovery field (PR VL2b) — absent on
                // legacy and non-coordinator records.
                let job_endpoint = v
                    .as_ref()
                    .and_then(|v| v.get("job_endpoint")?.as_str())
                    .map(str::to_string);
                (id, pid, None, false, job_endpoint)
            };
            out.push(MountRegistration {
                key,
                kind,
                id,
                pid,
                boot,
                heartbeat_ts,
                age_secs,
                heartbeat_fresh,
                holder_provably_dead,
                job_endpoint,
            });
        }
        out
    }

    /// Usurpation-class fail-stop: latch `failed` loud and count it in
    /// `writer_guard_fenced`.
    fn guard_fail_stop(&self, why: &str) {
        self.guard_fenced.fetch_add(1, Ordering::AcqRel);
        if !self.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: single-writer guard fail-stop — {why}; volume marked \
                 FAILED (mutations return EIO until remount; writer_guard_fenced)",
                self.path.display()
            );
        }
    }

    /// Heartbeat-cadence guard refresh (design §5.0 B1 pt 6 + B2): on PR
    /// volumes re-verify host-identity stability and run the Reservation
    /// Report re-check (holdership lapsed + no foreign holder ⇒
    /// re-register + re-acquire, counted in `writer_guard_pr_reacquires`;
    /// foreign holder or identity mismatch ⇒ fail-stop), then re-commit
    /// the `writer_claim` with a fresh timestamp. Best-effort; never
    /// fails the caller.
    pub async fn guard_heartbeat(&self) {
        if self.guard_fd.lock().unwrap().is_none()
            || self.read_only
            || self.is_failed()
            || self.is_shutting_down()
        {
            return;
        }
        // B1 first: never refresh a claim past a foreign reservation.
        if let Some(rsv) = self.reservations.clone() {
            match rsv_call(&rsv, |c| c.host_identity()).await {
                Ok(current) => {
                    if let Some(recorded) = self.pr_identity.get() {
                        if *recorded != current {
                            self.guard_fail_stop(&format!(
                                "host identity changed under the reservation \
                                 (recorded hostnqn={} hostid={}, now hostnqn={} hostid={}) — \
                                 our registration is not ours (stable hostnqn/hostid is \
                                 required for guarded namespaces)",
                                recorded.hostnqn, recorded.hostid, current.hostnqn, current.hostid
                            ));
                            return;
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "meta volume {}: host identity unreadable at re-check ({e}); \
                         skipping this beat",
                        self.path.display()
                    );
                    return;
                }
            }
            match rsv_call(&rsv, |c| c.report()).await {
                Ok(rep) => match rep.holder_key {
                    Some(k) if k == self.pr_key => {}
                    None => {
                        // PTPL lapse: the target dropped the reservation
                        // (power cycle) and nothing took it — re-prove
                        // holdership. The register rides the same ladder
                        // as the mount gate: a lapse that somehow left a
                        // stale same-host registration behind (exotic
                        // target restart states) recovers identically,
                        // and every fail-closed rung degrades to exactly
                        // the plain-register error this arm already
                        // escalates on.
                        let key = self.pr_key;
                        let re = async {
                            let out = rsv_call(&rsv, move |c| {
                                crate::meta_backend::reservation::register_ladder(c, key)
                            })
                            .await?;
                            if let crate::meta_backend::reservation::RegisterOutcome::RecoveredOwnStale {
                                unregistered,
                            } = &out
                            {
                                log::warn!(
                                    "meta volume {}: lapse re-register recovered our own \
                                     stale registration(s) {unregistered:#018x?} \
                                     (single-writer guard register ladder)",
                                    self.path.display()
                                );
                            }
                            rsv_call(&rsv, move |c| c.acquire_write_exclusive(key)).await
                        }
                        .await;
                        match re {
                            Ok(()) => {
                                self.pr_reacquires.fetch_add(1, Ordering::AcqRel);
                                self.pr_active.store(true, Ordering::Release);
                                log::warn!(
                                    "meta volume {}: reservation holdership had lapsed \
                                     (PTPL-less target power cycle?) — re-acquired \
                                     (writer_guard_pr_reacquires); audit the fabric",
                                    self.path.display()
                                );
                            }
                            Err(e) => {
                                self.guard_fail_stop(&format!(
                                    "re-acquire after reservation lapse failed ({e}) — \
                                     another mount may have taken the namespace"
                                ));
                                return;
                            }
                        }
                    }
                    Some(foreign) => {
                        self.guard_fail_stop(&format!(
                            "reservation is held by a foreign registrant \
                             (key {foreign:#018x}) — this holder has been usurped"
                        ));
                        return;
                    }
                },
                Err(e) => {
                    log::warn!(
                        "meta volume {}: reservation report failed at re-check ({e}); \
                         retrying next beat",
                        self.path.display()
                    );
                }
            }
        }
        // B2: refresh the claim heartbeat (one staleness law with the
        // client registrations).
        if self.claimed.load(Ordering::Acquire) {
            let claim = WriterClaim {
                id: self.writer_id.clone(),
                ts: unix_now_secs(),
                pid: std::process::id(),
                boot: self.boot_id.clone(),
                // The era is set once by the gate; the heartbeat carries
                // it forward verbatim (a refresh is never a new claim).
                term: self.writer_term(),
            };
            if let Err(e) = self
                .setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
                .await
            {
                log::warn!(
                    "meta volume {}: writer_claim heartbeat refresh failed: {e}",
                    self.path.display()
                );
            }
        }
    }

    /// The `squeezefs claim clear` admin verb body (design §5.0):
    /// operator-attested removal of a **stale** `writer_claim` on `path`.
    /// Refuses fresh claims, refuses when the volume is flock-held on this
    /// host or live-mounted anywhere (`format_preflight`-style live-check),
    /// and makes the removal durable before returning.
    pub async fn claim_clear(path: &Path) -> std::result::Result<ClaimClearOutcome, KvError> {
        // Layer A live-check: a held flock IS a live same-host mount.
        let guard_fd = match Self::acquire_writer_flock(path) {
            Ok(fd) => fd,
            Err(FlockOutcome::Held) => {
                return Err(KvError::Busy(format!(
                    "{}: refusing to clear the writer claim — the volume is live-mounted \
                     on this host (the writer lock is held). Unmount it first",
                    path.display()
                )));
            }
            Err(FlockOutcome::Io(e)) => {
                return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
            }
        };
        let mut inner = Self::open_inner(path).await?;
        *inner.guard_fd.get_mut().unwrap() = Some(guard_fd);
        let be = Arc::new(inner);
        // PR M7: conveyor identity before the clear's removexattr commit
        // (every `Arc::new(Self)` site wires it — the commit path fails
        // loud otherwise).
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        let now = unix_now_secs();

        // Preflight-style live sweep: fresh client registrations mean a
        // live mount somewhere — never clear under one.
        let mut live: Vec<String> = Vec::new();
        if let Ok(attrs) = be.listxattr(1).await {
            for k in attrs.iter().filter(|k| k.starts_with("client:")) {
                if let Ok(Some(val)) = be.getxattr(1, k).await {
                    let fresh = serde_json::from_slice::<serde_json::Value>(&val)
                        .ok()
                        .and_then(|v| v.get("ts")?.as_u64())
                        .map(|ts| {
                            now.saturating_sub(ts) <= crate::fuse_client::CLIENT_STALE_TTL_SECS
                        })
                        .unwrap_or(false);
                    if fresh {
                        live.push(k.clone());
                    }
                }
            }
        }
        if !live.is_empty() {
            return Err(KvError::Busy(format!(
                "{}: refusing to clear the writer claim — the volume has live client \
                 registrations: {live:?}",
                path.display()
            )));
        }

        let raw = be
            .getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .map_err(KvError::Io)?;
        let Some(raw) = raw else {
            return Ok(ClaimClearOutcome::NoClaim);
        };
        let holder = WriterClaim::decode(&raw);
        if let Some(c) = &holder {
            if c.age_secs(now) <= crate::fuse_client::CLIENT_STALE_TTL_SECS {
                return Err(KvError::Busy(format!(
                    "{}: refusing to clear a FRESH writer claim{} — the holder heartbeated \
                     within the {}s ttl and may be alive. Stop that writer (or wait for \
                     the claim to expire), then retry",
                    path.display(),
                    holder_suffix(&Some((c.clone(), now))),
                    crate::fuse_client::CLIENT_STALE_TTL_SECS,
                )));
            }
        }
        // Stale (or unattributable — clearable by the same attestation):
        // remove durably: commit + barrier + checkpoint (this backend has
        // no checkpoint task; drive the cycle explicitly).
        be.removexattr_internal(1, WRITER_CLAIM_XATTR).await?;
        be.sync_device().await.map_err(KvError::Io)?;
        be.checkpoint_now().await?;
        log::info!(
            "meta volume {}: writer claim cleared by operator attestation{}",
            path.display(),
            holder_suffix(&holder.clone().map(|c| (c, now))),
        );
        Ok(ClaimClearOutcome::Cleared(holder.unwrap_or(WriterClaim {
            id: "<unparseable claim value>".to_string(),
            ts: 0,
            pid: 0,
            boot: String::new(),
            term: 0,
        })))
    }
}

/// Run one synchronous reservation command off the async runtime
/// (control-plane micro-ioctl on the real client, pure memory on the
/// fake).
async fn rsv_call<T, F>(
    rsv: &Arc<dyn crate::meta_backend::reservation::ReservationClient>,
    f: F,
) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn crate::meta_backend::reservation::ReservationClient) -> std::io::Result<T>
        + Send
        + 'static,
{
    let rsv = rsv.clone();
    squeezefs_ipc::sqz_blocking::run_blocking(move || f(rsv.as_ref())).await
}

// ---------------------------------------------------------------------------
// KvTx: task-scoped record staging with a read-your-own-writes overlay
// (§4.4: "task-local record staging (Vec<(tree_id, Record)>) with
// read-your-own-writes overlay, the analog of read_blocks' patch overlay").
// Ops build one explicitly-threaded KvTx per transaction — the same overlay
// semantics as v2's task-local ACTIVE_TX without the task-local plumbing
// (kv ops never nest transactions).
// ---------------------------------------------------------------------------

/// One staged (not yet committed) transaction: `(tree_id, key, kind,
/// value)` in stage order. Seqs are assigned at commit, inside the node
/// locks, from the journal reservation (§4.4 pt 2).
pub(super) struct KvTx {
    staged: Vec<(u8, Vec<u8>, RecordKind, Bytes)>,
    /// The `#[track_caller]` construction site — the metadata-throughput
    /// D4.a attribution hook (design §5.4): a successful `commit_tx`
    /// counts one journal entry against this location in
    /// [`super::META_KV_COMMIT_SITES`], so per-op entry ratios decompose
    /// into *named* committers (`meta_kv_commit_sites` on the stats
    /// inode; the OQ-1 pin in `tests/meta_entry_economy_tests.rs`).
    site: &'static std::panic::Location<'static>,
    /// PR M7 (Issue 13, §5.5 revision 2): the tx's DLM I/D guard set,
    /// **co-owned by the conveyor queue entry** from enqueue to the tx's
    /// terminal outcome (post-ack on success, post-rollback on failure).
    /// A dropped committer future drops only its own frame's refs — the
    /// entry's ref keeps same-key exclusion alive structurally. Multi-
    /// commit ops clone one set per sequential `commit_tx`. Empty only
    /// for commits whose exclusion is architectural rather than
    /// DLM-borne (the pre-arm writer-claim tx — single-writer window by
    /// the flock; compensation records — checkpoint-class).
    guards: Arc<[DlmGuard]>,
}

impl KvTx {
    #[track_caller]
    fn new() -> Self {
        Self {
            staged: Vec::new(),
            site: std::panic::Location::caller(),
            guards: Arc::from(Vec::new()),
        }
    }

    /// Attach the transaction's DLM guard set (see the `guards` field —
    /// the routed ops hand their held set here; multi-commit ops clone
    /// the same `Arc` per commit).
    fn hold_guards(&mut self, guards: Arc<[DlmGuard]>) {
        self.guards = guards;
    }

    fn stage_put(&mut self, tree_id: u8, key: impl Into<Vec<u8>>, value: impl Into<Bytes>) {
        self.staged
            .push((tree_id, key.into(), RecordKind::Put, value.into()));
    }

    fn stage_delta(&mut self, tree_id: u8, key: impl Into<Vec<u8>>, delta: &InodeDelta) {
        self.staged.push((
            tree_id,
            key.into(),
            RecordKind::Delta,
            Bytes::from(delta.encode()),
        ));
    }

    /// Stage a pre-encoded `Delta` payload (the layout-delta class —
    /// `crate::layout_wire` wire bytes; write-commit-economy 2026-07-30).
    fn stage_delta_raw(&mut self, tree_id: u8, key: impl Into<Vec<u8>>, payload: impl Into<Bytes>) {
        self.staged
            .push((tree_id, key.into(), RecordKind::Delta, payload.into()));
    }

    fn stage_delete(&mut self, tree_id: u8, key: impl Into<Vec<u8>>) {
        self.staged
            .push((tree_id, key.into(), RecordKind::Delete, Bytes::new()));
    }

    /// Spec §6.2 item 1: stage the transaction's durable block-reference
    /// operations — one `Put` per reference taken, one `Delete` per
    /// reference dropped, into the **same** tx as the layout record and
    /// the inode record. One tx = one checksummed journal entry (§4.10),
    /// so the accounting can never disagree with the layout that
    /// justifies it, not even across a torn write; and the publish stays
    /// ONE commit (the write-commit-economy collapse is not re-split).
    fn stage_block_refs(&mut self, ops: &[super::block_refs::BlockRefOp]) {
        for op in ops {
            let key = op.reference.key().to_vec();
            if op.take {
                self.stage_put(
                    super::record::TREE_BLOCK_REFS,
                    key,
                    op.reference.value().to_vec(),
                );
            } else {
                self.stage_delete(super::record::TREE_BLOCK_REFS, key);
            }
        }
        let took = ops.iter().filter(|o| o.take).count() as u64;
        let dropped = ops.len() as u64 - took;
        if took > 0 {
            super::META_KV_BLOCK_REFS_STAGED.fetch_add(took, std::sync::atomic::Ordering::Relaxed);
        }
        if dropped > 0 {
            super::META_KV_BLOCK_REFS_RELEASED
                .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }
}

/// A key's pre-image captured at RAM apply, for the §4.4 pt 4 rollback.
struct UndoKey {
    tree_id: u8,
    key: Vec<u8>,
    pre: LiveLookup,
}

/// One transaction on the commit conveyor (PR M7, §5.5 D5): the encoded
/// records, the exact entry length, the D4.a attribution site, the
/// co-owned DLM guard set, and the committer's result channel.
/// One queued delta-class layout save on the volume's layout-merge
/// conveyor (rewrite-publish-drain Lever B, 2026-08-01): the
/// `merge_layout_and_size` parameters plus the submitter's result
/// channel (`use_delta` on success — the caller's chain accounting).
struct QueuedLayoutMerge {
    ino: Ino,
    /// Rung 19: the GLOBAL ino the durable block-reference records key on
    /// (`block_refs` law: `owner_ino` is the GLOBAL ino — the routed
    /// layer's pre-`route_ino` identity, NOT the volume-local/guest-
    /// namespaced `ino` above, which on a hosted slot reads
    /// `((slot+1) << 40) | local` and keyed the armD conviction's phantom
    /// records).
    refs_owner: Ino,
    /// Pre-encoded `LayoutDelta` wire bytes (encoded once, at enqueue).
    delta_wire: Bytes,
    /// The caller-provided full layout — the always-correct fallback
    /// where the backend eligibility half refuses the delta.
    full_layout: Bytes,
    size: u64,
    /// Spec §6.2 item 1: this member's durable block-reference ops, staged
    /// into the batch's ONE aggregated transaction alongside its layout
    /// record — the accounting aggregates exactly as the saves do.
    block_refs: Vec<super::block_refs::BlockRefOp>,
    /// DLM S11 rung 17 (KD-MW-8's composition law): **chain onto the
    /// durable head** instead of gate-refusing a divergent claim or
    /// re-basing with the caller's private full layout. `true` only for
    /// the S9 SHIPPED publish serve and the authority's own publishes on
    /// an ino with live foreign custody — both structurally unreachable
    /// on a solo mount (`false` keeps the shipped semantics verbatim).
    chain: bool,
    /// The submitter's result channel: `(use_delta, staged_version)` —
    /// the staged link's version (0 on a full-Put/unversioned commit),
    /// which is what lets a co-writer chain without a refetch (the
    /// design-mw-layout-versions §6 residual this rung pays).
    done: squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<(bool, u64)>>,
}

/// [`KvMetaBackend::spill_oversize_chained_full`]'s spill result — the
/// inline→oversize crossing arm's product (2026-08-19 wedge trigger fix).
struct SpilledChainedFull {
    /// The re-encoded head naming `indirect:{fresh_key}` — what the
    /// commit stages instead of the oversize inline value.
    encoded: Vec<u8>,
    /// The composed head (`block_map: None`, `block_map_id` = the fresh
    /// name) — the aggregated pass's memo seed.
    head: crate::layout_wire::LayoutMetadata,
    /// The FULL composed map — the memo's accumulated view.
    map: std::collections::HashMap<u32, String>,
    /// The fresh CoW blob's key (the MAP_BLOB take + memo `prior_fresh`).
    fresh_key: String,
    /// RES-9 custody for the fresh blob until the naming commit lands.
    guard: super::indirect_map::IndirectBlobGuard,
}

struct QueuedTx {
    /// The tx's records, seqs stamped by the pass inside the lock window.
    recs: Vec<(u8, Record)>,
    /// Exact journal entry length ([`entry_len_for`]) — the Σ-admission
    /// and the drain byte cap read it.
    len: u64,
    /// Enqueue instant (`meta_txpass_phase_ns` tx_queue_wait — the
    /// rewrite-publish-drain decomposition, 2026-08-01).
    enqueued_at: std::time::Instant,
    /// D4.a: the `KvTx` construction site (counted on success).
    site: &'static std::panic::Location<'static>,
    /// Issue 13 (§5.5 revision 2): the tx's DLM I/D guards, held by THIS
    /// entry until the tx's terminal outcome — dropped with the entry at
    /// fan-out (post-result-send on success, post-rollback on failure).
    /// Never read, only owned: the RAII hold IS the same-key exclusion.
    _guards: Arc<[DlmGuard]>,
    /// Fan-out channel. A dead receiver (dropped committer future) is
    /// harmless — semantically identical to timeout-fires-after-commit.
    done: squeezefs_ipc::sqz_channel::oneshot::Sender<std::result::Result<(), KvError>>,
}

/// The §5.5 panic guard: pipeline state that must never be dropped on
/// the floor, armed for the whole batch pass. Every NORMAL path (success
/// and failure alike) empties it; [`Drop`] therefore fires with content
/// only when the pass unwinds (panic) or the detached task is torn down
/// mid-await (runtime shutdown) — and then performs the all-sync §5.5
/// cleanup: release the un-transferred `Admission`, `complete()` any
/// registered reservation as **abandoned** (the §4.4 pt 4
/// unwritten-range mechanism — replay's checksum walk drops it), fail
/// the batch's oneshots with EIO, fail out anything still queued behind
/// the dead leader, release leadership, and escalate loud. A batch can
/// fail; `completed_upto` can never wedge.
struct PassSentinel<'a> {
    be: &'a Arc<KvMetaBackend>,
    /// Batch members not yet at their terminal outcome.
    entries: Vec<QueuedTx>,
    /// Members WITH their terminal outcome computed, awaiting fan-out
    /// (the pass task sends these after releasing the backend ref).
    outcomes: Vec<(QueuedTx, std::result::Result<(), KvError>)>,
    /// Σ admission, held from admit until transfer to the reservation.
    admission: Option<super::journal_core::Admission>,
    /// The registered batch reservation, held until the pass completes it.
    reservation: Option<Reservation>,
    /// Records are applied to RAM and neither journaled nor rolled back
    /// (the span where RAM would silently diverge from replay): a panic
    /// here additionally fail-stops the volume — reads are
    /// RAM-authoritative and writeback would make the divergence durable;
    /// with `failed` latched the checkpoint task idles and the divergence
    /// dies at remount.
    applied_unrolled: bool,
}

impl Drop for PassSentinel<'_> {
    fn drop(&mut self) {
        if self.entries.is_empty()
            && self.outcomes.is_empty()
            && self.admission.is_none()
            && self.reservation.is_none()
        {
            return;
        }
        // Computed-but-unsent outcomes are REAL terminal results (their
        // effects are committed/rolled back) — deliver them even on the
        // unwind path.
        for (q, outcome) in self.outcomes.drain(..) {
            let _ = q.done.send(outcome);
        }
        super::META_CONVEYOR_PASS_PANICS.fetch_add(1, Ordering::Relaxed);
        if let Some(res) = self.reservation.take() {
            // Abandoned, never wedged: the range stays unwritten; replay
            // drops it at the checksum walk (§4.4 pt 4).
            self.be.ring.complete(&res);
        }
        if let Some(adm) = self.admission.take() {
            self.be.ring.core().release(adm);
        }
        let batch_n = self.entries.len();
        for q in self.entries.drain(..) {
            let _ = q.done.send(Err(KvError::Io(self.be.eio(
                "commit conveyor pass panicked — batch failed loud (§5.5 panic guard)",
            ))));
        }
        if self.applied_unrolled && !self.be.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: conveyor pass panicked AFTER RAM apply — volume marked \
                 FAILED (RAM diverges from replay for the failed batch; mutations return \
                 EIO until remount, checkpoint ticks idle, the divergence dies with the \
                 process)",
                self.be.path.display()
            );
        }
        // The pass dies holding leadership: fail out everything queued
        // behind it and release, so later committers are never stranded
        // behind a dead leader (post-release arrivals elect fresh passes).
        let mut stranded_n = 0usize;
        loop {
            for q in self.be.conveyor.drain(usize::MAX, u64::MAX) {
                stranded_n += 1;
                let _ = q.done.send(Err(KvError::Io(self.be.eio(
                    "commit conveyor leader panicked before this tx was drained — retry \
                     after the volume recovers",
                ))));
            }
            if !self.be.conveyor.unlead_and_recheck() {
                break;
            }
        }
        self.be.note_journal_failure();
        log::error!(
            "meta volume {}: conveyor pass panic contained — {batch_n} batch member(s) \
             failed EIO, {stranded_n} queued tx(s) failed out, budget released, \
             reservation abandoned (meta_conveyor_pass_panics)",
            self.be.path.display()
        );
    }
}

/// How a routed dentry mutation updates its parent directory (the v2
/// routed arms' three shapes: 16-byte patch under a SHARED parent →
/// Δtime merge record here; full RMW; full RMW + nlink bump/dec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedParentUpdate {
    /// No parent touch (rename dentry surgery).
    None,
    /// SHARED parent: mtime/ctime via the §4.4 pt 6 Δtime merge record.
    SharedTimes,
    /// EXCLUSIVE parent: full times update.
    ExclusiveTimes,
    /// EXCLUSIVE parent: times + nlink (+1 on insert, −1 on remove —
    /// directory children).
    ExclusiveTimesBump,
}

impl KvMetaBackend {
    fn tree_by_id(&self, id: u8) -> &KvTree {
        match id {
            TREE_INODES => &self.inodes,
            TREE_DENTRIES => &self.dentries,
            TREE_XATTRS => &self.xattrs,
            // Spec §6.2 item 1: staged only by the layout-commit paths,
            // and only when the tree is engaged (`block_refs_engaged`
            // gates every staging site) — an absent tree here means a
            // caller staged accounting onto a volume that has none.
            super::record::TREE_BLOCK_REFS => self
                .block_refs
                .as_ref()
                .expect("block-reference records staged on a volume without incompat bit 8"),
            _ => unreachable!("kv commits stage only the §4.2 trees"),
        }
    }

    fn eio(&self, what: &str) -> crate::error::SqueezefsError {
        crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("meta volume {}: {what}", self.path.display()),
        ))
    }

    /// The write gate every mutation passes (§4.11 unknown-ro, §4.4 pt 4
    /// fail-stop, shutdown refusal).
    fn write_gate(&self) -> Result<()> {
        if self.read_only {
            return Err(match self.ro_cause {
                // DLM S5: the operator asked for a reader. The refusal
                // names the surface that produced it, so an application
                // error is traceable to the mount option and not to a
                // format accident.
                ReadOnlyCause::ReaderMount => crate::fuse_client::read_only_refusal(&format!(
                    "metadata mutation on meta volume {}",
                    self.path.display()
                )),
                // DLM S9: a co-writer's metadata mutations are not
                // FORBIDDEN, they are ROUTED — so the refusal names the
                // path that carries them instead of a mount option. It is
                // also counted: a nonzero `cowriter_local_commit_refusals`
                // means a daemon surface still commits directly rather than
                // going through `meta_ship`, which is S8's stated
                // "the daemon is not switched onto the router" gap meeting
                // a real workload.
                ReadOnlyCause::CoWriterMount => {
                    crate::fuse_client::METRICS
                        .cowriter_local_commit_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    // The S8-b falsifier is a MUST-STAY-0 counter, so the
                    // one thing that matters when it moves is WHICH surface
                    // reached this gate — callers absorb the error (their
                    // fallbacks are correct), which is how rung 10's
                    // fan-out row found a nonzero count with no line naming
                    // the culprit. A capture here is off every healthy path
                    // by construction (the gate refused).
                    log::error!(
                        "cowriter_local_commit_refusals: an un-routed daemon surface reached \
                         the co-writer write gate on {} — the S8 'daemon not switched onto \
                         the router' gap meeting a real workload. Backtrace:\n{}",
                        self.path.display(),
                        std::backtrace::Backtrace::force_capture()
                    );
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "metadata mutation on meta volume {} refused: this mount is a CO-WRITER \
                         (DLM S9) and holds NO metadata authority over it — the volume's \
                         journal ring, extent bitmap and root ledger have exactly one appender, \
                         and it is the authority that holds the D0 claim. Mutations must SHIP \
                         (`meta_ship::publish` / the S8 metadata verbs) rather than commit \
                         here; a surface that reached this gate is one the daemon has not yet \
                         routed (cowriter_local_commit_refusals)",
                        self.path.display()
                    ))
                }
                // Per-volume claim admission (§5.1.2): the mutation is not
                // FORBIDDEN, it is ROUTED — to a PEER of this same set,
                // not to "the authority", because this mount IS an
                // authority for the volumes it owns. Counted apart from
                // the co-writer class so neither counter's meaning rots.
                ReadOnlyCause::PeerOwnedVolume => {
                    crate::fuse_client::METRICS
                        .peer_volume_local_commit_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    // Must-stay-0, so what matters when it moves is WHICH
                    // surface reached the gate (the S8-b capture's
                    // reasoning, verbatim: callers absorb the error).
                    log::error!(
                        "peer_volume_local_commit_refusals: an un-routed daemon surface \
                         reached the peer-owned write gate on {} — a metadata mutation on a \
                         volume a PEER appends to must SHIP. Backtrace:\n{}",
                        self.path.display(),
                        std::backtrace::Backtrace::force_capture()
                    );
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "metadata mutation on meta volume {} refused: this volume is appended \
                         to by a PEER authority of this set — its journal ring, extent bitmap \
                         and root ledger have exactly one appender, and it is not this mount. \
                         Mutations must SHIP (`meta_ship`'s verbs / `meta_ship::publish`) \
                         rather than commit here; this mount commits locally on the volumes it \
                         OWNS (peer_volume_local_commit_refusals)",
                        self.path.display()
                    ))
                }
                _ => crate::error::SqueezefsError::InvalidOperation(format!(
                    "meta volume {} carries unknown read-only feature bits {:#x}: mounted \
                     read-only (§4.11); mutations withheld",
                    self.path.display(),
                    self.sb.unknown_ro()
                )),
            });
        }
        if self.is_failed() {
            return Err(self.eio(
                "volume failed after repeated journal write errors (EIO \
                                 until remount)",
            ));
        }
        if self.is_shutting_down() {
            return Err(self.eio("volume is shutting down"));
        }
        Ok(())
    }

    fn note_journal_failure(&self) {
        let n = self.journal_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= JOURNAL_FAILURE_LATCH && !self.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: {n} consecutive journal write failures — volume marked \
                 FAILED (mutations return EIO until remount; §4.4 pt 4 escalation)",
                self.path.display()
            );
        }
    }

    /// **The §4.4 commit pipeline, conveyor edition (PR M7 —
    /// design-metadata-throughput §5.5 D5).** Every mutating op stages a
    /// [`KvTx`] and lands here. The committer:
    ///
    /// 1. encodes its records and computes the exact entry size (the
    ///    §4.1 writer-side 128 KiB guard) — a tx that fails HERE fails
    ///    alone, before joining any batch;
    /// 2. enqueues `{records, Arc<[DlmGuard]>, oneshot}` on the
    ///    per-volume conveyor and leader-elects — **no await between the
    ///    two**, so a cancelled committer future can never strand a
    ///    queued entry leaderless. The election winner spawns the
    ///    **detached, panic-guarded pass task** ([`Self::conveyor_pass`])
    ///    and then parks on its own oneshot like every follower;
    /// 3. awaits its result. Dropping this future at the await drops
    ///    only the oneshot receiver and this frame's `Arc` refs — the
    ///    queue entry co-owns the guard set, so same-key exclusion
    ///    survives until the pass reaches the tx's terminal outcome
    ///    (§5.5 revision 2, Issue 13).
    ///
    /// The batch pipeline itself — one Σ admission, union leaf locks,
    /// contiguous per-tx reservations, one `write_at_batch`, one barrier
    /// — is [`Self::run_batch`]; a batch of 1 runs today's per-tx
    /// pipeline stages byte-for-byte (the degenerate case IS the
    /// pre-conveyor code path).
    async fn commit_tx(&self, tx: KvTx) -> std::result::Result<(), KvError> {
        if tx.is_empty() {
            return Ok(());
        }
        // D4.a attribution: the construction site this (about-to-be-
        // committed) tx counts against on success.
        let site = tx.site;
        // (1) Exact size before anything is queued (§4.4 pt 5) — an
        // oversized / undecodable tx fails ALONE, never inside a batch.
        let recs: Vec<(u8, Record)> = tx
            .staged
            .into_iter()
            .map(|(tree_id, key, kind, value)| {
                (
                    tree_id,
                    Record {
                        key,
                        seq: 0, // stamped from the reservation, in-lock
                        kind,
                        // D1.c stage_put audit: `Bytes::to_vec` COPIED every
                        // staged value at commit; `Vec::from(Bytes)` reclaims
                        // the unique Vec-backed allocation instead (stage
                        // sites build values as `Bytes::from(vec)`).
                        value: Vec::from(value),
                    },
                )
            })
            .collect();
        // Finding-A hardening: staged records must decode under their own
        // tree's typed decoder before any byte is persisted (debug tiers).
        #[cfg(debug_assertions)]
        for (tree_id, r) in &recs {
            super::node::debug_audit_records(*tree_id, 0, std::slice::from_ref(r));
        }
        // Admission honesty (the 2026-08-19 AlreadyFreezing wedge's third
        // leg): the per-volume record-value cap (§4.2 — the very check
        // `encode_bset_frame` runs at FREEZE time) is enforced HERE, the
        // one commit-side choke point every `KvTx` passes, so an
        // over-cap record fails ITS OWN commit loudly instead of being
        // acked into a node overlay the checkpoint can then never
        // serialize (the field capture: admission passed, the freeze
        // refused, the volume's checkpoint wedged forever). Direct
        // node-layer writers (`KvTree::insert`) and the user xattr path
        // already enforce it upstream — this is the backstop for every
        // OTHER staging site, present and future.
        let cap = self.cache.config().layout.record_value_cap();
        if let Some((_, r)) = recs.iter().find(|(_, r)| r.value.len() > cap) {
            return Err(KvError::ValueTooLarge {
                len: r.value.len(),
                cap,
            });
        }
        let len = entry_len_for(&recs)?;

        // (2) Enqueue + leader-elect. The pass task holds the QUEUE
        // strongly but the backend only weakly (upgraded per batch): an
        // idle pass must never keep a dropped-without-shutdown backend —
        // and its writer flock — alive past the last user `Arc`. Resolve
        // the identity BEFORE enqueueing so a broken wiring fails loud
        // with nothing queued (unreachable by construction — set at
        // every open path before the first commit).
        let weak = self.conveyor_self.get().cloned().ok_or_else(|| {
            KvError::Corrupt(
                "conveyor identity missing (commit before open wiring?) — refusing \
                     to enqueue a tx no pass task could ever drain"
                    .to_string(),
            )
        })?;
        let (done, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        self.conveyor.enqueue(
            QueuedTx {
                recs,
                len,
                enqueued_at: std::time::Instant::now(),
                site,
                _guards: tx.guards,
                done,
            },
            len,
        );
        if self.conveyor.try_lead() {
            // Detached (§5.5 lifecycle): no client-visible cancellation
            // can drop the pass mid-flight; the journal's accounting is
            // unforgiving (a dropped Admission leaks budget forever, an
            // uncompleted reservation wedges completed_upto).
            let conveyor = Arc::clone(&self.conveyor);
            // Stage 1c: the pass task is PLANE-CRITICAL (it holds the 4b
            // union leaf locks and every committer parks on its fan-out)
            // — sqz-meta lanes, never the main tokio runtime.
            crate::meta_exec::spawn_meta(
                "kv_conveyor_pass",
                Self::conveyor_pass_task(conveyor, weak),
            );
        }

        // (3) Park on the fan-out. A closed channel means the pass died
        // between drain and fan-out — the panic sentinel already failed
        // the batch loud (EIO here is the belt, not the mechanism).
        // The wedge census gauges the park (2026-08-07): writers parked
        // here while passes stay flat IS the stalled-conveyor signature.
        let _parked = super::ParkedGaugeGuard::enter(&super::META_COMMIT_PARKED);
        // Stage-1b named-wait census: ages this park in the watchdog's
        // lock-wait lines (the gauge above counts it; this NAMES it).
        let _census = crate::fuse_client::LockWaitToken::begin(
            crate::fuse_client::LockClass::Commit,
            0,
            0,
            len as u64,
        );
        match rx.await {
            Ok(out) => out,
            Err(_) => Err(KvError::Io(self.eio(
                "commit conveyor pass dropped its result channel (pass panic — batch \
                 failed loud)",
            ))),
        }
    }

    /// The detached conveyor pass task (§5.5): drain whatever is queued
    /// — bounded by the tx/byte caps, NO timers — and run the batch
    /// pipeline once per drain; on an empty drain release leadership and
    /// re-check (the `conveyor_core` no-lost-wakeup protocol). Exactly
    /// one pass task runs per volume at any time (leader uniqueness,
    /// loom-modeled).
    ///
    /// Holds the backend **per batch only** (Weak between batches) and
    /// fans results out AFTER releasing it: a committer woken by its
    /// result can drop the last user `Arc` and immediately re-open the
    /// volume — the writer flock is never parked behind this task.
    async fn conveyor_pass_task(conveyor: Arc<ConveyorCore<QueuedTx>>, weak: Weak<KvMetaBackend>) {
        loop {
            // Test seam: hold the pass pre-drain so arrivals accumulate
            // deterministically (register-recheck; zero-cost when unarmed).
            while TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed) == TEST_CONVEYOR_HOLD_PRE_DRAIN {
                let notified = TEST_CONVEYOR_HOLD_NOTIFY.notified();
                if TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed) != TEST_CONVEYOR_HOLD_PRE_DRAIN
                {
                    break;
                }
                notified.await;
            }
            let Some(be) = weak.upgrade() else {
                // Backend dropped without shutdown. Live committers hold
                // `&self`, so none exist; queued entries can only be
                // dropped-committer residue — fail them out (dead
                // receivers; the sends are the belt) and release
                // leadership. Guards are self-contained owned locks and
                // release with the entries.
                loop {
                    for q in conveyor.drain(usize::MAX, u64::MAX) {
                        let _ = q.done.send(Err(KvError::Corrupt(
                            "meta volume dropped with transactions still queued".to_string(),
                        )));
                    }
                    if !conveyor.unlead_and_recheck() {
                        return;
                    }
                }
            };
            let batch = conveyor.drain(be.batch_max_txs, be.batch_max_bytes);
            if batch.is_empty() {
                // Test seam: park the empty-drain tail WHILE the
                // per-iteration backend upgrade is held — the deliberate
                // exception to the drop-before-park rule below (see
                // [`TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL`]: it models the
                // OS descheduling this worker thread between the upgrade
                // and the drop, the 2026-07-27 torn-claim remount-flake
                // window).
                let mut park_noted = false;
                while TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed)
                    == TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL
                {
                    if !park_noted {
                        TEST_CONVEYOR_EMPTY_TAIL_PARKED.fetch_add(1, Ordering::SeqCst);
                        park_noted = true;
                    }
                    let notified = TEST_CONVEYOR_HOLD_NOTIFY.notified();
                    if TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed)
                        != TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL
                    {
                        break;
                    }
                    notified.await;
                }
                drop(be); // never park on leadership holding the backend
                if !conveyor.unlead_and_recheck() {
                    return;
                }
                continue;
            }
            let outcomes = be.run_batch(batch).await;
            // Release the backend BEFORE waking committers: a woken
            // committer may own the last user `Arc` and re-open.
            drop(be);

            // Pre-fanout test seam (the cancel-pre-fanout stage — held
            // with the backend already released).
            while TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed) == TEST_CONVEYOR_HOLD_PRE_FANOUT
            {
                let notified = TEST_CONVEYOR_HOLD_NOTIFY.notified();
                if TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed) != TEST_CONVEYOR_HOLD_PRE_FANOUT
                {
                    break;
                }
                notified.await;
            }

            // (9) Fan out per-tx results; each entry's guard set is
            // released at ITS terminal outcome — post-result-send on
            // success, post-rollback on failure (which already ran
            // inside the pipeline).
            for (q, outcome) in outcomes {
                let _ = q.done.send(outcome);
            }
        }
    }

    /// One conveyor batch through the §4.4 pipeline (§5.5: "runs the
    /// pipeline ONCE for the batch"), under the panic sentinel. Never
    /// errs — it returns every member's terminal outcome for the caller
    /// to fan out (after releasing the backend ref), each exactly once.
    async fn run_batch(
        self: &Arc<Self>,
        batch: Vec<QueuedTx>,
    ) -> Vec<(QueuedTx, std::result::Result<(), KvError>)> {
        use crate::fuse_client::{meta_txpass_phase_record, MetaTxPassPhase};
        super::META_CONVEYOR_LEADER_PASSES.fetch_add(1, Ordering::Relaxed);
        super::META_COMMIT_GROUP_SIZE.record(batch.len());
        super::META_COMMIT_GROUP_BYTES
            .fetch_add(batch.iter().map(|q| q.len).sum::<u64>(), Ordering::Relaxed);
        // Pass decomposition (2026-08-01): queue residence per drained tx
        // + the whole-pass span.
        let t_pass = std::time::Instant::now();
        for q in &batch {
            meta_txpass_phase_record(MetaTxPassPhase::TxQueueWait, q.enqueued_at);
        }

        // Issue-13 structural invariant, debug-asserted (it must be
        // UNFIREABLE now: a conflicting same-key writer cannot co-queue
        // because the earlier tx's D/I guards are alive inside the queue
        // until its terminal outcome). The sanctioned exception is
        // shared-parent Δtime merge records, whose LWW-by-seq semantics
        // are order-independent (§4.4 pt 6).
        #[cfg(debug_assertions)]
        {
            let mut seen: std::collections::HashMap<(u8, &[u8]), (usize, RecordKind)> =
                std::collections::HashMap::new();
            for (qi, q) in batch.iter().enumerate() {
                for (tree_id, r) in &q.recs {
                    match seen.entry((*tree_id, r.key.as_slice())) {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert((qi, r.kind));
                        }
                        std::collections::hash_map::Entry::Occupied(o) => {
                            let (prev_qi, prev_kind) = *o.get();
                            debug_assert!(
                                prev_qi == qi
                                    || (prev_kind == RecordKind::Delta
                                        && r.kind == RecordKind::Delta),
                                "same-key co-queue exclusion violated: tree {tree_id} key \
                                 {:02x?} staged by batch members {prev_qi} and {qi} with \
                                 non-merge kinds {prev_kind:?}/{:?} — a DLM guard was \
                                 released before its tx's terminal outcome",
                                r.key,
                                r.kind,
                            );
                        }
                    }
                }
            }
        }

        // Panic sentinel (§5.5 lifecycle): if the pipeline unwinds — or
        // the runtime tears the detached task down mid-await — the Drop
        // impl releases the un-transferred Admission, completes any
        // registered reservation as abandoned, fails the remaining
        // oneshots with EIO and escalates. All-sync cleanup; a batch can
        // fail loud but can never wedge `completed_upto`.
        let mut sentinel = PassSentinel {
            be: self,
            entries: batch,
            outcomes: Vec::new(),
            admission: None,
            reservation: None,
            applied_unrolled: false,
        };
        self.run_batch_pipeline(&mut sentinel).await;
        debug_assert!(
            sentinel.entries.is_empty()
                && sentinel.admission.is_none()
                && sentinel.reservation.is_none(),
            "the batch pipeline must reach a terminal outcome for every entry on every \
             non-panic path (the sentinel is for unwinds only)"
        );
        meta_txpass_phase_record(MetaTxPassPhase::PassTotal, t_pass);
        std::mem::take(&mut sentinel.outcomes)
    }

    /// The batch pipeline body. Mutates the sentinel as protocol stages
    /// pass so an unwind at ANY await leaves exactly the right cleanup
    /// state; on every normal path (success and failure alike) it fans
    /// out per-tx results and empties the sentinel itself.
    async fn run_batch_pipeline(&self, s: &mut PassSentinel<'_>) {
        use crate::fuse_client::{meta_txpass_phase_record, MetaTxPassPhase};
        // Inherited liveness re-checks (§5.5: "the shutdown/failure-flag
        // re-checks it inherits from commit_tx's admission loop").
        if self.is_shutting_down() || self.is_failed() {
            let e = self.eio_str("commit aborted: volume is shutting down or failed");
            self.fail_batch(s, &e);
            return;
        }

        let total_len: u64 = s.entries.iter().map(|q| q.len).sum();

        // (2) ONE Σ ring admission for the batch (AdmissionClass::User),
        // BEFORE any node lock (§4.4 pt 5) — parks holding nothing the
        // drain needs (the queue entries' DLM guards are held exactly
        // the way today's parked committers hold theirs; the checkpoint
        // drain takes no DLM locks, ever), with the D1.b park-escalation
        // rung moved verbatim from the per-tx pipeline.
        let t_adm = std::time::Instant::now();
        match self.admit_user_budget(total_len).await {
            Ok(adm) => s.admission = Some(adm),
            Err(e) => {
                self.fail_batch(s, &e);
                return;
            }
        }
        meta_txpass_phase_record(MetaTxPassPhase::PassAdmission, t_adm);

        // Test seam (PR M4 D1.b, same protocol position as the per-tx
        // pipeline: admission held, nothing reserved — the historical
        // budget-leak hazard window, now owned by the detached pass).
        {
            let stall = TEST_COMMIT_ADMITTED_STALL_MS.load(Ordering::Relaxed);
            if stall > 0 {
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(stall)).await;
            }
        }

        // (3–5) The union locked window, with the whole-set
        // drop-all-and-relock retry (§5.5 lock-order analysis: never
        // re-resolving one member while holding the others' locks).
        // `pass_leaf_locks` spans the WHOLE (3–5) window — resolve, lock
        // acquire (checkpoint-freeze/SMO interference shows here),
        // revalidate, pre-images, reservation, RAM apply.
        let t_locks = std::time::Instant::now();
        let mut attempt = 0usize;
        let (res, undo, failed) = loop {
            attempt += 1;
            if attempt > COMMIT_RETRY_BUDGET {
                let adm = s.admission.take().expect("admission held until reserve");
                self.ring.core().release(adm);
                let e = KvError::Corrupt(
                    "commit retry budget exhausted (revalidation never passed — SMO \
                     protocol bug)"
                        .to_string(),
                );
                self.fail_batch(s, &e);
                return;
            }
            // Latch-free resolution: per-entry record index → leaf.
            let mut leaves: Vec<Vec<Arc<CachedNode>>> = Vec::with_capacity(s.entries.len());
            let mut resolve_err: Option<KvError> = None;
            'resolve: for q in &s.entries {
                let mut entry_leaves = Vec::with_capacity(q.recs.len());
                for (tree_id, r) in &q.recs {
                    match self.tree_by_id(*tree_id).resolve_leaf(&r.key).await {
                        Ok(l) => entry_leaves.push(l),
                        Err(e) => {
                            resolve_err = Some(e);
                            break 'resolve;
                        }
                    }
                }
                leaves.push(entry_leaves);
            }
            if let Some(e) = resolve_err {
                // Device-read class: the batch fails as a unit; nothing
                // was reserved or applied.
                let adm = s.admission.take().expect("admission held until reserve");
                self.ring.core().release(adm);
                self.fail_batch(s, &e);
                return;
            }
            // Deduped ascending-NodeId lock order over the UNION
            // (§4.4 pt 1 / §4.9 4b verbatim, over a union set).
            let mut lock_set: Vec<Arc<CachedNode>> = leaves.iter().flatten().cloned().collect();
            lock_set.sort_by_key(|n| n.addr());
            lock_set.dedup_by_key(|n| n.addr());
            let mut guards = Vec::with_capacity(lock_set.len());
            for node in &lock_set {
                guards.push(node.lock().write().await);
            }
            // Revalidate EVERY member under the locks (§4.6); any stale
            // leaf ⇒ drop ALL, re-resolve ALL, re-lock the union.
            let stale = s.entries.iter().zip(&leaves).any(|(q, entry_leaves)| {
                entry_leaves.iter().zip(&q.recs).any(|(leaf, (_, r))| {
                    leaf.state().is_superseded()
                        || r.key[..] < *leaf.min_key()
                        || r.key[..] > *leaf.max_key()
                })
            });
            if stale {
                drop(guards);
                super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // (4) Pre-images, once per distinct (tree, key) across the
            // WHOLE batch — first touch wins, so the batch-range rollback
            // restores the pre-batch state exactly (same-key members are
            // Δtime merge records by the co-queue invariant; their
            // shared pre-image is the pre-batch fold).
            let mut undo: Vec<UndoKey> = Vec::new();
            {
                let mut captured: std::collections::HashSet<(u8, Vec<u8>)> =
                    std::collections::HashSet::new();
                let mut cap_err: Option<KvError> = None;
                'capture: for (q, entry_leaves) in s.entries.iter().zip(&leaves) {
                    for ((tree_id, r), leaf) in q.recs.iter().zip(entry_leaves) {
                        if !captured.insert((*tree_id, r.key.clone())) {
                            continue;
                        }
                        match leaf.snapshot().lookup(&r.key) {
                            Ok(pre) => undo.push(UndoKey {
                                tree_id: *tree_id,
                                key: r.key.clone(),
                                pre,
                            }),
                            Err(e) => {
                                cap_err = Some(e);
                                break 'capture;
                            }
                        }
                    }
                }
                if let Some(e) = cap_err {
                    drop(guards);
                    let adm = s.admission.take().expect("admission held until reserve");
                    self.ring.core().release(adm);
                    self.fail_batch(s, &e);
                    return;
                }
            }

            // (5) ONE contiguous reservation inside the window; per-tx
            // entry seqs stamped back-to-back (seq_k = start + Σ len_j,
            // j < k); RAM apply in QUEUE order with per-tx undo. Same-
            // window reservation+apply keeps per-key journal-seq order
            // equal to RAM apply order (§4.4 pt 2) across the batch.
            let adm = s.admission.take().expect("admission held until reserve");
            let res = self.ring.reserve_registered(adm);
            s.reservation = Some(res);
            {
                let mut seq_cursor = res.start;
                for q in s.entries.iter_mut() {
                    for (i, (_t, r)) in q.recs.iter_mut().enumerate() {
                        r.seq = seq_cursor + i as u64;
                    }
                    seq_cursor += q.len;
                }
            }
            // Apply per tx, grouped per node within the tx (one apply +
            // one snapshot swap per touched node per tx). A member whose
            // apply fails is rolled OUT of the window immediately —
            // records removed under the still-held locks (no freeze can
            // interleave: writeback takes these same node locks) — and
            // fails alone; the batch survives (§5.5 isolation).
            let mut failed: Vec<(usize, KvError)> = Vec::new();
            let poison = TEST_CONVEYOR_POISON_APPLY_INO.load(Ordering::Relaxed);
            let mut seq_cursor = res.start;
            for (qi, (q, entry_leaves)) in s.entries.iter().zip(&leaves).enumerate() {
                let entry_start = seq_cursor;
                seq_cursor += q.len;
                let mut apply_err: Option<KvError> = None;
                for node in &lock_set {
                    let group: Vec<OwnedRec> = q
                        .recs
                        .iter()
                        .zip(entry_leaves)
                        .filter(|(_, leaf)| leaf.addr() == node.addr())
                        .map(|((_, r), _)| {
                            OwnedRec::new(
                                Bytes::copy_from_slice(&r.key),
                                r.seq,
                                r.kind,
                                Bytes::copy_from_slice(&r.value),
                            )
                        })
                        .collect();
                    if group.is_empty() {
                        continue;
                    }
                    let gi = lock_set
                        .iter()
                        .position(|n| n.addr() == node.addr())
                        .expect("node is in its own lock set");
                    // FIND-SMO-TAIL §1b: the floor contribution is the
                    // member's ENTRY START, not its records' raw seqs —
                    // a leaf holding only rec[j>0] of this multi-leaf tx
                    // must not pin the checkpoint tail mid-entry.
                    if let Err(e) = node.apply_locked(&mut guards[gi], group, entry_start) {
                        apply_err = Some(e);
                        break;
                    }
                }
                // Test seam: poison AFTER the apply so the in-window
                // removal machinery is exercised for real.
                if apply_err.is_none()
                    && poison != 0
                    && q.recs
                        .iter()
                        .any(|(t, r)| *t == TREE_INODES && r.key[..] == inode_key(poison)[..])
                {
                    apply_err = Some(KvError::Corrupt(
                        "TEST_CONVEYOR_POISON_APPLY_INO armed apply fault".to_string(),
                    ));
                }
                if let Some(e) = apply_err {
                    // In-window removal of exactly this member's records
                    // (already-applied prefix included; never-applied
                    // keys remove as no-ops).
                    for ((tree_id, r), leaf) in q.recs.iter().zip(entry_leaves) {
                        let _ = tree_id;
                        let gi = lock_set
                            .iter()
                            .position(|n| n.addr() == leaf.addr())
                            .expect("leaf is locked");
                        leaf.remove_overlay_records_locked(
                            &mut guards[gi],
                            &r.key,
                            entry_start,
                            entry_start + q.len,
                        );
                    }
                    failed.push((qi, e));
                }
            }
            // Surviving members' records are now applied to RAM and not
            // yet journaled — the exact span the panic sentinel's
            // fail-stop covers (RAM would diverge from replay).
            s.applied_unrolled = s.entries.len() > failed.len();
            // Writeback pressure (§4.6 pt 1's SECOND trigger) — flagged
            // inside the window, drained by the checkpoint task.
            let mut threshold_crossed = false;
            for (gi, node) in lock_set.iter().enumerate() {
                if guards[gi].overlay_bytes() >= self.cache.config().writeback_delta_bytes {
                    let tree_id = s
                        .entries
                        .iter()
                        .zip(&leaves)
                        .flat_map(|(q, entry_leaves)| q.recs.iter().zip(entry_leaves))
                        .find(|(_, leaf)| leaf.addr() == node.addr())
                        .map(|((t, _), _)| *t);
                    if let Some(tree_id) = tree_id {
                        self.tree_by_id(tree_id).enqueue_maintenance(node.addr());
                        threshold_crossed = true;
                    }
                }
            }
            drop(guards);
            if threshold_crossed {
                // Wake the task for a maintenance-only pass NOW (appends,
                // no barrier/ledger — those stay on cadence): letting the
                // open delta balloon for a whole tick makes every commit's
                // RAM apply pay O(delta) — the K7 create-row cliff.
                self.ckpt_wake.notify_one();
            }
            break (res, undo, failed);
        };
        meta_txpass_phase_record(MetaTxPassPhase::PassLeafLocks, t_locks);

        // (6) The pass's own bytes, outside every lock: the surviving
        // members' entries — N ORDINARY checksummed entries in the one
        // contiguous reservation, one `write_at_batch` submission. A
        // failed member's sub-range stays unwritten (the §4.4 pt 4
        // unwritten-hole mechanism; replay's checksum walk drops it).
        let t_jwrite = std::time::Instant::now();
        let write_out = {
            let mut parts: Vec<(Reservation, &[(u8, Record)])> = Vec::new();
            let mut seq_cursor = res.start;
            for (qi, q) in s.entries.iter().enumerate() {
                let start = seq_cursor;
                seq_cursor += q.len;
                if failed.iter().any(|(fi, _)| *fi == qi) {
                    continue;
                }
                parts.push((Reservation { start, len: q.len }, &q.recs));
            }
            if parts.is_empty() {
                Ok(())
            } else {
                self.ring.write_entries_batch(&parts).await
            }
        };
        // Completion is unconditional and pass-owned (commit_entry's
        // guarantee, batch edition): a reservation that never completes
        // would wedge the completed-prefix watermark. An unwritten /
        // failed range still completes — it is an abandoned hole.
        let res = s.reservation.take().expect("reservation registered");
        self.ring.complete(&res);

        match write_out {
            Ok(()) => {
                // (7) One completed-prefix wait covers every member
                // (their entries all end at-or-before the batch end;
                // chain-reachability per the K3 barrier observation).
                self.ring.wait_completed_upto(res.end()).await;
                self.journal_failures.store(0, Ordering::Release);
                // A skipped member left an unwritten hole in front of
                // acked survivors: checkpoint past it BEFORE acking, so
                // an immediate crash cannot strand chain-reachability of
                // what we are about to ack (§4.4 pt 4's hole discipline,
                // applied batch-mid).
                let mut hole_err: Option<String> = None;
                if !failed.is_empty() {
                    if let Err(ck) = self.checkpoint_past(res.end()).await {
                        log::error!(
                            "meta volume {}: post-isolation checkpoint could not cover the \
                             batch hole: {ck} (volume escalating; failing the survivors \
                             loud rather than acking unreachable entries)",
                            self.path.display()
                        );
                        self.note_journal_failure();
                        hole_err = Some(format!(
                            "batch hole checkpoint failed after a member rollback: {ck}"
                        ));
                    }
                }
                let barrier_out = if hole_err.is_none() && self.strict {
                    // (8) Strict-0 group commit: ONE coalesced fdatasync
                    // for the whole batch (§4.6 pt 4 / §5.5 — the G3
                    // mechanism: the barrier leaves the throughput path).
                    self.sync_device().await.map_err(KvError::Io)
                } else {
                    if hole_err.is_none() {
                        self.needs_flush.store(true, Ordering::Release);
                    }
                    Ok(())
                };
                meta_txpass_phase_record(MetaTxPassPhase::PassJournalWrite, t_jwrite);
                s.applied_unrolled = false;

                // Terminal outcomes, in queue order (the pass task fans
                // them out after releasing the backend ref).
                let mut failed = failed;
                // PR VL5b (§5.5.2 step 3): the pass task IS the key tee —
                // every user commit on the volume passes through here, so
                // one armed-tee load per batch captures every migrating-
                // keyspace key with zero cost when no migration runs.
                let tee = self.migration_tee.load_full();
                for (qi, q) in std::mem::take(&mut s.entries).into_iter().enumerate() {
                    if let Some(pos) = failed.iter().position(|(fi, _)| *fi == qi) {
                        let (_, e) = failed.swap_remove(pos);
                        s.outcomes.push((q, Err(e)));
                        continue;
                    }
                    let outcome = match (&hole_err, &barrier_out) {
                        (Some(msg), _) => Err(KvError::Io(self.eio(msg))),
                        (None, Err(e)) => Err(self.clone_kv_error(e)),
                        (None, Ok(())) => {
                            // D4.a: one successful commit_tx = one journal
                            // entry against the tx's construction site.
                            super::note_commit_site(q.site);
                            if let Some(tee) = tee.as_deref() {
                                tee.note_committed(&q.recs);
                            }
                            Ok(())
                        }
                    };
                    s.outcomes.push((q, outcome));
                }
            }
            Err(e) => {
                log::warn!(
                    "meta volume {}: batch journal write failed (seqs [{}, {})): {e} — \
                     rolling back {} member(s)",
                    self.path.display(),
                    res.start,
                    res.end(),
                    s.entries.len(),
                );
                // (8') Whole-batch rollback: the §4.4 pt 4 seq-conditional
                // machinery reused over the batch's contiguous range with
                // the first-touch pre-images (each member's records carry
                // seqs inside [start, end), so removal + skip-if-newer
                // compensation compose exactly as for one tx).
                self.rollback_failed_tx(res.start, res.end(), &undo).await;
                s.applied_unrolled = false;
                self.note_journal_failure();
                // The reserved range is now a PERMANENT hole in the ring
                // (§4.1 discovery loses same-page successors of a dead
                // chain): checkpoint past it — zero ring bytes by the
                // §4.4 pt 5 progress theorem.
                if let Err(ck) = self.checkpoint_past(res.end()).await {
                    log::error!(
                        "meta volume {}: post-failure checkpoint could not drain the \
                         journal hole: {ck} (volume escalating)",
                        self.path.display()
                    );
                    self.note_journal_failure();
                }
                let mut failed = failed;
                for (qi, q) in std::mem::take(&mut s.entries).into_iter().enumerate() {
                    let outcome = if let Some(pos) = failed.iter().position(|(fi, _)| *fi == qi) {
                        let (_, member_e) = failed.swap_remove(pos);
                        Err(member_e)
                    } else {
                        Err(self.clone_kv_error(&e))
                    };
                    s.outcomes.push((q, outcome));
                }
            }
        }
    }

    /// The §4.4 pt 5 user ring admission with the D1.b park-escalation
    /// rung, extracted verbatim from the per-tx pipeline (PR M7 moves it
    /// into the pass; the semantics — park holding nothing the drain
    /// needs, time-bounded liveness re-checks, `note_journal_failure`
    /// per threshold crossing, flag re-check after every park — are
    /// unchanged and pinned by the M4 watchdog suite).
    async fn admit_user_budget(
        &self,
        len: u64,
    ) -> std::result::Result<super::journal_core::Admission, KvError> {
        let threshold = self.timeout_threshold;
        let mut parked_since: Option<std::time::Instant> = None;
        loop {
            if let Some(adm) = self.ring.try_admit(len, AdmissionClass::User) {
                return Ok(adm);
            }
            self.stalls.fetch_add(1, Ordering::Relaxed);
            let notified = self.ring_space_notified();
            if let Some(adm) = self.ring.try_admit(len, AdmissionClass::User) {
                return Ok(adm);
            }
            // Re-check liveness flags after each park so shutdown/failure
            // cannot strand a parked pass (its queued txs fail out below).
            if self.is_shutting_down() || self.is_failed() {
                return Err(KvError::Io(
                    self.eio("commit aborted while parked for ring space"),
                ));
            }
            let since = *parked_since.get_or_insert_with(std::time::Instant::now);
            let until_crossing = threshold
                .saturating_sub(since.elapsed())
                .max(std::time::Duration::from_millis(10));
            // Dropping `notified` on tick expiry only unregisters this
            // waiter; the re-registration + try_admit recheck at the
            // loop top preserves the register-recheck-await shape.
            let _ = squeezefs_ipc::sqz_time::timeout(until_crossing, notified).await;
            if since.elapsed() >= threshold {
                log::error!(
                    "meta volume {}: conveyor pass parked {} ms (≥ {} ms) waiting for \
                     journal-ring admission ({len} B) — the drain is not advancing \
                     (wedged-not-failed class); escalating through the journal-failure \
                     lattice (D1.b audit row 2)",
                    self.path.display(),
                    since.elapsed().as_millis(),
                    threshold.as_millis(),
                );
                self.note_journal_failure();
                parked_since = None;
            }
            if self.is_shutting_down() || self.is_failed() {
                return Err(KvError::Io(
                    self.eio("commit aborted while parked for ring space"),
                ));
            }
        }
    }

    /// Fail every remaining batch member with (a clone of) one error —
    /// the batch-as-a-unit failure paths (pre-reserve). The outcomes are
    /// terminal: nothing was reserved or applied for these members.
    fn fail_batch(&self, s: &mut PassSentinel<'_>, e: &KvError) {
        for q in std::mem::take(&mut s.entries) {
            let err = self.clone_kv_error(e);
            s.outcomes.push((q, Err(err)));
        }
    }

    /// Per-member error instances for fan-out (`KvError` is not `Clone`;
    /// errno fidelity is preserved for the `Io` class, message fidelity
    /// for the rest — these are terminal error paths, never hot).
    fn clone_kv_error(&self, e: &KvError) -> KvError {
        match e {
            KvError::Io(crate::error::SqueezefsError::Io(ioe)) => {
                KvError::Io(crate::error::SqueezefsError::Io(match ioe.raw_os_error() {
                    Some(raw) => std::io::Error::from_raw_os_error(raw),
                    None => std::io::Error::new(ioe.kind(), ioe.to_string()),
                }))
            }
            KvError::Io(other) => KvError::Io(crate::error::SqueezefsError::Io(
                std::io::Error::other(other.to_string()),
            )),
            other => KvError::Corrupt(other.to_string()),
        }
    }

    /// `eio` over an owned message (the batch paths format contexts).
    fn eio_str(&self, what: &str) -> KvError {
        KvError::Io(self.eio(what))
    }

    /// A notified-future handle on the ring's space notify (private
    /// helper so the admission loop can use the register-recheck-await
    /// pattern without exposing the Notify).
    fn ring_space_notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.ring.space_notified()
    }

    /// **The §4.4 pt 4 seq-conditional rollback.** The locks were
    /// released before the failed write, so a concurrent transaction may
    /// have committed to the same key; an absolute pre-image restore
    /// would clobber it. Instead, per key:
    ///
    /// - **remove** this tx's records (seq ∈ `[lo, hi)`) from the open
    ///   overlay — removal is exactly LWW-correct: a newer committed
    ///   record (necessarily a Δtime merge record, the only same-key
    ///   writer the DLM discipline does not exclude) keeps folding onto
    ///   the restored base, which is precisely what replay computes for
    ///   the unwritten-hole entry;
    /// - if the newest record for the key still bears a seq in this tx's
    ///   range, the records escaped the open overlay (a checkpoint freeze
    ///   raced the failed write): apply a **compensating record** from
    ///   the captured pre-image, journaled through the checkpoint-class
    ///   reserve so replay agrees; a newer seq means a committed record
    ///   stands — skip (LWW-exact).
    ///
    /// The failing tx still holds its DLM I/D guards (the op returns only
    /// after rollback), so dependent readers and same-key `Put` writers
    /// stay excluded throughout — §4.4 pt 4's exactness argument.
    async fn rollback_failed_tx(&self, lo: u64, hi: u64, undo: &[UndoKey]) {
        // Phase 1: removal under re-acquired ascending locks.
        let mut comp: Vec<(u8, Vec<u8>, RecordKind, Bytes)> = Vec::new();
        for attempt in 0..COMMIT_RETRY_BUDGET {
            comp.clear();
            let mut leaves: Vec<Arc<CachedNode>> = Vec::with_capacity(undo.len());
            let mut resolve_failed = false;
            for u in undo {
                match self.tree_by_id(u.tree_id).resolve_leaf(&u.key).await {
                    Ok(l) => leaves.push(l),
                    Err(e) => {
                        log::error!("rollback: leaf resolution failed for a dirty key: {e}");
                        resolve_failed = true;
                        break;
                    }
                }
            }
            if resolve_failed {
                return; // volume is failing; escalation handles it
            }
            let mut lock_set: Vec<Arc<CachedNode>> = leaves.clone();
            lock_set.sort_by_key(|n| n.addr());
            lock_set.dedup_by_key(|n| n.addr());
            let mut guards = Vec::with_capacity(lock_set.len());
            for node in &lock_set {
                guards.push(node.lock().write().await);
            }
            let stale = leaves.iter().zip(undo).any(|(leaf, u)| {
                leaf.state().is_superseded()
                    || u.key[..] < *leaf.min_key()
                    || u.key[..] > *leaf.max_key()
            });
            if stale {
                drop(guards);
                super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
                if attempt + 1 == COMMIT_RETRY_BUDGET {
                    log::error!("rollback retry budget exhausted (SMO protocol bug)");
                    return;
                }
                continue;
            }
            for (u, leaf) in undo.iter().zip(&leaves) {
                let gi = lock_set
                    .iter()
                    .position(|n| n.addr() == leaf.addr())
                    .expect("leaf is locked");
                leaf.remove_overlay_records_locked(&mut guards[gi], &u.key, lo, hi);
                // Seq-conditional probe: did any of this tx's records
                // escape the open overlay?
                if let Some(newest) = leaf.snapshot().newest_record_seq(&u.key) {
                    if newest >= lo && newest < hi {
                        let (kind, value) = match &u.pre {
                            LiveLookup::Live(v) => (RecordKind::Put, Bytes::copy_from_slice(v)),
                            LiveLookup::Tombstone | LiveLookup::Absent => {
                                (RecordKind::Delete, Bytes::new())
                            }
                        };
                        comp.push((u.tree_id, u.key.clone(), kind, value));
                    }
                }
            }
            drop(guards);
            break;
        }
        if comp.is_empty() {
            return;
        }

        // Phase 2 (rare — a freeze raced the failed write): compensating
        // records through the checkpoint-class reserve, so a crash after
        // the escaped records reach a durable bset still replays to the
        // rolled-back state.
        let mut tx = KvTx::new();
        for (tree_id, key, kind, value) in comp {
            tx.staged.push((tree_id, key, kind, value));
        }
        if let Err(e) = self.commit_compensation(tx).await {
            log::error!(
                "meta volume {}: rollback compensation failed ({e}) — volume escalating",
                self.path.display()
            );
            self.note_journal_failure();
        }
    }

    /// Commit a compensation tx through the checkpoint-class reserve
    /// (never parks behind user admissions; skip-if-newer re-checked
    /// under the locks).
    async fn commit_compensation(&self, tx: KvTx) -> std::result::Result<(), KvError> {
        let mut recs: Vec<(u8, Record)> = tx
            .staged
            .into_iter()
            .map(|(tree_id, key, kind, value)| {
                (
                    tree_id,
                    Record {
                        key,
                        seq: 0,
                        kind,
                        // Same single-copy move as `commit_tx` (D1.c).
                        value: Vec::from(value),
                    },
                )
            })
            .collect();
        #[cfg(debug_assertions)]
        for (tree_id, r) in &recs {
            super::node::debug_audit_records(*tree_id, 0, std::slice::from_ref(r));
        }
        let len = entry_len_for(&recs)?;
        let Some(adm) = self.ring.try_admit(len, AdmissionClass::Checkpoint) else {
            return Err(KvError::JournalReserveExhausted { needed: len });
        };
        for attempt in 0..COMMIT_RETRY_BUDGET {
            let mut leaves: Vec<Arc<CachedNode>> = Vec::with_capacity(recs.len());
            for (tree_id, r) in &recs {
                leaves.push(self.tree_by_id(*tree_id).resolve_leaf(&r.key).await?);
            }
            let mut lock_set: Vec<Arc<CachedNode>> = leaves.clone();
            lock_set.sort_by_key(|n| n.addr());
            lock_set.dedup_by_key(|n| n.addr());
            let mut guards = Vec::with_capacity(lock_set.len());
            for node in &lock_set {
                guards.push(node.lock().write().await);
            }
            let stale = leaves.iter().zip(&recs).any(|(leaf, (_, r))| {
                leaf.state().is_superseded()
                    || r.key[..] < *leaf.min_key()
                    || r.key[..] > *leaf.max_key()
            });
            if stale {
                drop(guards);
                if attempt + 1 == COMMIT_RETRY_BUDGET {
                    self.ring.core().release(adm);
                    return Err(KvError::Corrupt(
                        "compensation retry budget exhausted".to_string(),
                    ));
                }
                continue;
            }
            let res = self.ring.reserve_registered(adm);
            for (i, (_t, r)) in recs.iter_mut().enumerate() {
                r.seq = res.start + i as u64;
            }
            for (i, (_t, r)) in recs.iter().enumerate() {
                let gi = lock_set
                    .iter()
                    .position(|n| n.addr() == leaves[i].addr())
                    .expect("leaf is locked");
                leaves[i].apply_locked(
                    &mut guards[gi],
                    vec![OwnedRec::new(
                        Bytes::copy_from_slice(&r.key),
                        r.seq,
                        r.kind,
                        Bytes::copy_from_slice(&r.value),
                    )],
                    // §1b floor rounding: the whole compensation tx is
                    // one entry starting at `res.start`.
                    res.start,
                )?;
            }
            drop(guards);
            return self.ring.commit_entry(&res, &recs).await;
        }
        unreachable!("loop returns or errors within the budget")
    }

    // -----------------------------------------------------------------
    // Op building blocks (shared by the trait impl and the routed arms).
    // -----------------------------------------------------------------

    /// Inode-timestamp stamps ride the kernel's coarse clock domain —
    /// see `crate::coarse_realtime_ns` (fstests generic/423: a fine-clock
    /// stamp here could LEAD the kernel's own locally-authored wb-cache
    /// cmtime by up to a tick, inverting cross-inode ctime order).
    fn now_ns() -> u64 {
        crate::coarse_realtime_ns()
    }

    fn to_inode(ino: Ino, v: &InodeValue) -> Inode {
        Inode {
            ino,
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            size: v.size,
            nlink: v.nlink,
            atime: v.atime,
            mtime: v.mtime,
            ctime: v.ctime,
            flags: v.flags,
            rdev: v.rdev,
        }
    }

    /// Dentry-value `file_type` byte from mode bits (§4.2: `dt` such that
    /// `dt << 12 == mode & S_IFMT`).
    fn ft_byte(mode: u32) -> u8 {
        ((mode & libc::S_IFMT) >> 12) as u8
    }

    /// Resolve `name` under `parent` to its full chain position
    /// (key + value), if live.
    async fn find_dentry_pos(
        &self,
        parent: Ino,
        name: &str,
    ) -> std::result::Result<Option<([u8; 16], DentryValue)>, KvError> {
        if name.len() > 255 {
            return Ok(None);
        }
        let hash = dentry_name_hash54(name.as_bytes(), self.sb.hash_seed);
        let start = dentry_key(parent, hash, 0);
        let end = dentry_key(parent, hash, u8::MAX);
        for (k, v) in self.chain_scan(&self.dentries, &start, &end).await? {
            let d = DentryValue::decode(&v)?;
            if d.name == name.as_bytes() {
                let mut key = [0u8; 16];
                key.copy_from_slice(&k);
                return Ok(Some((key, d)));
            }
        }
        Ok(None)
    }

    /// The occupied `coll_seq`s of one hash chain, folded through the tx
    /// overlay (RYOW: a slot this tx staged a `Delete` for — rename
    /// EXCHANGE — is free for its re-insert; a slot it staged a `Put`
    /// for is occupied). One live range scan; chain keys carry the
    /// `coll_seq` as their last byte (§4.2 composites).
    async fn chain_occupancy(
        &self,
        tx: &KvTx,
        tree: &KvTree,
        start: &[u8],
        end: &[u8],
    ) -> std::result::Result<Vec<u8>, KvError> {
        let mut occ: std::collections::BTreeSet<u8> = self
            .chain_scan(tree, start, end)
            .await?
            .iter()
            .map(|(k, _)| k[k.len() - 1])
            .collect();
        for (t, k, kind, _) in &tx.staged {
            if *t == tree.tree_id() && k[..] >= *start && k[..] <= *end {
                match kind {
                    RecordKind::Delete => {
                        occ.remove(&k[k.len() - 1]);
                    }
                    RecordKind::Put => {
                        occ.insert(k[k.len() - 1]);
                    }
                    RecordKind::Delta => {}
                }
            }
        }
        Ok(occ.into_iter().collect())
    }

    /// The chain key a NEW dentry for `(parent, name)` should occupy —
    /// the first free `coll_seq` in the seeded-hash chain, tx-overlay-
    /// aware. Chain exhaustion (a 257th same-hash name) is the clean
    /// counted refusal of §4.2.
    async fn dentry_insert_key(
        &self,
        tx: &KvTx,
        parent: Ino,
        name: &str,
    ) -> std::result::Result<[u8; 16], KvError> {
        if name.len() > 255 {
            return Err(KvError::NameTooLong { len: name.len() });
        }
        let hash = dentry_name_hash54(name.as_bytes(), self.sb.hash_seed);
        let start = dentry_key(parent, hash, 0);
        let end = dentry_key(parent, hash, u8::MAX);
        let occupied = self
            .chain_occupancy(tx, &self.dentries, &start, &end)
            .await?;
        match first_free_coll_seq(occupied) {
            Some(coll) => Ok(dentry_key(parent, hash, coll)),
            None => {
                super::META_KV_DENTRY_COLLISION_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                Err(KvError::DentryChainOverflow)
            }
        }
    }

    /// The chain key `(ino, name)`'s xattr record occupies: existing name
    /// ⇒ `(true, its key)` (overwrite-in-place); absent ⇒ `(false, the
    /// first free slot)`. Tx-overlay-aware for the slot assignment.
    async fn xattr_slot(
        &self,
        tx: &KvTx,
        ino: Ino,
        name: &str,
    ) -> std::result::Result<(bool, [u8; 16]), KvError> {
        if name.len() > 255 {
            return Err(KvError::NameTooLong { len: name.len() });
        }
        let hash = xattr_name_hash56(name.as_bytes(), self.sb.hash_seed);
        let start = xattr_key(ino, hash, 0);
        let end = xattr_key(ino, hash, u8::MAX);
        for (k, v) in self.chain_scan(&self.xattrs, &start, &end).await? {
            if XattrValue::decode(&v)?.name == name.as_bytes() {
                let mut key = [0u8; 16];
                key.copy_from_slice(&k);
                return Ok((true, key));
            }
        }
        let occupied = self.chain_occupancy(tx, &self.xattrs, &start, &end).await?;
        match first_free_coll_seq(occupied) {
            Some(coll) => Ok((false, xattr_key(ino, hash, coll))),
            None => {
                super::META_KV_DENTRY_COLLISION_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                Err(KvError::DentryChainOverflow)
            }
        }
    }

    /// Stage the parent-directory time update (§4.4 pt 6): under a
    /// SHARED parent lock a Δtime **merge record** (concurrent same-dir
    /// creates/unlinks never clobber each other's value fields); under an
    /// EXCLUSIVE parent lock a full `Put` carrying `nlink_delta` too.
    async fn stage_parent_update(
        &self,
        tx: &mut KvTx,
        parent: Ino,
        shared: bool,
        nlink_delta: i64,
        now: u64,
    ) -> std::result::Result<(), KvError> {
        if shared {
            debug_assert_eq!(
                nlink_delta, 0,
                "nlink mutation requires the exclusive parent"
            );
            tx.stage_delta(TREE_INODES, inode_key(parent), &InodeDelta::times(now, now));
            return Ok(());
        }
        let Some(mut pv) = self.read_inode_value(parent).await? else {
            // The v2 create/link paths tolerate an unreadable parent when
            // updating times (best-effort `if let Ok`); mirror that.
            return Ok(());
        };
        pv.mtime = now;
        pv.ctime = now;
        match nlink_delta.cmp(&0) {
            std::cmp::Ordering::Greater => pv.nlink += nlink_delta as u32,
            std::cmp::Ordering::Less => {
                let dec = (-nlink_delta) as u32;
                // The v2 guard: a directory's nlink never drops below 2
                // through the parent-side decrement.
                if pv.nlink > 2 {
                    pv.nlink -= dec;
                } else {
                    // POSIX-11: the floor SUPPRESSED a real decrement, so
                    // the parent's link count is permanently one too low
                    // for the subdirectories it still holds — and `find`'s
                    // leaf optimization (`nlink == 2` ⇒ "no subdirs")
                    // then SKIPS them. The guard stays (a count that
                    // underflows is worse), but a suppression is a
                    // BUG SIGNAL, never routine: count it and say so.
                    crate::fuse_client::METRICS
                        .dir_nlink_underflows
                        .fetch_add(1, Ordering::Relaxed);
                    log::warn!(
                        "directory nlink underflow guard fired on parent {parent}: nlink \
                         {} cannot absorb a -{dec} parent-side decrement — the count is \
                         already at/below the floor and the deficit is now permanent \
                         (POSIX-11; run `squeezefs fsck` on this volume)",
                        pv.nlink
                    );
                }
            }
            std::cmp::Ordering::Equal => {}
        }
        tx.stage_put(TREE_INODES, inode_key(parent), pv.encode());
        Ok(())
    }

    /// §4.8 batched destroy: DLM-exclusive on every ino, revalidate under
    /// the locks (live `nlink > 0` and missing inos skip — the v2
    /// contract), then ONE journal entry carrying the inode `Delete`s
    /// plus each ino's enumerated xattr `Delete`s (the keys are
    /// contiguous in `TREE_XATTRS`; typically just `"layout"`). No ino
    /// free: v3 never reuses (§4.8), which deletes the v2
    /// free-strictly-after-durable ordering rule whole.
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        self.destroy_inode_records(inos, true).await
    }

    /// [`Self::destroy_inodes`] **without** the live-`nlink` skip — fsck
    /// class C9's repair verb, and its only caller.
    ///
    /// The skip exists to protect a LIVE inode from a racing reclaim
    /// (nlink > 0 means a name still points here). A C9 finding is the
    /// verified statement that no name does: the inode was minted in a
    /// prior writer era (so no in-flight create can own it) and a full
    /// dentry-tree pass, re-run after the settle window under this ino's
    /// exclusive 4a lease, found nothing naming it. That is a strictly
    /// stronger proof than the nlink counter — which is exactly what the
    /// residue lies about (a crashed cross-volume create leaves
    /// `nlink == 1` and no name).
    ///
    /// Destroying the record and its xattrs in ONE journaled transaction
    /// is also what keeps repair crash-safe: there is no window where the
    /// filesystem holds an `nlink == 0` unreferenced inode, which no class
    /// claims and nothing would ever reclaim.
    pub async fn destroy_unreferenced_inodes(&self, inos: &[Ino]) -> Result<()> {
        self.destroy_inode_records(inos, false).await
    }

    async fn destroy_inode_records(&self, inos: &[Ino], skip_live: bool) -> Result<()> {
        self.write_gate()?;
        if inos.is_empty() {
            return Ok(());
        }
        let lock_plan: Vec<(u64, LockMode)> =
            inos.iter().map(|&ino| (ino, LockMode::Exclusive)).collect();
        let guards: Arc<[DlmGuard]> = Arc::from(self.dlm.lock_many(&lock_plan, &[]).await);

        let mut tx = KvTx::new();
        let mut doomed = 0usize;
        for &ino in inos {
            match self.read_inode_value(ino).await? {
                Some(v) if skip_live && v.nlink > 0 => {
                    log::debug!("destroy_inodes: ino {ino} has nlink {}, skipping", v.nlink);
                }
                Some(_) => {
                    doomed += 1;
                    tx.stage_delete(TREE_INODES, inode_key(ino));
                    // Reap the corpse's xattrs in the SAME entry (§4.8).
                    let start = xattr_key(ino, 0, 0);
                    let end = xattr_key(ino, HASH56_MAX, u8::MAX);
                    let mut cursor: Vec<u8> = start.to_vec();
                    loop {
                        let page = self.xattrs.range(&cursor, &end, SCAN_PAGE).await?;
                        let Some((last, _)) = page.last() else { break };
                        cursor = key_successor(last);
                        for (k, _) in &page {
                            tx.stage_delete(TREE_XATTRS, k.to_vec());
                        }
                    }
                }
                None => {} // missing or already destroyed: nothing to do
            }
        }
        if doomed == 0 {
            return Ok(());
        }
        crate::fuse_client::METRICS
            .meta_reclaim_batch_size
            .record(doomed);
        tx.hold_guards(guards.clone());
        self.commit_tx(tx).await?;
        // POSIX-1: the live-inode gauge moves only on a COMMITTED destroy
        // (a failed commit leaves the records live, and the bisect retry
        // re-counts the halves it actually lands).
        self.destroyed_inodes
            .fetch_add(doomed as u64, Ordering::Relaxed);
        // PR M6: a destroyed corpse's pending times refinement is moot —
        // GC it under the exclusive locks (the drain would drop it on the
        // missing-inode read anyway; this keeps the map tight).
        for &ino in inos {
            self.retire_pending_times(ino);
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Routed-layer op parts (PR K6b: `RoutedMetaBackend`'s v3 arms).
    //
    // The caller (the routed layer) holds the DLM I/D guards — level 4a
    // of P1-9 — exactly like the v2 arms drive `dentry::`/`inode::`
    // fragments under routed locks. Semantics mirror the routed v2 code
    // paths (global child inos in dentries, `nlink = 2` directories,
    // parent nlink bumps, Δtime merge records under SHARED parents),
    // which differ deliberately from the single-volume trait impl above
    // (that mirrors the retired v2 trait behavior — the conformance
    // surface).
    // -----------------------------------------------------------------

    /// Routed dentry resolution: `(stored child ino (global), S_IFMT
    /// bits)` — the v2 `DiskDentry { child_ino, file_type }` shape.
    pub async fn routed_find_dentry(
        &self,
        local_parent: Ino,
        name: &str,
    ) -> Result<Option<(Ino, u32)>> {
        Ok(self
            .find_dentry(local_parent, name)
            .await?
            .map(|d| (d.child_ino, u32::from(d.file_type) << 12)))
    }

    /// Whether a directory has any live entries (the routed rename's
    /// ENOTEMPTY probe).
    pub async fn dir_has_entries(&self, local_ino: Ino) -> Result<bool> {
        let start = dentry_key(local_ino, 0, 0);
        let end = dentry_key(local_ino, HASH54_MAX, u8::MAX);
        Ok(!self.dentries.range(&start, &end, 1).await?.is_empty())
    }

    /// Routed same-volume create — ONE whole-tx entry: EEXIST check,
    /// setgid inheritance, monotonic local ino (global stored in the
    /// dentry), `nlink = 2` directories, parent bump/Δtime.
    pub async fn routed_create_local(
        &self,
        local_parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        // POSIX-3: the inode's size AT CREATE, committed in this same
        // whole-tx entry. 0 for every ordinary create; the symlink
        // handler passes strlen(target) so the DURABLE record sizes the
        // link (a cache-only patch reported st_size == 0 on the first
        // post-TTL lstat()).
        initial_size: u64,
        // PR VL5b: the routed layer pre-allocates BOTH — the effective
        // local key ino (native watermark or guest-namespaced cursor
        // mint) and its global encoding (which rides the mint slot, not
        // this volume's index) — so this backend stays keyspace-agnostic.
        local_ino: Ino,
        global_ino: Ino,
        // Rung 13 (UPDATE intents): the CLIENT's mint instant for an
        // intent apply — the record's times, so a stat answers the same
        // times before and after the flush. `None` = the owner's clock
        // (every ordinary create).
        ts_override: Option<u64>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<Inode> {
        self.write_gate()?;
        if self.find_dentry(local_parent, name).await?.is_some() {
            return Err(crate::error::SqueezefsError::already_exists(
                "File already exists",
            ));
        }
        let parent_v = self
            .read_inode_value(local_parent)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {local_parent} not found")))?;
        let mut final_gid = gid;
        let mut final_mode = mode;
        if (parent_v.mode & libc::S_ISGID) != 0 {
            final_gid = parent_v.gid;
            if (mode & libc::S_IFMT) == libc::S_IFDIR {
                final_mode |= libc::S_ISGID;
            }
        }
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let now = ts_override.unwrap_or_else(Self::now_ns);
        let child = InodeValue {
            mode: final_mode,
            uid,
            gid: final_gid,
            nlink: if is_dir { 2 } else { 1 },
            flags: 0,
            rdev,
            // POSIX-3: durable at create (0 for every ordinary create).
            size: initial_size,
            atime: now,
            mtime: now,
            ctime: now,
        };
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_ino), child.encode());
        let dkey = self.dentry_insert_key(&tx, local_parent, name).await?;
        tx.stage_put(
            TREE_DENTRIES,
            dkey,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(global_ino, Self::ft_byte(final_mode), name.as_bytes())?,
        );
        // Directory (exclusive parent): nlink+1 + times; regular file
        // (SHARED parent): the §4.4 pt 6 Δtime merge record.
        self.stage_parent_update(&mut tx, local_parent, !is_dir, i64::from(is_dir), now)
            .await?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(Self::to_inode(global_ino, &child))
    }

    /// Cross-volume create, target side: mint the local inode record.
    pub async fn routed_mint_inode(
        &self,
        local_ino: Ino,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        // POSIX-3: see `routed_create_local`.
        initial_size: u64,
        // Rung 13: see `routed_create_local`.
        ts_override: Option<u64>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<InodeValue> {
        self.write_gate()?;
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let now = ts_override.unwrap_or_else(Self::now_ns);
        let v = InodeValue {
            mode,
            uid,
            gid,
            nlink: if is_dir { 2 } else { 1 },
            flags: 0,
            rdev,
            // POSIX-3: durable at create (0 for every ordinary create).
            size: initial_size,
            atime: now,
            mtime: now,
            ctime: now,
        };
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_ino), v.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(v)
    }

    /// Parent-side dentry insert (cross-volume create/link tail, rename
    /// insert): dentry Put + the routed parent update.
    pub async fn routed_add_dentry(
        &self,
        local_parent: Ino,
        name: &str,
        global_child: Ino,
        ft_mode_bits: u32,
        parent_update: RoutedParentUpdate,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        let dkey = self.dentry_insert_key(&tx, local_parent, name).await?;
        tx.stage_put(
            TREE_DENTRIES,
            dkey,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(global_child, Self::ft_byte(ft_mode_bits), name.as_bytes())?,
        );
        let now = Self::now_ns();
        match parent_update {
            RoutedParentUpdate::None => {}
            RoutedParentUpdate::SharedTimes => {
                self.stage_parent_update(&mut tx, local_parent, true, 0, now)
                    .await?
            }
            RoutedParentUpdate::ExclusiveTimes => {
                self.stage_parent_update(&mut tx, local_parent, false, 0, now)
                    .await?
            }
            RoutedParentUpdate::ExclusiveTimesBump => {
                self.stage_parent_update(&mut tx, local_parent, false, 1, now)
                    .await?
            }
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Routed same-volume unlink — ONE whole-tx entry with the routed v2
    /// semantics (directory child ⇒ nlink 0; parent nlink−1 for dirs;
    /// Δtime under the shared parent).
    pub async fn routed_unlink_local(
        &self,
        local_parent: Ino,
        name: &str,
        local_child: Ino,
        is_dir: bool,
        parent_shared: bool,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let Some((dkey, _d)) = self.find_dentry_pos(local_parent, name).await? else {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Dentry not found",
            )));
        };
        let now = Self::now_ns();
        let mut tx = KvTx::new();
        tx.stage_delete(TREE_DENTRIES, dkey);
        self.stage_parent_update(
            &mut tx,
            local_parent,
            parent_shared,
            if is_dir { -1 } else { 0 },
            now,
        )
        .await?;
        let mut child = self
            .read_inode_value(local_child)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {local_child} not found")))?;
        if is_dir {
            child.nlink = 0;
        } else if child.nlink > 0 {
            child.nlink -= 1;
        }
        child.ctime = now;
        tx.stage_put(TREE_INODES, inode_key(local_child), child.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Routed same-volume link — ONE whole-tx entry: nlink+1 + ctime,
    /// dentry Put (global child ino), best-effort parent times (the
    /// routed v2 arm's shape). EEXIST is the caller's check (it holds
    /// the D-guard).
    pub async fn routed_link_local(
        &self,
        local_parent: Ino,
        name: &str,
        local_child: Ino,
        global_child: Ino,
        guards: Arc<[DlmGuard]>,
    ) -> Result<Inode> {
        self.write_gate()?;
        let mut child = self
            .read_inode_value(local_child)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {local_child} not found")))?;
        if child.nlink >= 65000 {
            return Err(crate::error::SqueezefsError::too_many_links(
                "Too many links",
            ));
        }
        child.nlink += 1;
        // Monotone ctime bump over the FOLDED view: the reply is served
        // from this value, so it must never regress below a previously
        // served base+refinement (generic/423 inversion class; the
        // setattr commit-arm discipline — fold, carry, retire).
        self.fold_pending_times(local_child, &mut child);
        let now = Self::now_ns();
        if (now as i64) > (child.ctime as i64) {
            child.ctime = now;
        }
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_child), child.encode());
        let dkey = self.dentry_insert_key(&tx, local_parent, name).await?;
        tx.stage_put(
            TREE_DENTRIES,
            dkey,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(global_child, Self::ft_byte(child.mode), name.as_bytes())?,
        );
        self.stage_parent_update(&mut tx, local_parent, false, 0, now)
            .await?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        // The committed Put carries the folded refinement (I-guard held
        // by the routed caller: race-free).
        self.retire_pending_times(local_child);
        Ok(Self::to_inode(global_child, &child))
    }

    /// Child-side nlink adjustment (cross-volume unlink / link / rename
    /// destination): `delta = +1` (link), `-1` (unlink/replace), with the
    /// routed dir rule (`is_dir` unlink zeroes). Returns the post-update
    /// value.
    pub async fn routed_nlink_adjust(
        &self,
        local_child: Ino,
        delta: i64,
        is_dir_unlink: bool,
        guards: Arc<[DlmGuard]>,
    ) -> Result<InodeValue> {
        self.write_gate()?;
        let mut v = self
            .read_inode_value(local_child)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {local_child} not found")))?;
        if delta > 0 {
            if v.nlink >= 65000 {
                return Err(crate::error::SqueezefsError::too_many_links(
                    "Too many links",
                ));
            }
            v.nlink += delta as u32;
        } else if is_dir_unlink {
            v.nlink = 0;
        } else if v.nlink > 0 {
            v.nlink = v.nlink.saturating_sub((-delta) as u32);
        }
        // Monotone ctime bump over the FOLDED view (the local `link`'s
        // generic/423 discipline — the returned value is served, so it
        // must never regress below a previously served base+refinement).
        self.fold_pending_times(local_child, &mut v);
        let now = Self::now_ns();
        if (now as i64) > (v.ctime as i64) {
            v.ctime = now;
        }
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_child), v.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        // The committed Put carries the folded refinement (I-guard held
        // by the routed caller: race-free).
        self.retire_pending_times(local_child);
        Ok(v)
    }

    /// Routed same-volume rename — ONE whole-tx entry covering the v2
    /// arm's semantics: EXCHANGE swap, NOREPLACE guard, replace with
    /// destination nlink accounting + ENOTEMPTY (when the destination
    /// inode is local: `dest_local`), directory-move parent nlink shifts —
    /// **plus the PR M6 D4.b time surface in the SAME entry**: Δtime merge
    /// records on both parents (folded into the nlink-shift `Put`s when a
    /// directory move already rewrites them), and a Δctime on the moved
    /// inode when it lives on this volume (`src_local`; a remote child's
    /// stamp is the routed layer's per-volume fragment). One entry means
    /// one crash exposure: naming and times commit or revert together
    /// (`tests/crash_contract_tests.rs` rename atomicity), and strict mode
    /// pays ONE barrier where the old fragment shape paid two.
    #[allow(clippy::too_many_arguments)] // the routed rename's parameter surface
    pub async fn routed_rename_local(
        &self,
        local_old_parent: Ino,
        old_name: &str,
        local_new_parent: Ino,
        new_name: &str,
        flags: u32,
        src_local: Option<Ino>,
        dest_local: Option<Ino>,
        // RENAME_WHITEOUT (fstests generic/631, the overlayfs-upper
        // contract): `(local key ino, global dentry ino)` pre-allocated
        // by the routed layer — the whiteout char-0:0 inode and its
        // dentry at the OLD name ride this same whole-tx entry.
        whiteout: Option<(Ino, Ino)>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let old_pos = self.find_dentry_pos(local_old_parent, old_name).await?;
        let new_pos = self.find_dentry_pos(local_new_parent, new_name).await?;
        let mut tx = KvTx::new();
        let now = Self::now_ns();
        if flags & libc::RENAME_EXCHANGE != 0 {
            let enoent = || {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            };
            let (old_key, old_d) = old_pos.ok_or_else(enoent)?;
            let (new_key, new_d) = new_pos.ok_or_else(enoent)?;
            tx.stage_delete(TREE_DENTRIES, old_key);
            tx.stage_delete(TREE_DENTRIES, new_key);
            let nk = self
                .dentry_insert_key(&tx, local_new_parent, new_name)
                .await?;
            tx.stage_put(
                TREE_DENTRIES,
                nk,
                // D1.c single-copy staging (no intermediate name Vec).
                DentryValue::encode_parts(old_d.child_ino, old_d.file_type, new_name.as_bytes())?,
            );
            let ok = self
                .dentry_insert_key(&tx, local_old_parent, old_name)
                .await?;
            tx.stage_put(
                TREE_DENTRIES,
                ok,
                // D1.c single-copy staging (no intermediate name Vec).
                DentryValue::encode_parts(new_d.child_ino, new_d.file_type, old_name.as_bytes())?,
            );
            // D4.b: both parents' Δtimes + both swapped inodes' Δctimes
            // ride the swap entry (dedup the shared-parent / same-inode
            // shapes).
            tx.stage_delta(
                TREE_INODES,
                inode_key(local_old_parent),
                &InodeDelta::times(now, now),
            );
            if local_new_parent != local_old_parent {
                tx.stage_delta(
                    TREE_INODES,
                    inode_key(local_new_parent),
                    &InodeDelta::times(now, now),
                );
            }
            if let Some(src) = src_local {
                tx.stage_delta(TREE_INODES, inode_key(src), &InodeDelta::ctime(now));
            }
            if let Some(dst) = dest_local {
                if src_local != Some(dst) {
                    tx.stage_delta(TREE_INODES, inode_key(dst), &InodeDelta::ctime(now));
                }
            }
            return self.commit_tx(tx).await.map_err(Into::into);
        }
        if flags & libc::RENAME_NOREPLACE != 0 && new_pos.is_some() {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EEXIST),
            ));
        }
        let Some((old_key, old_d)) = old_pos else {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Source dentry not found",
            )));
        };
        let is_dir = u32::from(old_d.file_type) << 12 == libc::S_IFDIR;
        let cross_dir = local_old_parent != local_new_parent;
        if is_dir && cross_dir {
            // Parent nlink shift, best-effort like the v2 arm — the D4.b
            // parent times fold into these full `Put`s (same records,
            // same entry; no separate Δtime needed below).
            if let Some(mut op) = self.read_inode_value(local_old_parent).await? {
                if op.nlink > 2 {
                    op.nlink -= 1;
                }
                op.mtime = now;
                op.ctime = now;
                tx.stage_put(TREE_INODES, inode_key(local_old_parent), op.encode());
            }
            if let Some(mut np) = self.read_inode_value(local_new_parent).await? {
                np.nlink += 1;
                np.mtime = now;
                np.ctime = now;
                tx.stage_put(TREE_INODES, inode_key(local_new_parent), np.encode());
            }
        } else {
            // D4.b: parent Δtime merge records (one per distinct parent).
            tx.stage_delta(
                TREE_INODES,
                inode_key(local_old_parent),
                &InodeDelta::times(now, now),
            );
            if cross_dir {
                tx.stage_delta(
                    TREE_INODES,
                    inode_key(local_new_parent),
                    &InodeDelta::times(now, now),
                );
            }
        }
        let mut dest_replaced = None;
        if let Some((new_key, _new_d)) = new_pos {
            if let Some(dest) = dest_local {
                if let Some(mut dv) = self.read_inode_value(dest).await? {
                    if (dv.mode & libc::S_IFMT) == libc::S_IFDIR {
                        if self.dir_has_entries(dest).await? {
                            return Err(crate::error::SqueezefsError::Io(
                                std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                            ));
                        }
                        // A replaced directory is REMOVED: both its "."
                        // self-link and its parent entry vanish — nlink 0,
                        // the routed_unlink_local rmdir rule (fstests
                        // generic/035; pinned in
                        // tests/rename_semantics_tests.rs).
                        dv.nlink = 0;
                    } else if dv.nlink > 0 {
                        dv.nlink -= 1;
                    }
                    dv.ctime = now;
                    tx.stage_put(TREE_INODES, inode_key(dest), dv.encode());
                    dest_replaced = Some(dest);
                }
            }
            tx.stage_delete(TREE_DENTRIES, new_key);
        }
        tx.stage_delete(TREE_DENTRIES, old_key);
        let nk = self
            .dentry_insert_key(&tx, local_new_parent, new_name)
            .await?;
        tx.stage_put(
            TREE_DENTRIES,
            nk,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(old_d.child_ino, old_d.file_type, new_name.as_bytes())?,
        );
        // D4.b: the moved inode's ctime, in the same entry (skip when the
        // dest-replace `Put` above already stamped the same local inode —
        // rename over a hardlink of itself).
        if let Some(src) = src_local {
            if dest_replaced != Some(src) {
                tx.stage_delta(TREE_INODES, inode_key(src), &InodeDelta::ctime(now));
            }
        }
        // RENAME_WHITEOUT: mint the char-0:0 whiteout at the OLD name in
        // the SAME whole-tx entry (atomic with the move — overlayfs'
        // rename-whiteout-over-victim depends on it). VFS contract:
        // S_IFCHR, perm 0, rdev 0; owner 0:0 (whiteouts are mounter
        // plumbing — overlayfs checks type + rdev only).
        if let Some((w_local, w_global)) = whiteout {
            let wv = InodeValue {
                mode: libc::S_IFCHR,
                uid: 0,
                gid: 0,
                nlink: 1,
                flags: 0,
                rdev: 0,
                size: 0,
                atime: now,
                mtime: now,
                ctime: now,
            };
            tx.stage_put(TREE_INODES, inode_key(w_local), wv.encode());
            let wk = self
                .dentry_insert_key(&tx, local_old_parent, old_name)
                .await?;
            tx.stage_put(
                TREE_DENTRIES,
                wk,
                DentryValue::encode_parts(
                    w_global,
                    Self::ft_byte(libc::S_IFCHR),
                    old_name.as_bytes(),
                )?,
            );
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// PR M6 D4.b, the cross-volume fragment: stamp `ctime = now` on a
    /// moved/exchanged inode that lives on a DIFFERENT volume than the
    /// rename's dentry surgery (per-volume fragments — cross-volume
    /// renames were never transactional across volumes). Best-effort on a
    /// missing inode, like every routed rename fragment.
    pub async fn routed_touch_ctime(&self, local_ino: Ino, guards: Arc<[DlmGuard]>) -> Result<()> {
        self.write_gate()?;
        if self.read_inode_value(local_ino).await?.is_none() {
            return Ok(());
        }
        let mut tx = KvTx::new();
        tx.stage_delta(
            TREE_INODES,
            inode_key(local_ino),
            &InodeDelta::ctime(Self::now_ns()),
        );
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Rename-replace destination handling when the destination inode
    /// lives on ANOTHER volume: ENOTEMPTY probe + nlink dec + ctime (the
    /// v2 arm's `if let Ok` best-effort shape).
    pub async fn routed_dest_replace(
        &self,
        local_dest: Ino,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let Some(mut dv) = self.read_inode_value(local_dest).await? else {
            return Ok(());
        };
        if (dv.mode & libc::S_IFMT) == libc::S_IFDIR {
            if self.dir_has_entries(local_dest).await? {
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                ));
            }
            // Replaced directory ⇒ nlink 0 (the rmdir rule — generic/035).
            dv.nlink = 0;
        } else if dv.nlink > 0 {
            dv.nlink -= 1;
        }
        dv.ctime = Self::now_ns();
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_dest), dv.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// §5.3: layout xattr + size as ONE two-record transaction (v2's
    /// non-transactional two-write path, made atomic on v3) — the
    /// fsync/release writeback shape.
    pub async fn set_layout_and_size(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
    ) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        v.size = size;
        // NEVER author times here (generic/003 remount divergence,
        // 2026-07-28): this is size+layout bookkeeping for data whose
        // times the writing op already stamped (the handler's parked
        // refinement / the kernel's flush-times SETATTR). A fabricated
        // fresh-tick ctime was a second clock authority that outran every
        // value the daemon had served — visible only after remount.
        // (`read_inode_value` is deliberately unfolded here: the write's
        // parked refinement stays pending across this Put — still
        // fold-visible on reads, still drained as a Δtime on top of it —
        // so the Put can never regress the freshest served times.)
        let tx0 = KvTx::new();
        let (_existing, key) = self.xattr_slot(&tx0, ino, "layout").await?;
        let mut tx = tx0;
        tx.stage_put(
            TREE_XATTRS,
            key,
            XattrValue {
                name: b"layout".to_vec(),
                value: layout.to_vec(),
            }
            .encode()?,
        );
        tx.stage_put(TREE_INODES, inode_key(ino), v.encode());
        // Spec §6.2 item 1: the accounting rides THIS tx (no second
        // commit). Silently skipped on a volume without incompat bit 8 —
        // that volume's ownership answers stay derived state.
        if self.block_refs.is_some() {
            tx.stage_block_refs(block_refs);
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Spec §6.2 item 1: commit a standalone set of durable
    /// block-reference operations — the reclaim path's release (a corpse
    /// carries no layout save to ride) and the fsck repair seam.
    ///
    /// Deliberately NOT on the publish path: block publishes stage their
    /// accounting into the layout transaction ([`Self::set_layout_and_size`]
    /// / [`Self::merge_layout_and_size`]) so the no-second-commit property
    /// the write-commit-economy campaign bought is preserved. A no-op — not
    /// an error — on a volume without incompat bit 8.
    pub async fn commit_block_refs(
        &self,
        ino: Ino,
        ops: &[super::block_refs::BlockRefOp],
    ) -> Result<()> {
        if self.block_refs.is_none() || ops.is_empty() {
            return Ok(());
        }
        self.write_gate()?;
        // The ino's I-guard: the records belong to this ino's ownership
        // set, so the same 4a lock that serializes its layout commits
        // serializes their release (lock order unchanged — 4a before 4b,
        // which `commit_tx` takes).
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let mut tx = KvTx::new();
        tx.stage_block_refs(ops);
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Write-commit-economy campaign (2026-07-30): the block-publish
    /// commit — [`Self::set_layout_and_size`] semantics with an
    /// O(batch)-bytes **layout delta record** where a live inline
    /// bincode base exists to fold onto, and the caller-provided full
    /// layout as the always-correct fallback. Returns whether the delta
    /// was staged (the caller's chain accounting). One two-record
    /// transaction either way: {layout delta | layout Put} + inode Put —
    /// size can never lead its data's map (they ride ONE checksummed
    /// journal entry; the generic/795 law by construction).
    ///
    /// Delta eligibility here is the backend's OWN half of the ladder:
    /// a live layout record must exist at the slot and must not be a
    /// legacy-JSON value (byte peek). The **caller** owns the other
    /// half — never passing a delta whose base is `indirect:` or whose
    /// RAM authority diverged from the persisted base outside the map
    /// (the routing `layout_delta_chain` accounting); the fold's
    /// `decode_base_layout` refusals make a violated rule loud, never
    /// silent.
    pub async fn merge_layout_and_size(
        &self,
        ino: Ino,
        refs_owner: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: Bytes,
        size: u64,
        block_refs: Vec<super::block_refs::BlockRefOp>,
    ) -> Result<bool> {
        self.merge_layout_and_size_ext(ino, refs_owner, delta, full_layout, size, block_refs, false)
            .await
            .map(|(used, _version)| used)
    }

    /// DLM S11 rung 17 — [`Self::merge_layout_and_size`] in **chain-onto-
    /// head** mode (KD-MW-8's composition law; the design-mw-layout-
    /// versions §6 residual this rung pays): on a `KV_LAYOUT_VERSIONS`
    /// volume the delta's base claim is RE-STAMPED, under this backend's
    /// own 4a I-guard, to the durable head just probed, and its link
    /// version is re-minted from THIS process's sequencer — so a shipped
    /// co-writer's delta composes onto whatever head its peers produced
    /// instead of refusing (the refetch wedge) or re-basing with the
    /// caller's private full layout (the clobber that minted the
    /// s11-range leg's C8 drift). At the chain cap the OWNER compacts:
    /// the folded durable layout + this delta re-base as one full `Put`
    /// computed from the AUTHORITY's state, never the caller's.
    ///
    /// Returns `(use_delta, staged_version)` — the staged link's version
    /// (0 on a full-Put commit), the publish reply's chain-without-
    /// refetch input. Unreachable from any solo path (the callers are
    /// the S9 publish serve and the granted-ino local arm).
    pub async fn merge_layout_and_size_chained(
        &self,
        ino: Ino,
        refs_owner: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: Bytes,
        size: u64,
        block_refs: Vec<super::block_refs::BlockRefOp>,
    ) -> Result<(bool, u64)> {
        self.merge_layout_and_size_ext(ino, refs_owner, delta, full_layout, size, block_refs, true)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn merge_layout_and_size_ext(
        &self,
        ino: Ino,
        refs_owner: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: Bytes,
        size: u64,
        block_refs: Vec<super::block_refs::BlockRefOp>,
        chain: bool,
    ) -> Result<(bool, u64)> {
        // Rewrite-publish-drain Lever B (2026-08-01): delta-class layout
        // saves aggregate on the per-volume layout-merge conveyor — one
        // multi-ino KvTx per drained window (one journal entry, one ring
        // write, one fan-out) instead of one commit per ino. The field
        // decomposition named the per-ino commit chain (guard → inode
        // read → slot probe → commit_tx → wake, 2.2 ms at a 92 %-utilized
        // journal-conveyor server) as the dominant rewrite publish
        // constituent; aggregation divides the commits, ring writes, and
        // wake hops by the window. `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1`
        // is the A/B lever — the pre-campaign per-save path, verbatim.
        self.write_gate()?;
        if crate::routing::publish_commit_group_max() == Some(1) {
            return self
                .merge_layout_and_size_direct(
                    ino,
                    refs_owner,
                    delta,
                    &full_layout,
                    size,
                    &block_refs,
                    chain,
                )
                .await;
        }
        let (done, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        // Spec §6.2 item 9: the versioned wire rides ONLY volumes whose
        // superblock carries KV_LAYOUT_VERSIONS — an un-stamped volume
        // strips the pair at encode and stays byte-identical to the
        // shipped bit-5 wire (a pre-item-9 binary keeps reading it).
        let delta_wire = Bytes::from(if self.layout_versions_stamped() {
            delta.encode()
        } else {
            delta.encode_unversioned()
        });
        let weight = (delta_wire.len() + full_layout.len()) as u64;
        // Enqueue-then-elect with no await between (the conveyor_core
        // no-lost-wakeup protocol).
        self.layout_conveyor.enqueue(
            QueuedLayoutMerge {
                ino,
                refs_owner,
                delta_wire,
                full_layout,
                size,
                block_refs,
                chain,
                done,
            },
            weight,
        );
        if self.layout_conveyor.try_lead() {
            let conveyor = Arc::clone(&self.layout_conveyor);
            let weak = self.conveyor_self.get().cloned().ok_or_else(|| {
                KvError::Corrupt(
                    "conveyor identity missing (layout merge before open wiring?)".to_string(),
                )
            })?;
            // Detached (the M7 cancellation-safety law): no client-visible
            // cancellation can drop a batch mid-commit. Stage 1c venue:
            // sqz-meta lanes (plane-critical — the publish plane's twin).
            crate::meta_exec::spawn_meta(
                "kv_layout_merge_pass",
                Self::layout_merge_pass_task(conveyor, weak),
            );
        }
        // The wedge census gauges this park too (2026-08-07) — the
        // publish plane's twin of META_COMMIT_PARKED.
        let _parked = super::ParkedGaugeGuard::enter(&super::META_PUBLISH_PARKED);
        match rx.await {
            Ok(out) => out,
            Err(_) => Err(self.eio(
                "layout-merge conveyor pass dropped its result channel (pass panic — \
                 save failed loud; custody stays with the caller's never-lossy ladder)",
            )),
        }
    }

    /// The detached layout-merge pass task (Lever B): drain → one
    /// aggregated commit → fan out, until the queue idles; then release
    /// leadership (release-then-recheck). Holds the backend per batch
    /// only (Weak between batches — the M7 lifecycle discipline).
    async fn layout_merge_pass_task(
        conveyor: Arc<ConveyorCore<QueuedLayoutMerge>>,
        weak: Weak<KvMetaBackend>,
    ) {
        loop {
            // Test seam: let concurrent-ino saves accumulate
            // deterministically (`TEST_LAYOUT_MERGE_HOLD_MS`).
            let hold = TEST_LAYOUT_MERGE_HOLD_MS.load(Ordering::Relaxed);
            if hold > 0 {
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold)).await;
            }
            let Some(be) = weak.upgrade() else {
                // Backend dropped without shutdown: queued entries are
                // dropped-committer residue — fail them loud, release
                // leadership (the M7 pass shape).
                loop {
                    for q in conveyor.drain(usize::MAX, u64::MAX) {
                        let _ = q.done.send(Err(crate::error::SqueezefsError::Io(
                            std::io::Error::other(
                                "meta volume dropped with layout merges still queued",
                            ),
                        )));
                    }
                    if !conveyor.unlead_and_recheck() {
                        return;
                    }
                }
            };
            let cap = crate::routing::publish_commit_group_max().unwrap_or(be.batch_max_txs);
            let batch = conveyor.drain(cap, be.batch_max_bytes);
            if batch.is_empty() {
                drop(be); // never park on leadership holding the backend
                if !conveyor.unlead_and_recheck() {
                    return;
                }
                continue;
            }
            be.layout_merge_pass(batch).await;
            drop(be);
        }
    }

    /// One aggregated layout-merge batch: the batch's I-guards in ONE
    /// deduped ascending `lock_many` plan, every member's
    /// {layout delta | full Put} + inode Put staged into ONE KvTx, ONE
    /// `commit_tx` — with per-op isolation (a NotFound / slot-probe
    /// failure fails that member ALONE; survivors commit). The panic
    /// guard fails everything queued loud (never a wedged conveyor).
    async fn layout_merge_pass(self: &Arc<Self>, batch: Vec<QueuedLayoutMerge>) {
        use crate::fuse_client::{publish_phase_record, PublishPhase};
        struct PassGuard {
            conveyor: Arc<ConveyorCore<QueuedLayoutMerge>>,
            clean: bool,
        }
        impl Drop for PassGuard {
            fn drop(&mut self) {
                if self.clean {
                    return;
                }
                loop {
                    for q in self.conveyor.drain(usize::MAX, u64::MAX) {
                        let _ = q.done.send(Err(crate::error::SqueezefsError::Io(
                            std::io::Error::other(
                                "layout-merge conveyor pass panicked — save failed loud",
                            ),
                        )));
                    }
                    if !self.conveyor.unlead_and_recheck() {
                        break;
                    }
                }
            }
        }
        let mut guard = PassGuard {
            conveyor: Arc::clone(&self.layout_conveyor),
            clean: false,
        };

        // Duplicate an unclonable error to every surviving member,
        // preserving nothing but the message (fencing/NotFound classes
        // are per-op and never duplicated).
        fn dup_err(e: &crate::error::SqueezefsError) -> crate::error::SqueezefsError {
            crate::error::SqueezefsError::Io(std::io::Error::other(format!(
                "aggregated layout commit failed: {e}"
            )))
        }

        // The batch's 4a I-guards, ONE deduped ascending plan (the
        // `destroy_inodes` precedent — deadlock-free by ordering).
        let t_guard = std::time::Instant::now();
        let lock_plan: Vec<(u64, LockMode)> =
            batch.iter().map(|q| (q.ino, LockMode::Exclusive)).collect();
        let guards: Arc<[DlmGuard]> = Arc::from(self.dlm.lock_many(&lock_plan, &[]).await);
        publish_phase_record(PublishPhase::CommitGuard, t_guard);

        let mut tx = KvTx::new();
        // Per-op outcomes: staged members await the shared commit;
        // failed members own their error immediately. The fourth field
        // is the rung-20 compose arm's RES-9 blob guard (disarmed on
        // commit Ok; dropped-armed on Err, freeing the fresh blob).
        #[allow(clippy::type_complexity)]
        let mut staged: Vec<(
            squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<(bool, u64)>>,
            bool,
            u64,
            Option<super::indirect_map::IndirectBlobGuard>,
        )> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut failed: Vec<(
            squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<(bool, u64)>>,
            crate::error::SqueezefsError,
        )> = Vec::new();
        // Spec §6.2 item 9 (versioned volumes): the tree probe reads the
        // COMMITTED chain, but this pass stages several members into ONE
        // tx — a second member for the SAME ino must gate against the
        // head this pass just staged, or two links naming one base could
        // enter one commit (the exact fork the gate exists to refuse).
        let versions_stamped = self.layout_versions_stamped();
        let mut batch_heads: std::collections::HashMap<Ino, (u32, Option<(u64, u64)>)> =
            std::collections::HashMap::new();
        // Rung 20 residual 1 — the PASS-LOCAL compose memo (the batch-
        // prior law's blob face): same-ino batch mates compose onto the
        // ACCUMULATED view, never a re-read of the COMMITTED blob —
        // `xattrs.lookup` folds committed state, so re-reading would
        // erase a pass mate's just-staged entries while their ledger refs
        // land (the exact "1 durable vs 0 layout references" mint).
        struct ComposedState {
            /// The accumulated composed layout, its FULL map held
            /// `Some(..)` inside the memo (the staged Puts carry
            /// `block_map: None` + the fresh blob's name).
            layout: crate::layout_wire::LayoutMetadata,
            /// The COMMITTED head's blob — displaced once, by the first
            /// successful compose. `None` when the chain entered the
            /// memo at an inline→oversize CROSSING (2026-08-19 wedge
            /// trigger fix): an INLINE head names no blob, so there is
            /// nothing to release and nothing to free.
            old_blob: Option<String>,
            /// The latest staged-but-uncommitted fresh blob for this ino
            /// (a successor member releases + frees it).
            prior_fresh: Option<String>,
        }
        let mut composed_heads: std::collections::HashMap<Ino, ComposedState> =
            std::collections::HashMap::new();
        // Blob keys the commit stops naming: freed strictly AFTER commit
        // Ok (old blobs once per composed ino + every superseded
        // intermediate fresh key); on commit Err freed NEVER (the
        // committed heads still name the old blobs).
        let mut displaced_blobs: Vec<String> = Vec::new();
        for mut op in batch {
            let t_iread = std::time::Instant::now();
            let v = match self.read_inode_value(op.ino).await {
                Ok(Some(mut v)) => {
                    v.size = op.size;
                    v
                }
                Ok(None) => {
                    // The reclaimed-ino face: NotFound classification is
                    // load-bearing for the never-lossy ladder's
                    // verified-orphan-discard arm — per-op, never shared.
                    failed.push((
                        op.done,
                        Self::not_found(format!("Inode {} not found", op.ino)),
                    ));
                    continue;
                }
                Err(e) => {
                    failed.push((op.done, e.into()));
                    continue;
                }
            };
            publish_phase_record(PublishPhase::CommitInodeRead, t_iread);
            // NEVER author times here — the `set_layout_and_size`
            // clock-authority rule verbatim (generic/003; the unfolded
            // `read_inode_value` keeps parked Δtime refinements pending).
            let t_slot = std::time::Instant::now();
            let (existing, key) = match self.xattr_slot(&tx, op.ino, "layout").await {
                Ok(x) => x,
                Err(e) => {
                    failed.push((op.done, e.into()));
                    continue;
                }
            };
            // The backend eligibility half, per member: a live non-JSON
            // base + the durable incompat ratchet.
            let mut use_delta = false;
            let mut probe_depth = 0u32;
            // Rung 17: the staged link's version (0 = full Put /
            // unversioned) and the chain-onto-head compaction override
            // (the folded-durable full Put replacing the caller's).
            let mut staged_version = 0u64;
            let mut chained_full: Option<Vec<u8>> = None;
            // Rung 20 residual 1: this member's fresh-blob RES-9 guard
            // (compose arm only) — disarmed with the batch commit.
            let mut member_blob_guard: Option<super::indirect_map::IndirectBlobGuard> = None;
            // The inline→oversize crossing's fresh blob (2026-08-19
            // wedge trigger fix): its MAP_BLOB take joins the recomputed
            // refs below. NO released twin and nothing joins
            // `displaced_blobs` — the displaced head was INLINE.
            let mut crossing_fresh: Option<String> = None;
            if existing {
                match self.xattrs.lookup(&key).await {
                    Ok(Some(cur)) => {
                        let base_ok = XattrValue::decode(&cur)
                            .map(|x| !x.value.starts_with(b"{"))
                            .unwrap_or(false);
                        // DUR-8b (aggregated twin of the direct path):
                        // the cap must bound the DURABLE chain, not the
                        // caller's RAM counter, which a metadata-cache
                        // refill resets to 0. An earlier member of THIS
                        // pass already moved the ino's head: its staged
                        // state wins over the committed probe.
                        let max_chain = crate::routing::layout_delta_max_chain();
                        let (depth, head_versions) = match batch_heads.get(&op.ino) {
                            Some(&h) => h,
                            None => match self.xattrs.delta_chain_probe(&key).await {
                                Ok(p) => p,
                                Err(e) => {
                                    failed.push((op.done, e.into()));
                                    continue;
                                }
                            },
                        };
                        probe_depth = depth;
                        if base_ok && max_chain > 0 && depth < max_chain {
                            use_delta = self.layout_deltas_ready().await;
                        }
                        // Rung 19 (the MPI-IO row's live conviction): an
                        // INDIRECT head can neither fold a delta (the
                        // fold's own law calls a staged one corruption —
                        // "layout delta base unusable: indirect base",
                        // and the `{`-peek above admitted exactly that:
                        // every subsequent lookup/checkpoint of the key
                        // then refuses FOREVER) nor absorb the owner-side
                        // compaction (apply refuses the same base). A
                        // chained member on one REFUSES with the
                        // retried-class marker and stages NOTHING; the
                        // caller's ladder re-composes from the refetched
                        // indirect truth. Solo/un-chained members are
                        // untouched (their caller half already gates on
                        // its own RAM `block_map_id`).
                        //
                        // A memo entry counts as an indirect head TOO
                        // (2026-08-19 wedge trigger fix): a batch-prior
                        // member that CROSSED to indirect staged a head
                        // `xattrs.lookup` cannot see — a mate that took
                        // the delta arm here would stage a delta onto
                        // the just-staged indirect head, the exact
                        // fold-poison the rung-19 gate exists to refuse.
                        let head_indirect = op.chain
                            && versions_stamped
                            && base_ok
                            && (composed_heads.contains_key(&op.ino)
                                || XattrValue::decode(&cur).is_ok_and(|x| {
                                    matches!(
                                        crate::layout_wire::decode_base_layout(&x.value),
                                        Err(e) if format!("{e}").contains("indirect")
                                    )
                                }));
                        if head_indirect {
                            // Rung 20 residual 1: UNARMED keeps the
                            // refusal verbatim; an ARMED authority
                            // composes onto the FULL rehydrated map,
                            // through the PASS-LOCAL memo so same-ino
                            // batch mates accumulate (the batch-prior
                            // law's blob face — a re-read of the
                            // COMMITTED blob would erase a mate's
                            // just-staged entries).
                            let Some(io) = super::indirect_map::indirect_map_io() else {
                                failed.push((
                                    op.done,
                                    crate::error::SqueezefsError::InvalidOperation(format!(
                                        "layout delta base unusable: indirect base — ino {}'s                                      durable head spilled to an indirect map; a chained                                      delta cannot fold onto it and a partial full Put                                      would clobber it (rung 19: refetch and recompose)",
                                        op.ino
                                    )),
                                ));
                                continue;
                            };
                            let state = match composed_heads.entry(op.ino) {
                                std::collections::hash_map::Entry::Occupied(entry) => {
                                    entry.into_mut()
                                }
                                std::collections::hash_map::Entry::Vacant(vacant) => {
                                    // First same-ino member: rehydrate the
                                    // COMMITTED blob into the memo. No
                                    // displaced-name bookkeeping yet — the
                                    // old blob is displaced only by the
                                    // first compose that actually STAGES.
                                    let head = match XattrValue::decode(&cur) {
                                        Ok(x) => {
                                            match crate::layout_wire::decode_layout_any(&x.value) {
                                                Ok(h) => h,
                                                Err(e) => {
                                                    failed.push((
                                                        op.done,
                                                        crate::error::SqueezefsError::InvalidOperation(
                                                            format!(
                                                                "rung 20: undecodable indirect head \
                                                                 for ino {}: {e}",
                                                                op.ino
                                                            ),
                                                        ),
                                                    ));
                                                    continue;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            failed.push((op.done, e.into()));
                                            continue;
                                        }
                                    };
                                    let Some(old_blob) = head
                                        .block_map_id
                                        .as_deref()
                                        .and_then(|id| id.strip_prefix("indirect:"))
                                        .map(str::to_string)
                                    else {
                                        // The gate matched the decode ERROR
                                        // TEXT; the head itself names no
                                        // blob — refuse verbatim.
                                        failed.push((
                                            op.done,
                                            crate::error::SqueezefsError::InvalidOperation(format!(
                                                "layout delta base unusable: indirect base — ino {}'s                                      durable head spilled to an indirect map; a chained                                      delta cannot fold onto it and a partial full Put                                      would clobber it (rung 19: refetch and recompose)",
                                                op.ino
                                            )),
                                        ));
                                        continue;
                                    };
                                    // The blob read happens BEFORE any KvTx
                                    // staging (no node locks held; the 4a
                                    // I-guards across data I/O follow the
                                    // conveyor precedent).
                                    let full = match (io.read)(old_blob.clone()).await {
                                        Ok(f) => f,
                                        Err(e) => {
                                            failed.push((op.done, e));
                                            continue;
                                        }
                                    };
                                    let mut layout = head;
                                    layout.block_map = Some(full.into_iter().collect());
                                    vacant.insert(ComposedState {
                                        layout,
                                        old_blob: Some(old_blob),
                                        prior_fresh: None,
                                    })
                                }
                            };
                            let d = match crate::layout_wire::LayoutDelta::decode(&op.delta_wire) {
                                Ok(d) => d,
                                Err(e) => {
                                    failed.push((
                                        op.done,
                                        crate::error::SqueezefsError::InvalidOperation(format!(
                                            "rung 20: undecodable shipped layout delta for \
                                             ino {}: {e}",
                                            op.ino
                                        )),
                                    ));
                                    continue;
                                }
                            };
                            // The accounting: this member's entries
                            // against the ACCUMULATED view (the memo's
                            // map, pre-apply).
                            let mut composed_refs = if self.block_refs.is_some() {
                                state.layout.block_map.as_ref().and_then(|memo_map| {
                                    Self::recompute_refs_against_map(
                                        memo_map,
                                        &d.entries,
                                        op.refs_owner,
                                        &op.block_refs,
                                    )
                                })
                            } else {
                                None
                            };
                            // Candidate = memo + this delta. Committed
                            // into the memo ONLY after its blob write
                            // succeeds — a failed mate must not corrupt
                            // the view its pass successors compose onto.
                            let mut candidate = state.layout.clone();
                            d.apply_to(&mut candidate);
                            // `apply_to` clobbered `block_map_id` with
                            // the caller's stale id — the fresh key
                            // overrides below.
                            let candidate_map = candidate.block_map.take().unwrap_or_default();
                            let mut sorted: Vec<(u32, String)> =
                                candidate_map.iter().map(|(b, k)| (*b, k.clone())).collect();
                            sorted.sort_unstable_by_key(|&(b, _)| b);
                            let (new_key, guard) = match (io.write)(op.ino, sorted).await {
                                Ok(w) => w,
                                Err(e) => {
                                    failed.push((op.done, e));
                                    continue;
                                }
                            };
                            candidate.block_map_id = Some(format!("indirect:{new_key}"));
                            let full = match crate::layout_wire::encode_layout(&candidate) {
                                Ok(f) => f,
                                Err(e) => {
                                    failed.push((
                                        op.done,
                                        crate::error::SqueezefsError::InvalidOperation(format!(
                                            "rung 20: composed indirect head re-encode failed \
                                             for ino {}: {e}",
                                            op.ino
                                        )),
                                    ));
                                    continue;
                                }
                            };
                            // The blob custody transfer: the PRIOR staged
                            // name (this pass's latest fresh key, else
                            // the committed blob) released, the fresh
                            // taken. `None` prior = the chain entered
                            // the memo at an inline CROSSING and this is
                            // unreachable (the crossing seeds
                            // `prior_fresh`), kept structural: an inline
                            // head releases nothing.
                            let prior_named =
                                state.prior_fresh.clone().or_else(|| state.old_blob.clone());
                            if let Some(refs) = composed_refs.as_mut() {
                                if let Some(prior) = prior_named.as_deref() {
                                    super::indirect_map::push_map_blob_transfer_op(
                                        refs,
                                        prior,
                                        op.refs_owner,
                                        false,
                                    );
                                }
                                super::indirect_map::push_map_blob_transfer_op(
                                    refs,
                                    &new_key,
                                    op.refs_owner,
                                    true,
                                );
                            }
                            if let Some(refs) = composed_refs {
                                op.block_refs = refs;
                            }
                            // Staging is certain from here: commit the
                            // candidate into the memo and record the name
                            // the commit stops carrying.
                            match state.prior_fresh.replace(new_key) {
                                Some(prev) => displaced_blobs.push(prev),
                                None => {
                                    if let Some(old) = state.old_blob.clone() {
                                        displaced_blobs.push(old);
                                    }
                                }
                            }
                            state.layout = candidate;
                            state.layout.block_map = Some(candidate_map);
                            chained_full = Some(full);
                            use_delta = false;
                            member_blob_guard = Some(guard);
                            crate::fuse_client::METRICS
                                .publish_blob_composes
                                .fetch_add(1, Ordering::Relaxed);
                        } else if op.chain && versions_stamped && base_ok {
                            // KD-MW-8's composition law (rung 17): the
                            // SHIPPED/granted-ino arm never gate-refuses
                            // and never re-bases with the caller's
                            // private full layout. Below the cap the
                            // claim RE-STAMPS onto the durable head (the
                            // link version re-minted from THIS process's
                            // sequencer — cross-writer versions cannot
                            // collide); at the cap the OWNER compacts:
                            // the folded durable layout + this delta as
                            // one full Put computed from the AUTHORITY's
                            // state.
                            //
                            // ONE exception, found live by the rung's own
                            // from-zero leg (C8 drift 8742, clean bytes):
                            // an ino with a BATCH-PRIOR member in THIS
                            // pass must never take the compaction arm —
                            // `xattrs.lookup` folds the COMMITTED state,
                            // so the full Put would erase the pass mate's
                            // just-staged entries while their ledger refs
                            // land (the exact "1 durable record vs 0
                            // layout references" mint). A batch-prior ino
                            // stays on the chained delta past the cap;
                            // the NEXT pass compacts. Pinned red-first by
                            // `a_batch_prior_ino_never_compacts_over_its_
                            // own_pass_mates`.
                            if !use_delta
                                && batch_heads.contains_key(&op.ino)
                                && self.layout_deltas_ready().await
                            {
                                use_delta = true;
                            }
                            let minted = if use_delta {
                                crate::dlm::mint_layout_version()
                            } else {
                                0
                            };
                            if use_delta && minted != 0 {
                                let head_v = head_versions.map(|(_, v)| v).unwrap_or(0);
                                match crate::layout_wire::restamp_delta_versions(
                                    &op.delta_wire,
                                    head_v,
                                    minted,
                                ) {
                                    Ok(wire) => {
                                        op.delta_wire = Bytes::from(wire);
                                        staged_version = minted;
                                    }
                                    Err(e) => {
                                        failed.push((
                                            op.done,
                                            crate::error::SqueezefsError::InvalidOperation(
                                                format!(
                                                    "rung 17: undecodable shipped layout \
                                                     delta for ino {}: {e}",
                                                    op.ino
                                                ),
                                            ),
                                        ));
                                        continue;
                                    }
                                }
                            } else {
                                // The owner-side compaction: fold the
                                // durable current + this delta into one
                                // full Put (never the caller's layout).
                                use_delta = false;
                                let folded = match XattrValue::decode(&cur) {
                                    Ok(x) => x.value.to_vec(),
                                    Err(e) => {
                                        failed.push((op.done, e.into()));
                                        continue;
                                    }
                                };
                                let applied =
                                    crate::layout_wire::LayoutDelta::decode(&op.delta_wire)
                                        .and_then(|d| d.apply(&folded));
                                match applied {
                                    Ok(full) => {
                                        // The inline→oversize CROSSING
                                        // (2026-08-19 wedge trigger fix):
                                        // spill (armed) or refuse
                                        // (unarmed) — NEVER stage the
                                        // oversize record.
                                        match self.spill_oversize_chained_full(op.ino, &full).await
                                        {
                                            Ok(None) => chained_full = Some(full),
                                            Ok(Some(spill)) => {
                                                // Seed the pass-local
                                                // memo (the batch-prior
                                                // law's crossing face):
                                                // a same-ino mate must
                                                // compose onto THIS
                                                // accumulated view —
                                                // never stage a delta
                                                // onto the just-staged
                                                // indirect head, never
                                                // re-read the committed
                                                // (still-inline) blob.
                                                let mut memo_layout = spill.head;
                                                memo_layout.block_map = Some(spill.map);
                                                composed_heads.insert(
                                                    op.ino,
                                                    ComposedState {
                                                        layout: memo_layout,
                                                        old_blob: None,
                                                        prior_fresh: Some(spill.fresh_key.clone()),
                                                    },
                                                );
                                                chained_full = Some(spill.encoded);
                                                member_blob_guard = Some(spill.guard);
                                                crossing_fresh = Some(spill.fresh_key);
                                            }
                                            Err(e) => {
                                                failed.push((op.done, e));
                                                continue;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        failed.push((
                                            op.done,
                                            crate::error::SqueezefsError::InvalidOperation(
                                                format!(
                                                    "rung 17: owner-side chain compaction \
                                                     failed for ino {}: {e}",
                                                    op.ino
                                                ),
                                            ),
                                        ));
                                        continue;
                                    }
                                }
                            }
                        } else if use_delta && versions_stamped {
                            // Spec §6.2 item 9: the durable base-name gate
                            // — Stage / re-base / REFUSE loud (per member:
                            // a diverged claim fails ALONE, survivors
                            // commit).
                            match self.admit_versioned_delta(
                                op.ino,
                                crate::layout_wire::layout_delta_versions(&op.delta_wire),
                                depth,
                                head_versions,
                            ) {
                                Ok(stage) => use_delta = stage,
                                Err(e) => {
                                    failed.push((op.done, e));
                                    continue;
                                }
                            }
                            if use_delta {
                                staged_version =
                                    crate::layout_wire::layout_delta_versions(&op.delta_wire)
                                        .map(|(_, v)| v)
                                        .unwrap_or(0);
                            }
                        }
                        // Rung 19 (the width-N refs composition): a CHAINED
                        // member's accounting is recomputed from the
                        // transition this commit performs — the entries
                        // onto the folded head — replacing the caller's
                        // frame. Un-chained members (the solo path) keep
                        // their ops verbatim. The restamp above rewrites
                        // only the wire's version pair, so decoding here
                        // reads the same entries the caller shipped. The
                        // rung-20 compose arm already owns its accounting
                        // (recomputed against the ACCUMULATED view) —
                        // re-running here would clobber it back to the
                        // caller's frame (`recompute_chained_refs`
                        // returns `None` on an indirect head).
                        if op.chain && self.block_refs.is_some() && !head_indirect {
                            if let Ok(d) = crate::layout_wire::LayoutDelta::decode(&op.delta_wire) {
                                if let Some(refs) = Self::recompute_chained_refs(
                                    &cur,
                                    &d.entries,
                                    op.refs_owner,
                                    &op.block_refs,
                                ) {
                                    op.block_refs = refs;
                                }
                            }
                            // The crossing's blob custody: ONE take for
                            // the fresh blob — no released twin (the
                            // displaced head was INLINE, not a blob).
                            if let Some(fresh) = crossing_fresh.as_deref() {
                                super::indirect_map::push_map_blob_transfer_op(
                                    &mut op.block_refs,
                                    fresh,
                                    op.refs_owner,
                                    true,
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failed.push((op.done, e.into()));
                        continue;
                    }
                }
            }
            publish_phase_record(PublishPhase::CommitSlotProbe, t_slot);
            if versions_stamped {
                // Record the head THIS member is about to stage for any
                // later same-ino member of the pass (delta: one deeper,
                // head = this wire's pair; full Put: a bare re-base).
                batch_heads.insert(
                    op.ino,
                    if use_delta {
                        (
                            probe_depth.saturating_add(1),
                            crate::layout_wire::layout_delta_versions(&op.delta_wire),
                        )
                    } else {
                        (0, None)
                    },
                );
            }
            if use_delta {
                super::META_KV_LAYOUT_DELTA_BYTES
                    .fetch_add(op.delta_wire.len() as u64, Ordering::Relaxed);
                super::META_KV_LAYOUT_DELTA_COMMITS.fetch_add(1, Ordering::Relaxed);
                tx.stage_delta_raw(TREE_XATTRS, key, op.delta_wire);
            } else {
                super::META_KV_LAYOUT_FULL_COMMITS.fetch_add(1, Ordering::Relaxed);
                let full_src: &[u8] = match &chained_full {
                    Some(full) => full,
                    None => &op.full_layout,
                };
                let full = match XattrValue::encode_parts(b"layout", full_src) {
                    Ok(f) => f,
                    Err(e) => {
                        failed.push((op.done, e.into()));
                        continue;
                    }
                };
                tx.stage_put(TREE_XATTRS, key, full);
            }
            tx.stage_put(TREE_INODES, inode_key(op.ino), v.encode());
            // Spec §6.2 item 1: this member's accounting joins the SAME
            // aggregated tx as its layout record — one journal entry for
            // the whole window, accounting included. A member that failed
            // its inode read / slot probe above `continue`d before this
            // point, so no orphan accounting can be staged for a save
            // that never happens.
            if self.block_refs.is_some() {
                tx.stage_block_refs(&op.block_refs);
            }
            staged.push((op.done, use_delta, staged_version, member_blob_guard));
        }
        for (done, e) in failed {
            let _ = done.send(Err(e));
        }
        if staged.is_empty() {
            guard.clean = true;
            return;
        }
        crate::fuse_client::METRICS
            .publish_commit_groups
            .fetch_add(1, Ordering::Relaxed);
        crate::fuse_client::METRICS
            .publish_commit_group_saves
            .fetch_add(staged.len() as u64, Ordering::Relaxed);
        tx.hold_guards(guards);
        let t_tx = std::time::Instant::now();
        let out = self.commit_tx(tx).await;
        publish_phase_record(PublishPhase::CommitTxWait, t_tx);
        match out {
            Ok(()) => {
                for (done, use_delta, staged_version, blob_guard) in staged {
                    // Rung 20: the batch commit named this member's fresh
                    // blob — custody transfers.
                    if let Some(mut g) = blob_guard {
                        g.disarm();
                    }
                    let _ = done.send(Ok((use_delta, staged_version)));
                }
                // Rung 20: the names the commit stopped carrying (old
                // blobs + superseded intermediates) are freed only NOW —
                // the DUR-6 CoW law (freeing before the barrier would
                // destroy a still-committed map).
                if !displaced_blobs.is_empty() {
                    if let Some(io) = super::indirect_map::indirect_map_io() {
                        for blob_key in displaced_blobs {
                            (io.free)(blob_key).await;
                        }
                    }
                }
            }
            Err(e) => {
                let e: crate::error::SqueezefsError = e.into();
                // The fresh-blob guards drop ARMED with `staged` — every
                // composed member's fresh blob is freed; the displaced
                // names are freed NEVER (the committed heads still carry
                // them).
                for (done, _, _, _blob_guard) in staged {
                    let _ = done.send(Err(dup_err(&e)));
                }
            }
        }
        guard.clean = true;
    }

    /// DLM S11 rung 19 — **the width-N refs composition**: recompute a
    /// CHAINED merge's staged durable accounting from the transition this
    /// commit actually performs — the delta's entries applied onto the
    /// FOLDED durable head (`cur`, just looked up under this member's own
    /// 4a I-guard) — never the caller's frame. At width N the caller
    /// computed its ops against its private RAM base under its own
    /// `INODE_META_LOCKS`, which legitimately lags its peers: staged
    /// verbatim, the head's displaced binding is stranded ("1 durable vs
    /// 0 layout references" — the s11-blockcyclic C8 face) and a stale
    /// caller release deletes a record the composition KEEPS (the
    /// swapped pair's loss half). O(batch + inline-head decode): the
    /// head decode is bounded by the xattr value cap (a format constant),
    /// paid only on the chained (multi-writer) plane — the solo path
    /// never calls this.
    ///
    /// `None` keeps the caller's ops: no resolver armed (un-armed mounts,
    /// where chained merges cannot occur in production —
    /// `multi_writer::arm_multi_writer` installs it) or a head that is
    /// not an inline decodable layout (the legacy/undecodable face; the
    /// INDIRECT face routes through the rung-20 compose arms before this
    /// is ever called — they recompute against the FULL rehydrated map
    /// via [`Self::recompute_refs_against_map`]). The caller's MAP-BLOB
    /// ops (the indirect blob custody transfer, index-disjoint from map
    /// entries) always travel verbatim.
    ///
    /// `ino` is the accounting OWNER — the GLOBAL ino (the routed layer's
    /// pre-`route_ino` identity). The block-reference key law
    /// (`block_refs.rs`: "owner_ino: the referencing inode (GLOBAL ino)")
    /// is load-bearing: resolving with the volume-LOCAL ino keys phantom
    /// records on a hosted slot (`((slot+1) << 40) | local` — the armD
    /// conviction) that no release, no delete-path teardown and no oracle
    /// walk can ever match.
    fn recompute_chained_refs(
        cur: &[u8],
        entries: &[(u32, String)],
        ino: Ino,
        caller: &[super::block_refs::BlockRefOp],
    ) -> Option<Vec<super::block_refs::BlockRefOp>> {
        let x = XattrValue::decode(cur).ok()?;
        let base = crate::layout_wire::decode_base_layout(&x.value).ok()?;
        let head = base.block_map.unwrap_or_default();
        Self::recompute_refs_against_map(&head, entries, ino, caller)
    }

    /// The composed-view refs walk over an EXPLICIT head map — the
    /// blob-aware compose arms (rung 20 residual 1) hand it the FULL
    /// rehydrated map, [`Self::recompute_chained_refs`] the inline head.
    /// Identical in-order composed-view transitions + the caller
    /// `is_map_blob()` verbatim-extend; `None` = no resolver armed (the
    /// caller's ops stand, byte-identical to the pre-rung-19 shape).
    fn recompute_refs_against_map(
        head: &std::collections::HashMap<u32, String>,
        entries: &[(u32, String)],
        ino: Ino,
        caller: &[super::block_refs::BlockRefOp],
    ) -> Option<Vec<super::block_refs::BlockRefOp>> {
        use super::block_refs::BlockRefOp;
        let resolver = super::block_refs::block_ref_resolver()?;
        let mut out: Vec<BlockRefOp> = Vec::with_capacity(entries.len() + 1);
        let resolve = |key: &str, idx: u32, take: bool, out: &mut Vec<BlockRefOp>| {
            match resolver(key, ino, idx) {
                Some(r) => out.push(if take {
                    BlockRefOp::taken(r)
                } else {
                    BlockRefOp::released(r)
                }),
                // The same discipline as the router's `block_ref_for`
                // sites: an unresolvable key is counted, never silent.
                None => {
                    super::META_KV_BLOCK_REFS_UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        // Entries walk IN ORDER over a live view — a batch may name one
        // index twice and the transitions compose left to right.
        let mut view: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
        for (idx, key) in entries {
            let prev = view
                .get(idx)
                .copied()
                .or_else(|| head.get(idx).map(String::as_str));
            match prev {
                Some(p) if p == key => {}
                Some(p) => {
                    resolve(p, *idx, false, &mut out);
                    resolve(key, *idx, true, &mut out);
                }
                None => resolve(key, *idx, true, &mut out),
            }
            view.insert(*idx, key);
        }
        out.extend(caller.iter().filter(|o| o.reference.is_map_blob()).copied());
        Some(out)
    }

    /// **The inline→oversize CROSSING arm** (the 2026-08-19
    /// AlreadyFreezing wedge's TRIGGER — this is the fix's site-(a)/(b)
    /// half; site (c), `PublishService::custody_scoped_layout`, and the
    /// router's save path already spill): the owner-side chain
    /// compaction composed `chained_full = delta.apply(folded)` onto an
    /// INLINE head with **no cap check** — the oversize record passed
    /// commit admission, reached the node overlay, and could only be
    /// refused at FREEZE time ("record value length 66121 exceeds the
    /// per-volume cap 65792"), from which tick the node latched
    /// `AlreadyFreezing` and the volume's checkpoint wedged forever
    /// (journal tail pinned, conveyor degraded, fleet cascade).
    ///
    /// The decision arithmetic is site (c)'s VERBATIM: spill when the
    /// encoded value exceeds this volume's `xattr_value_cap −
    /// LAYOUT_INLINE_HEADROOM` (PR K8's inline ceiling — its 4 KiB
    /// headroom covers the record envelope, so a value that passes here
    /// always freezes).
    ///
    /// `Ok(None)` = fits inline: stage `full` verbatim (the pre-fix
    /// shape, byte-identical). `Ok(Some(..))` = spilled: a fresh CoW
    /// blob was written and FLUSHED (the hook's DUR-6 §3 contract —
    /// the naming commit never points at bytes that can vanish on power
    /// loss); stage the returned re-encode, hold the RES-9 guard until
    /// the commit lands, and the compose is counted. `Err` = a crossing
    /// with no `indirect_map_io` hook armed: the fail-safe retried-class
    /// refusal (the rung-19 "layout delta base unusable" pattern) — the
    /// caller must stage NOTHING.
    async fn spill_oversize_chained_full(
        &self,
        ino: Ino,
        full: &[u8],
    ) -> Result<Option<SpilledChainedFull>> {
        let inline_cap = self
            .xattr_value_cap()
            .saturating_sub(crate::routing::LAYOUT_INLINE_HEADROOM);
        if full.len() <= inline_cap {
            return Ok(None);
        }
        let Some(io) = super::indirect_map::indirect_map_io() else {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "layout delta base unusable: the owner-side chain compaction for ino {ino} \
                 composed a {} B inline layout, past this volume's inline ceiling \
                 {inline_cap} B (xattr value cap {} − {} B layout headroom) — staging it \
                 would wedge the volume's checkpoint at freeze time (the record-value cap), \
                 and no indirect-map hook is armed to spill it (rung 20: refetch and \
                 recompose)",
                full.len(),
                self.xattr_value_cap(),
                crate::routing::LAYOUT_INLINE_HEADROOM,
            )));
        };
        let mut head = crate::layout_wire::decode_base_layout(full).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "chain-compaction crossing: undecodable composed layout for ino {ino}: {e}"
            ))
        })?;
        let map = head.block_map.take().unwrap_or_default();
        let mut sorted: Vec<(u32, String)> = map.iter().map(|(b, k)| (*b, k.clone())).collect();
        sorted.sort_unstable_by_key(|&(b, _)| b);
        // The blob write happens BEFORE any KvTx staging — no node locks
        // are held here; the held 4a I-guard across data I/O follows the
        // conveyor precedent (the rung-20 compose arms' discipline).
        let (fresh_key, guard) = (io.write)(ino, sorted).await?;
        head.block_map_id = Some(format!("indirect:{fresh_key}"));
        let encoded = crate::layout_wire::encode_layout(&head).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "chain-compaction crossing: composed indirect head re-encode failed for \
                 ino {ino}: {e}"
            ))
        })?;
        crate::fuse_client::METRICS
            .publish_blob_composes
            .fetch_add(1, Ordering::Relaxed);
        Ok(Some(SpilledChainedFull {
            encoded,
            head,
            map,
            fresh_key,
            guard,
        }))
    }

    /// The pre-aggregation single-ino body (the
    /// `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1` A/B path, verbatim; `chain`
    /// selects the rung-17 chain-onto-head arm — see
    /// [`Self::merge_layout_and_size_chained`]).
    #[allow(clippy::too_many_arguments)]
    async fn merge_layout_and_size_direct(
        &self,
        ino: Ino,
        refs_owner: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        chain: bool,
    ) -> Result<(bool, u64)> {
        use crate::fuse_client::{publish_phase_record, PublishPhase};
        self.write_gate()?;
        // Publish decomposition (2026-08-01): the meta_commit interior —
        // guard / inode read / slot probe / tx wait — on the delta-save
        // path (100 % of rewrite publish batches per the field ledger).
        let t_guard = std::time::Instant::now();
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        publish_phase_record(PublishPhase::CommitGuard, t_guard);
        let t_iread = std::time::Instant::now();
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        publish_phase_record(PublishPhase::CommitInodeRead, t_iread);
        v.size = size;
        // NEVER author times here — the `set_layout_and_size` clock-
        // authority rule verbatim (generic/003; the unfolded
        // `read_inode_value` keeps parked Δtime refinements pending).
        let t_slot = std::time::Instant::now();
        let tx0 = KvTx::new();
        let (existing, key) = self.xattr_slot(&tx0, ino, "layout").await?;
        let mut tx = tx0;

        // The backend eligibility half: a live, non-JSON base to fold
        // onto (one point lookup under the held I-guard), and the
        // incompat bit durably stamped (the one-time ratchet).
        let mut use_delta = false;
        let mut staged_version = 0u64;
        let mut delta_versions =
            (delta.version != 0).then_some((delta.base_version, delta.version));
        let mut chained_full: Option<Vec<u8>> = None;
        // Rung 19 (the width-N refs composition): the CHAINED arm's
        // recomputed accounting — `None` keeps the caller's ops (the solo
        // path verbatim).
        let mut refs_override: Option<Vec<super::block_refs::BlockRefOp>> = None;
        // Rung 20 residual 1 (the blob-aware compose): RES-9 custody for
        // the fresh CoW blob until the commit lands, and the displaced
        // predecessor's post-commit free.
        let mut fresh_blob_guard: Option<super::indirect_map::IndirectBlobGuard> = None;
        let mut displaced_blob: Option<String> = None;
        // The inline→oversize crossing's fresh blob (2026-08-19 wedge
        // trigger fix): its MAP_BLOB take joins the recomputed refs
        // below. NO released twin and `displaced_blob` stays `None` —
        // the displaced head was INLINE, so nothing is freed.
        let mut crossing_fresh: Option<String> = None;
        if existing {
            if let Some(cur) = self.xattrs.lookup(&key).await? {
                let base_ok = XattrValue::decode(&cur)
                    .map(|x| !x.value.starts_with(b"{"))
                    .unwrap_or(false);
                // DUR-8b: the cap must bound the DURABLE chain. The
                // caller's `layout_delta_chain` is a RAM counter that a
                // metadata-cache refill resets to 0, so before this probe
                // the on-disk chain was bounded only by node compaction —
                // not by `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN`. One extra
                // leaf resolve on the publish path, no record decodes.
                let max_chain = crate::routing::layout_delta_max_chain();
                let (depth, head_versions) = self.xattrs.delta_chain_probe(&key).await?;
                if base_ok && max_chain > 0 && depth < max_chain {
                    use_delta = self.layout_deltas_ready().await;
                }
                // Rung 19: the chained indirect-head gate (the aggregated
                // pass's twin — see there). UNARMED it refuses verbatim
                // (fail-safe, retried-class). An ARMED authority (rung 20
                // residual 1) HAS a data router — the blob rehydrates,
                // the delta applies onto the FULL map, a fresh CoW blob
                // is written (flushed BEFORE the naming commit — DUR-6
                // §3), and the commit stages a full Put naming it, the
                // accounting recomputed against the full head plus the
                // blob custody transfer.
                let head_indirect = chain
                    && self.layout_versions_stamped()
                    && base_ok
                    && XattrValue::decode(&cur).is_ok_and(|x| {
                        matches!(
                            crate::layout_wire::decode_base_layout(&x.value),
                            Err(e) if format!("{e}").contains("indirect")
                        )
                    });
                if head_indirect {
                    let Some(io) = super::indirect_map::indirect_map_io() else {
                        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                            "layout delta base unusable: indirect base — ino {ino}'s durable                          head spilled to an indirect map; a chained delta cannot fold onto                          it and a partial full Put would clobber it (rung 19: refetch and                          recompose)"
                        )));
                    };
                    let x = XattrValue::decode(&cur)?;
                    let head = crate::layout_wire::decode_layout_any(&x.value).map_err(|e| {
                        crate::error::SqueezefsError::InvalidOperation(format!(
                            "rung 20: undecodable indirect head for ino {ino}: {e}"
                        ))
                    })?;
                    // The gate matched on the decode ERROR TEXT; re-derive
                    // the blob key honestly from the head itself.
                    let Some(old_blob) = head
                        .block_map_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("indirect:"))
                        .map(str::to_string)
                    else {
                        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                            "layout delta base unusable: indirect base — ino {ino}'s durable                          head spilled to an indirect map; a chained delta cannot fold onto                          it and a partial full Put would clobber it (rung 19: refetch and                          recompose)"
                        )));
                    };
                    // The blob read/write happens BEFORE any KvTx staging
                    // — no node locks are held here (commit takes leaf
                    // locks internally); the held 4a I-guard across data
                    // I/O follows the conveyor precedent.
                    let full_entries = (io.read)(old_blob.clone()).await?;
                    let full_map: std::collections::HashMap<u32, String> =
                        full_entries.into_iter().collect();
                    // The accounting: the delta's entries against the
                    // FULL head (the map-entry half; the blob custody
                    // transfer joins once the fresh key exists).
                    let mut composed_refs = if self.block_refs.is_some() {
                        Self::recompute_refs_against_map(
                            &full_map,
                            &delta.entries,
                            refs_owner,
                            block_refs,
                        )
                    } else {
                        None
                    };
                    let mut composed = head;
                    composed.block_map = Some(full_map);
                    delta.apply_to(&mut composed);
                    // `apply_to` clobbered `block_map_id` with the
                    // caller's stale id — the fresh key overrides below.
                    let final_map = composed.block_map.take().unwrap_or_default();
                    let mut sorted: Vec<(u32, String)> = final_map.into_iter().collect();
                    sorted.sort_unstable_by_key(|&(b, _)| b);
                    let (new_key, guard) = (io.write)(ino, sorted).await?;
                    composed.block_map_id = Some(format!("indirect:{new_key}"));
                    if let Some(refs) = composed_refs.as_mut() {
                        super::indirect_map::push_map_blob_transfer_op(
                            refs, &old_blob, refs_owner, false,
                        );
                        super::indirect_map::push_map_blob_transfer_op(
                            refs, &new_key, refs_owner, true,
                        );
                    }
                    chained_full =
                        Some(crate::layout_wire::encode_layout(&composed).map_err(|e| {
                            crate::error::SqueezefsError::InvalidOperation(format!(
                                "rung 20: composed indirect head re-encode failed for ino \
                                 {ino}: {e}"
                            ))
                        })?);
                    refs_override = composed_refs;
                    use_delta = false;
                    staged_version = 0;
                    fresh_blob_guard = Some(guard);
                    displaced_blob = Some(old_blob);
                    crate::fuse_client::METRICS
                        .publish_blob_composes
                        .fetch_add(1, Ordering::Relaxed);
                } else if chain && self.layout_versions_stamped() && base_ok {
                    // KD-MW-8 rung 17 (see `merge_layout_and_size_chained`
                    // and the aggregated pass's twin): re-stamp onto the
                    // durable head below the cap; owner-side compaction
                    // at it.
                    let minted = if use_delta {
                        crate::dlm::mint_layout_version()
                    } else {
                        0
                    };
                    if use_delta && minted != 0 {
                        let head_v = head_versions.map(|(_, ver)| ver).unwrap_or(0);
                        delta_versions = Some((head_v, minted));
                        staged_version = minted;
                    } else {
                        use_delta = false;
                        let folded = XattrValue::decode(&cur)?.value.to_vec();
                        let full = delta.apply(&folded).map_err(|e| {
                            crate::error::SqueezefsError::InvalidOperation(format!(
                                "rung 17: owner-side chain compaction failed for ino {ino}: {e}"
                            ))
                        })?;
                        // The inline→oversize CROSSING (2026-08-19 wedge
                        // trigger fix): the composed map may no longer
                        // fit inline — spill (armed) or refuse (unarmed),
                        // NEVER stage the oversize record.
                        match self.spill_oversize_chained_full(ino, &full).await? {
                            None => chained_full = Some(full),
                            Some(spill) => {
                                chained_full = Some(spill.encoded);
                                fresh_blob_guard = Some(spill.guard);
                                crossing_fresh = Some(spill.fresh_key);
                            }
                        }
                    }
                } else if use_delta && self.layout_versions_stamped() {
                    // Spec §6.2 item 9: the durable base-name gate — Stage /
                    // re-base / REFUSE loud (see `admit_versioned_delta`).
                    use_delta =
                        self.admit_versioned_delta(ino, delta_versions, depth, head_versions)?;
                    if use_delta {
                        staged_version = delta.version;
                    }
                }
                // Rung 19 (the width-N refs composition — the aggregated
                // pass's twin): a CHAINED merge's accounting is the
                // entries-onto-the-folded-head transition, never the
                // caller's frame. The compose arm above already owns its
                // accounting (recomputed against the FULL rehydrated
                // head), and this call would clobber it back to `None`
                // (`decode_base_layout` refuses indirect heads).
                if chain && self.block_refs.is_some() && !head_indirect {
                    refs_override =
                        Self::recompute_chained_refs(&cur, &delta.entries, refs_owner, block_refs);
                    // The crossing's blob custody: ONE take for the
                    // fresh blob — no released twin (the displaced head
                    // was INLINE, not a blob).
                    if let Some(fresh) = crossing_fresh.as_deref() {
                        if let Some(refs) = refs_override.as_mut() {
                            super::indirect_map::push_map_blob_transfer_op(
                                refs, fresh, refs_owner, true,
                            );
                        }
                    }
                }
            }
        }
        publish_phase_record(PublishPhase::CommitSlotProbe, t_slot);
        if use_delta {
            // §6.2 item 9 strip seam: only a KV_LAYOUT_VERSIONS volume
            // stores the versioned wire (see `merge_layout_and_size`).
            let wire = if self.layout_versions_stamped() {
                match delta_versions {
                    // The chained restamp: the encode carries the
                    // owner-assigned pair, never the caller's claim.
                    Some((base, ver)) if chain => {
                        let mut d = delta.clone();
                        d.set_versions(base, ver);
                        d.encode()
                    }
                    _ => delta.encode(),
                }
            } else {
                delta.encode_unversioned()
            };
            super::META_KV_LAYOUT_DELTA_BYTES.fetch_add(wire.len() as u64, Ordering::Relaxed);
            super::META_KV_LAYOUT_DELTA_COMMITS.fetch_add(1, Ordering::Relaxed);
            tx.stage_delta_raw(TREE_XATTRS, key, wire);
        } else {
            super::META_KV_LAYOUT_FULL_COMMITS.fetch_add(1, Ordering::Relaxed);
            let full_src: &[u8] = match &chained_full {
                Some(full) => full,
                None => full_layout,
            };
            tx.stage_put(
                TREE_XATTRS,
                key,
                XattrValue::encode_parts(b"layout", full_src)?,
            );
        }
        tx.stage_put(TREE_INODES, inode_key(ino), v.encode());
        // Spec §6.2 item 1: accounting rides THIS tx (no second commit).
        if self.block_refs.is_some() {
            tx.stage_block_refs(refs_override.as_deref().unwrap_or(block_refs));
        }
        tx.hold_guards(guards);
        let t_tx = std::time::Instant::now();
        self.commit_tx(tx).await?;
        publish_phase_record(PublishPhase::CommitTxWait, t_tx);
        // Rung 20 residual 1: the commit named the fresh blob — custody
        // transfers (the RES-9 mint guard stands down; an ERROR path
        // above dropped it armed instead, freeing the fresh blob), and
        // the DISPLACED predecessor is freed only NOW: the commit just
        // stopped naming it, and freeing before the barrier would
        // destroy the still-committed map (the DUR-6 CoW law).
        if let Some(mut guard) = fresh_blob_guard {
            guard.disarm();
        }
        if let Some(old) = displaced_blob {
            if let Some(io) = super::indirect_map::indirect_map_io() {
                (io.free)(old).await;
            }
        }
        Ok((use_delta, staged_version))
    }

    /// Rung 17's covering-version probe: the ino's durable layout-chain
    /// HEAD version (0 = no layout / bare `Put` / unversioned head). A
    /// read-side probe — no guard, no commit; monotone consumers only
    /// (`≥` watermark compares), per the fencing-read law.
    pub async fn layout_head_version(&self, ino: Ino) -> Result<u64> {
        let tx = KvTx::new();
        let (existing, key) = self.xattr_slot(&tx, ino, "layout").await?;
        if !existing {
            return Ok(0);
        }
        let (_depth, head) = self.xattrs.delta_chain_probe(&key).await?;
        Ok(head.map(|(_, v)| v).unwrap_or(0))
    }

    /// Spec §6.2 item 9: whether this volume carries the
    /// `KV_LAYOUT_VERSIONS` incompat bit — the OPEN-time superblock
    /// snapshot, deliberately with NO mount-time ratchet (ruling D9:
    /// nothing stamps the bit in production; the offline
    /// [`super::superblock::set_layout_versions_bit`] verb is the
    /// Phase-8 upgrade path), so version emission can never race the
    /// bit's durability: the bit is on disk strictly before the first
    /// versioned record.
    fn layout_versions_stamped(&self) -> bool {
        self.sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS != 0
    }

    /// Spec §6.2 item 9 — the durable base-name **commit gate** for one
    /// delta admission on a versioned volume (the pure verdict lives in
    /// [`crate::layout_wire::layout_version_gate`]; this is the loud
    /// half). `Ok(true)` = stage the delta; `Ok(false)` = fall back to
    /// the always-correct full `Put` (the convergent re-base — the
    /// refetched-writer, compaction-collapse, and pre-stamp first-touch
    /// shapes); `Err` = a NONZERO base claim that is not the durable
    /// chain head — the "divergent chains fold to divergent layouts"
    /// hazard, refused loud and staged NEVER (a silent full-`Put`
    /// fallback here would let a stale-based writer clobber a head it
    /// never saw).
    ///
    /// The refusal is UNREACHABLE from this daemon's own publish path
    /// by construction: every local layout persist funnels through
    /// `save_metadata_to_backend_ext`, whose republish stamps the RAM
    /// provenance (`CachedMetadata::layout_version`) to exactly the
    /// link it staged (or 0 on a full save), all under
    /// `INODE_META_LOCKS` + this backend's own 4a I-guard. What it
    /// exists to catch is the FOREIGN half: an S8/S9 shipped publish
    /// whose co-writer's notion of the base disagrees with this
    /// authority's durable chain — the error crosses the publish wire
    /// back to the co-writer, which must refetch and recompute rather
    /// than have its stale base silently folded or clobbered in.
    fn admit_versioned_delta(
        &self,
        ino: Ino,
        delta_versions: Option<(u64, u64)>,
        chain_depth: u32,
        head_versions: Option<(u64, u64)>,
    ) -> Result<bool> {
        use crate::layout_wire::LayoutVersionGate;
        match crate::layout_wire::layout_version_gate(delta_versions, chain_depth, head_versions) {
            LayoutVersionGate::Stage => Ok(true),
            LayoutVersionGate::Rebase => Ok(false),
            LayoutVersionGate::Diverged { claimed, head } => {
                Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "layout publish for ino {ino} REFUSED (spec §6.2 item 9): the delta names \
                     base version {claimed:#x} but the durable chain head on meta volume {} is \
                     {head:#x} (depth {chain_depth}) — divergent chains fold to divergent \
                     layouts, so the writer must refetch its base and recompute; nothing was \
                     staged",
                    self.path.display()
                )))
            }
        }
    }

    /// The one-time `KV_LAYOUT_DELTAS` ratchet (KD-14 ordering: the bit
    /// is durable BEFORE the volume's first delta record can be).
    /// `false` = the ratchet could not complete — the caller falls back
    /// to the always-correct full `Put` (never block writes on it).
    async fn layout_deltas_ready(&self) -> bool {
        if self.layout_deltas_ok.load(Ordering::Acquire) {
            return true;
        }
        let _g = self.layout_delta_ratchet.lock().await;
        if self.layout_deltas_ok.load(Ordering::Acquire) {
            return true;
        }
        match super::superblock::set_layout_deltas_bit(&self.path).await {
            Ok(_newly_set) => {
                // Barrier the sector-0 write before any delta entry can
                // become durable (same device — one fdatasync covers it).
                if let Err(e) = self.sync_device().await {
                    log::warn!(
                        "meta volume {}: layout-delta ratchet barrier failed ({e}); \
                         staying on the full-Put path",
                        self.path.display()
                    );
                    return false;
                }
                self.layout_deltas_ok.store(true, Ordering::Release);
                true
            }
            Err(e) => {
                log::warn!(
                    "meta volume {}: could not stamp KV_LAYOUT_DELTAS ({e}); \
                     staying on the full-Put path",
                    self.path.display()
                );
                false
            }
        }
    }
}

impl KvMetaBackend {
    /// The setattr body with the DLM I-guard already held — the routed
    /// layer's entry (it locks through `volume_dlm`, the SAME manager, so
    /// re-locking here would self-deadlock on a stripe).
    ///
    /// **PR M6 (design-metadata-throughput §5.4 D4): the SETATTR-echo
    /// absorb arm.** Under writeback cache the kernel authors regular-file
    /// ctime locally after every rename/unlink/link/setxattr
    /// (`fuse_update_ctime` — those replies carry no attrs, so nothing
    /// can pre-empt the dirtying) and synchronously flushes a times-only
    /// `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)` whose mtime is the
    /// daemon's own round-tripped value, UNCHANGED. That echo measured a
    /// whole 1.000 journal entries/op on rename AND unlink storms (the M2
    /// attribution rig) — the entire G4 gap. It is recognized here by
    /// shape — times-only, mtime absent-or-equal to the folded stored
    /// value — and **parked in the pending-times map with zero journal
    /// entries**: read-visible immediately (every inode read folds the
    /// map, monotone max), durable via the batched drain. Everything else
    /// — changed mtime (the buffered-write flush carrying kernel-authored
    /// write times), explicit utimes (atime present, exact-set), chmod /
    /// chown / truncate — commits exactly as before, folding and retiring
    /// any pending refinement so exact-set semantics never get max-clamped
    /// by a dead echo.
    #[allow(clippy::too_many_arguments)] // the trait's parameter surface
    pub async fn setattr_locked(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<Inode> {
        self.write_gate()?;
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        // Fold the pending refinement into the base: the absorb arm's
        // eligibility compares against the freshest view, and the commit
        // arm's RMW must carry (never clobber) a newer refined ctime.
        self.fold_pending_times(ino, &mut v);

        let times_only =
            mode.is_none() && uid.is_none() && gid.is_none() && size.is_none() && atime.is_none();
        if times_only && mtime.is_none_or(|m| m == v.mtime) {
            if let Some(req_ctime) = ctime {
                super::META_KV_TIMES_ECHO_ABSORBED.fetch_add(1, Ordering::Relaxed);
                // SIGNED compare (i64 ns in the u64 word — fold parity):
                // unsigned read a pre-epoch stored ctime as huge and
                // refused every post-epoch echo forever.
                if (req_ctime as i64) > (v.ctime as i64) {
                    v.ctime = req_ctime;
                    self.park_times_refinement(ino, v.mtime, req_ctime);
                }
                // A refinement at-or-behind the folded view persists
                // nothing (monotonicity: ctime never moves backwards) —
                // still an absorption, still zero entries.
                return Ok(Self::to_inode(ino, &v));
            }
        }

        let mut ctime_updated = false;
        if let Some(m) = mode {
            v.mode = m;
            ctime_updated = true;
        }
        if let Some(u) = uid {
            v.uid = u;
            ctime_updated = true;
        }
        if let Some(g) = gid {
            v.gid = g;
            ctime_updated = true;
        }
        if let Some(s) = size {
            v.size = s;
            ctime_updated = true;
        }
        if let Some(a) = atime {
            v.atime = a;
        }
        if let Some(m) = mtime {
            v.mtime = m;
        }
        if let Some(c) = ctime {
            v.ctime = c;
        } else if ctime_updated {
            // Auto-bump is MONOTONE over the folded view (signed — i64 ns
            // in the u64 word): `now` is the coarse kernel-domain clock
            // and a parked refinement can sit ahead of it inside a tick;
            // an auto-stamp must never regress a ctime a reader already
            // saw (generic/423 inversion class). Explicit sets above stay
            // verbatim — exact-set semantics.
            let now = Self::now_ns();
            if (now as i64) > (v.ctime as i64) {
                v.ctime = now;
            }
        }
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(ino), v.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        // The committed Put carries (or intentionally supersedes) the
        // refinement — retire it so a dead echo never max-folds over an
        // exact-set (under the caller's I-guard: race-free).
        self.retire_pending_times(ino);
        Ok(Self::to_inode(ino, &v))
    }

    /// The setxattr body with the DLM I-guard already held (see
    /// [`Self::setattr_locked`] for the re-entrancy rationale).
    pub async fn setxattr_locked(
        &self,
        ino: Ino,
        name: &str,
        value: &[u8],
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        // The USER value cap (§4.2): the record envelope (1-byte name_len
        // + name ≤ 255) rides in the node layer's separate envelope
        // allowance, so a full `XATTR_SIZE_MAX` value fits regardless of
        // name length (fstests generic/020; pinned in
        // tests/kv_backend_tests.rs::xattr_value_cap_is_the_full_xattr_size_max).
        let cap = self.cache.config().layout.xattr_value_cap();
        if value.len() > cap {
            return Err(KvError::ValueTooLarge {
                len: value.len(),
                cap,
            }
            .into());
        }
        let tx0 = KvTx::new();
        let (_existing, key) = self.xattr_slot(&tx0, ino, name).await?;
        let mut tx = tx0;
        tx.stage_put(
            TREE_XATTRS,
            key,
            // D1.c single-copy staging (no intermediate name/value Vecs).
            XattrValue::encode_parts(name.as_bytes(), value)?,
        );
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// The removexattr body with the DLM I-guard already held.
    pub async fn removexattr_locked(
        &self,
        ino: Ino,
        name: &str,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let tx0 = KvTx::new();
        let (existing, key) = self.xattr_slot(&tx0, ino, name).await?;
        if !existing {
            // ENODATA (Linux ENOATTR): the ATTRIBUTE is absent — never
            // generic NotFound, which the errno map renders ENOENT and
            // misnames the (existing) file (fstests generic/533; pinned
            // in tests/job_fabric_tests.rs).
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::ENODATA),
            ));
        }
        let mut tx = tx0;
        tx.stage_delete(TREE_XATTRS, key);
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// VAL-2 internal writer: `setxattr` **without** the
    /// [`xattr_name_allowed`] screen — the path the daemon's own record
    /// writers take ([`WRITER_CLAIM_XATTR`], `client:{id}`, `layout`,
    /// `system.symlink`). Identical to the [`Metadata::setxattr`] body
    /// minus the screen; never reachable from a client.
    pub async fn setxattr_internal(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.setxattr_locked(ino, name, value, guards).await
    }

    /// VAL-2 internal writer: `removexattr` without the screen (see
    /// [`Self::setxattr_internal`]).
    pub async fn removexattr_internal(&self, ino: Ino, name: &str) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.removexattr_locked(ino, name, guards).await
    }
}

// ---------------------------------------------------------------------------
// DLM S3.5 — the cross-volume transaction applier (design-cow-kv-metadata
// §4.10a; the machinery and its protocol live in
// `crate::meta_backend::crossvol_tx`). This is the ONE place a plan step
// becomes records: the live path and mount recovery call the SAME function,
// so "every step is idempotent" is a property of one code path rather than
// a claim about two.
// ---------------------------------------------------------------------------

impl KvMetaBackend {
    /// Whether this volume barriers inside every commit (§4.6 pt 4). The
    /// cross-volume protocol's explicit ordering barriers are redundant
    /// then, and skipping them keeps a strict mount exactly as fast as it
    /// was.
    pub fn xv_strict_barriers(&self) -> bool {
        self.strict
    }

    /// The routed layer's read of one local inode record — what a
    /// cross-volume plan's count steps take their `(pre, post)` witness
    /// from, under the op's held I-guard.
    pub async fn read_inode_value_routed(&self, local_ino: Ino) -> Result<Option<InodeValue>> {
        Ok(self.read_inode_value(local_ino).await?)
    }

    /// The backend's metadata clock (§ the `now_ns` note above), for plan
    /// builders that must record the timestamp a step will apply.
    pub fn now_ns_pub() -> u64 {
        Self::now_ns()
    }

    /// Stage the intent rider (§4.11): the `Put` rides step 0's
    /// transaction, so intent and first effect are ONE checksummed journal
    /// entry; the `Delete` is the retirement. Both keys derive entirely
    /// from the tx id, so neither needs the collision-chain probe an
    /// ordinary xattr write needs — which is what lets the rider join a
    /// transaction without adding a lock.
    fn stage_intent_rider(tx: &mut KvTx, rider: Option<&XvRider>) -> Result<()> {
        match rider {
            None => {}
            Some(XvRider::Put { tx_id, image }) => tx.stage_put(
                TREE_XATTRS,
                crossvol_tx::intent_key(*tx_id),
                XattrValue::encode_parts(crossvol_tx::intent_name(*tx_id).as_bytes(), image)?,
            ),
            Some(XvRider::Delete { tx_id }) => {
                tx.stage_delete(TREE_XATTRS, crossvol_tx::intent_key(*tx_id))
            }
        }
        Ok(())
    }

    /// Retire an intent: a `Delete` of an EXACT key, so it needs no probe
    /// and cannot disturb another ino-1-class record.
    pub async fn xv_retire_intent(&self, tx_id: u64, guards: Arc<[DlmGuard]>) -> Result<()> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        Self::stage_intent_rider(&mut tx, Some(&XvRider::Delete { tx_id }))?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// This volume's OPEN cross-volume intents as `(tx_id, image)` — a
    /// bounded range scan over the reserved intent ino, empty on a healthy
    /// volume. The mount-recovery driver's only input.
    pub async fn xv_scan_intents(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        let end = xattr_key(crossvol_tx::XV_INTENT_INO, HASH56_MAX, u8::MAX);
        let mut cursor: Vec<u8> = xattr_key(crossvol_tx::XV_INTENT_INO, 0, 0).to_vec();
        let mut out = Vec::new();
        loop {
            let page = self.xattrs.range(&cursor, &end, SCAN_PAGE).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                let (_ino, hash, coll) = decode_xattr_key(k)?;
                let tx_id = (u64::from(coll) << 56) | hash;
                out.push((tx_id, XattrValue::decode(v)?.value));
            }
        }
        Ok(out)
    }

    /// Apply ONE localised plan step in ONE whole-tx entry, optionally
    /// carrying the transaction's intent rider, after checking the step's
    /// witness. Never applies an effect twice, and never overwrites an
    /// object that moved under the plan (`ForeignSkipped`, counted loud by
    /// the caller). The rider is committed even when the witness declines
    /// the effect: an intent that named nothing to do is retired by the
    /// same protocol as one that did.
    pub async fn xv_apply_step(
        &self,
        step: &XvLocalStep,
        rider: Option<&XvRider>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<XvStepOutcome> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        Self::stage_intent_rider(&mut tx, rider)?;
        let mut status = XvStepStatus::Applied;
        let mut inode = None;
        let mut retire_times_for: Option<Ino> = None;

        match step {
            XvLocalStep::RemoveDentry {
                local_parent,
                name,
                expect_child,
                parent_update,
            } => match self.find_dentry_pos(*local_parent, name).await? {
                // Absent ⇒ this step already ran (the only other producer
                // of that state is a foreign removal, which is the same
                // no-op for us).
                None => status = XvStepStatus::AlreadyApplied,
                Some((_, d)) if d.child_ino != *expect_child => {
                    status = XvStepStatus::ForeignSkipped
                }
                Some((dkey, _)) => {
                    tx.stage_delete(TREE_DENTRIES, dkey);
                    self.stage_routed_parent_update(&mut tx, *local_parent, *parent_update, -1)
                        .await?;
                }
            },
            XvLocalStep::InsertDentry {
                local_parent,
                name,
                child,
                ft_bits,
                parent_update,
            } => match self.find_dentry_pos(*local_parent, name).await? {
                Some((_, d)) if d.child_ino == *child => status = XvStepStatus::AlreadyApplied,
                Some(_) => status = XvStepStatus::ForeignSkipped,
                None => {
                    let dkey = self.dentry_insert_key(&tx, *local_parent, name).await?;
                    tx.stage_put(
                        TREE_DENTRIES,
                        dkey,
                        DentryValue::encode_parts(
                            *child,
                            Self::ft_byte(*ft_bits),
                            name.as_bytes(),
                        )?,
                    );
                    self.stage_routed_parent_update(&mut tx, *local_parent, *parent_update, 1)
                        .await?;
                }
            },
            XvLocalStep::SetNlink {
                local_ino,
                pre,
                post,
                ctime,
            } => match self.read_inode_value(*local_ino).await? {
                // A destroyed object cannot be accounted; the routed
                // fragments this replaces were best-effort on a missing
                // inode too (the retired `routed_parent_nlink_delta`
                // fragment this step replaces).
                None => status = XvStepStatus::ForeignSkipped,
                Some(mut v) if v.nlink == *post && *pre != *post => {
                    status = XvStepStatus::AlreadyApplied;
                    // The post-image the caller replies from is still the
                    // stored one (an already-applied step is not an error).
                    if ctime.is_some() {
                        self.fold_pending_times(*local_ino, &mut v);
                    }
                    inode = Some(v);
                }
                Some(mut v) if v.nlink == *pre => {
                    v.nlink = *post;
                    if let Some(ct) = ctime {
                        // Monotone over the FOLDED view (the generic/423
                        // inversion discipline the routed arms established:
                        // the reply is served from this value, so it must
                        // never regress below a served base+refinement).
                        self.fold_pending_times(*local_ino, &mut v);
                        let bump = (*ct).max(Self::now_ns());
                        if (bump as i64) > (v.ctime as i64) {
                            v.ctime = bump;
                        }
                        retire_times_for = Some(*local_ino);
                    }
                    tx.stage_put(TREE_INODES, inode_key(*local_ino), v.encode());
                    inode = Some(v);
                }
                Some(v) => {
                    // Neither the witness nor the post-image: the count
                    // moved under the plan.
                    log::error!(
                        "meta volume {}: cross-volume step wanted ino {local_ino} nlink \
                         {pre} → {post} but found {} — skipping rather than clobbering",
                        self.path.display(),
                        v.nlink
                    );
                    status = XvStepStatus::ForeignSkipped;
                }
            },
            XvLocalStep::TouchCtime { local_ino, ctime } => {
                if self.read_inode_value(*local_ino).await?.is_none() {
                    status = XvStepStatus::ForeignSkipped;
                } else {
                    // Idempotent by construction: a Δctime merge record
                    // whose value only ever moves forward.
                    tx.stage_delta(
                        TREE_INODES,
                        inode_key(*local_ino),
                        &InodeDelta::ctime((*ctime).max(Self::now_ns())),
                    );
                }
            }
            XvLocalStep::MintInode {
                local_ino,
                mode,
                uid,
                gid,
                rdev,
            } => {
                if self.read_inode_value(*local_ino).await?.is_some() {
                    status = XvStepStatus::AlreadyApplied;
                } else {
                    let now = Self::now_ns();
                    let v = InodeValue {
                        mode: *mode,
                        uid: *uid,
                        gid: *gid,
                        nlink: if (*mode & libc::S_IFMT) == libc::S_IFDIR {
                            2
                        } else {
                            1
                        },
                        flags: 0,
                        rdev: *rdev,
                        size: 0,
                        atime: now,
                        mtime: now,
                        ctime: now,
                    };
                    tx.stage_put(TREE_INODES, inode_key(*local_ino), v.encode());
                    inode = Some(v);
                }
            }
        }

        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        if let Some(ino) = retire_times_for {
            // The committed Put carries the folded refinement (the step's
            // I-guard is held by the transaction's caller: race-free).
            self.retire_pending_times(ino);
        }
        Ok(XvStepOutcome { status, inode })
    }

    /// The routed parent update by wire code, with the insert/remove sign
    /// applied to the `Bump` shape — the exact
    /// [`RoutedParentUpdate`] semantics the fragments this applier
    /// replaces used.
    async fn stage_routed_parent_update(
        &self,
        tx: &mut KvTx,
        local_parent: Ino,
        update: RoutedParentUpdate,
        sign: i64,
    ) -> std::result::Result<(), KvError> {
        let now = Self::now_ns();
        match update {
            RoutedParentUpdate::None => Ok(()),
            RoutedParentUpdate::SharedTimes => {
                self.stage_parent_update(tx, local_parent, true, 0, now)
                    .await
            }
            RoutedParentUpdate::ExclusiveTimes => {
                self.stage_parent_update(tx, local_parent, false, 0, now)
                    .await
            }
            RoutedParentUpdate::ExclusiveTimesBump => {
                self.stage_parent_update(tx, local_parent, false, sign, now)
                    .await
            }
        }
    }
}

/// The VAL-2 refusal for a screened name on a mutating path: EPERM,
/// counted on the stats inode's `fuse_reserved_xattr_refusals` (the same
/// gauge the FUSE boundary bumps — a refusal here means something inside
/// the daemon reached a generic entry point with an internal name).
fn screened_xattr_refusal(name: &str) -> crate::error::SqueezefsError {
    crate::fuse_client::METRICS
        .fuse_reserved_xattr_refusals
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log::warn!("refusing xattr op on reserved name {name:?} (VAL-2 allowlist)");
    crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::EPERM))
}

/// The full mutating `Metadata` surface on v3 (PR K6b): every op the v2
/// backend serves, staged as a `KvTx` (records + read-your-own-writes
/// overlay) and committed through the §4.4 pipeline — pre-lock ring
/// admission, ascending-NodeId leaf locks with revalidate/retry, in-lock
/// reservation, out-of-lock entry write, seq-conditional rollback.
///
/// Error shapes and semantics mirror the retired v2 trait impl (the
/// dual-format conformance suite runs the same assertions against both).
#[async_trait::async_trait]
impl Metadata for KvMetaBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        // The read side is live since K6a.
        KvMetaBackend::lookup(self, parent, name).await
    }

    /// Mirrors the retired v2 `create`: exclusive parent, EEXIST check,
    /// setgid inheritance, full parent-time update — one whole-tx journal
    /// entry (inode + dentry + parent).
    async fn create_with_rdev(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    ) -> Result<Inode> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![
            self.dlm.lock_inode_exclusive(parent).await,
            self.dlm.lock_dentry_exclusive(parent, name).await,
        ]);

        if self.find_dentry(parent, name).await?.is_some() {
            return Err(crate::error::SqueezefsError::already_exists(
                "File already exists",
            ));
        }
        let parent_v = self
            .read_inode_value(parent)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {parent} not found")))?;
        let mut final_gid = gid;
        let mut final_mode = mode;
        if (parent_v.mode & libc::S_ISGID) != 0 {
            final_gid = parent_v.gid;
            if (mode & libc::S_IFMT) == libc::S_IFDIR {
                final_mode |= libc::S_ISGID;
            }
        }

        let ino = self.allocate_ino();
        let now = Self::now_ns();
        let child = InodeValue {
            mode: final_mode,
            uid,
            gid: final_gid,
            nlink: 1,
            flags: 0,
            rdev,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
        };
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(ino), child.encode());
        let dkey = self.dentry_insert_key(&tx, parent, name).await?;
        tx.stage_put(
            TREE_DENTRIES,
            dkey,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(ino, Self::ft_byte(final_mode), name.as_bytes())?,
        );
        self.stage_parent_update(&mut tx, parent, false, 0, now)
            .await?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(Self::to_inode(ino, &child))
    }

    /// Mirrors the retired v2 `unlink`: two-phase child discovery with
    /// revalidation, shared parent for regular files (Δtime merge record,
    /// §4.4 pt 6), exclusive for directories/self-references — one
    /// whole-tx entry.
    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        self.write_gate()?;
        loop {
            let phase1 = self
                .dlm
                .lock_many(
                    &[(parent, LockMode::Shared)],
                    &[(parent, name, LockMode::Exclusive)],
                )
                .await;
            let Some((dkey, dentry)) = self.find_dentry_pos(parent, name).await? else {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Dentry not found",
                )));
            };
            let ino = dentry.child_ino;
            let is_dir = u32::from(dentry.file_type) << 12 == libc::S_IFDIR;
            let parent_shared = !is_dir && parent != ino;
            drop(phase1);

            let inode_set: &[(u64, LockMode)] = if parent == ino {
                &[(parent, LockMode::Exclusive)]
            } else if parent_shared {
                &[(parent, LockMode::Shared), (ino, LockMode::Exclusive)]
            } else {
                &[(parent, LockMode::Exclusive), (ino, LockMode::Exclusive)]
            };
            let full: Arc<[DlmGuard]> = Arc::from(
                self.dlm
                    .lock_many(inode_set, &[(parent, name, LockMode::Exclusive)])
                    .await,
            );
            match self.find_dentry_pos(parent, name).await? {
                Some((cur_key, cur)) if cur.child_ino == ino && cur_key == dkey => {}
                _ => continue, // dentry changed under us — rediscover
            }

            let now = Self::now_ns();
            let mut tx = KvTx::new();
            tx.stage_delete(TREE_DENTRIES, dkey);
            self.stage_parent_update(&mut tx, parent, parent_shared, 0, now)
                .await?;
            let mut child = self
                .read_inode_value(ino)
                .await?
                .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
            if child.nlink > 0 {
                child.nlink -= 1;
            }
            child.ctime = now;
            tx.stage_put(TREE_INODES, inode_key(ino), child.encode());
            tx.hold_guards(full);
            self.commit_tx(tx).await?;
            return Ok(ino);
        }
    }

    /// Mirrors the retired v2 `link`: whole set locked upfront, EEXIST
    /// check, nlink+1 + ctime, parent-time full update — one entry.
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(
            self.dlm
                .lock_many(
                    &[
                        (new_parent, LockMode::Exclusive),
                        (ino, LockMode::Exclusive),
                    ],
                    &[(new_parent, new_name, LockMode::Exclusive)],
                )
                .await,
        );

        if self.find_dentry(new_parent, new_name).await?.is_some() {
            return Err(crate::error::SqueezefsError::already_exists(
                "File already exists",
            ));
        }
        let mut child = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        // Fold the parked times refinement into the base so the ctime
        // bump below is MONOTONE against every already-served view (a
        // getattr served base+refinement moments ago; stamping `now`
        // over the unfolded base could regress below it — the
        // generic/423 inversion class), and so the committed Put CARRIES
        // the refinement (retired below, the setattr commit-arm
        // discipline).
        self.fold_pending_times(ino, &mut child);
        child.nlink += 1;
        let now = Self::now_ns();
        // Signed monotone bump (times are i64 ns in the u64 word).
        if (now as i64) > (child.ctime as i64) {
            child.ctime = now;
        }

        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(ino), child.encode());
        let dkey = self.dentry_insert_key(&tx, new_parent, new_name).await?;
        tx.stage_put(
            TREE_DENTRIES,
            dkey,
            // D1.c single-copy staging (no intermediate name Vec).
            DentryValue::encode_parts(ino, Self::ft_byte(child.mode), new_name.as_bytes())?,
        );
        self.stage_parent_update(&mut tx, new_parent, false, 0, now)
            .await?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        // The committed Put carries the folded refinement (under the
        // I-guard: race-free) — same discipline as the setattr commit arm.
        self.retire_pending_times(ino);
        Ok(Self::to_inode(ino, &child))
    }

    /// Mirrors the retired v2 `rename`: pure dentry surgery (EXCHANGE
    /// swap / NOREPLACE guard / replace) — one whole-tx entry, so a
    /// rename can never be half-visible after a crash (§4.10).
    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        self.write_gate()?;
        if flags & (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
            == (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
        {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        // WHITEOUT|EXCHANGE is VFS-forbidden (see the routed impl).
        if flags & libc::RENAME_WHITEOUT != 0 && flags & libc::RENAME_EXCHANGE != 0 {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        let guards: Arc<[DlmGuard]> = Arc::from(
            self.dlm
                .lock_many(
                    &[
                        (old_parent, LockMode::Exclusive),
                        (new_parent, LockMode::Exclusive),
                    ],
                    &[
                        (old_parent, old_name, LockMode::Exclusive),
                        (new_parent, new_name, LockMode::Exclusive),
                    ],
                )
                .await,
        );

        let old_pos = self.find_dentry_pos(old_parent, old_name).await?;
        let new_pos = self.find_dentry_pos(new_parent, new_name).await?;

        let mut tx = KvTx::new();
        if flags & libc::RENAME_EXCHANGE != 0 {
            let enoent = || {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            };
            let (old_key, old_d) = old_pos.ok_or_else(enoent)?;
            let (new_key, new_d) = new_pos.ok_or_else(enoent)?;
            tx.stage_delete(TREE_DENTRIES, old_key);
            tx.stage_delete(TREE_DENTRIES, new_key);
            let nk = self.dentry_insert_key(&tx, new_parent, new_name).await?;
            tx.stage_put(
                TREE_DENTRIES,
                nk,
                // D1.c single-copy staging (no intermediate name Vec).
                DentryValue::encode_parts(old_d.child_ino, old_d.file_type, new_name.as_bytes())?,
            );
            let ok = self.dentry_insert_key(&tx, old_parent, old_name).await?;
            tx.stage_put(
                TREE_DENTRIES,
                ok,
                // D1.c single-copy staging (no intermediate name Vec).
                DentryValue::encode_parts(new_d.child_ino, new_d.file_type, old_name.as_bytes())?,
            );
        } else {
            if flags & libc::RENAME_NOREPLACE != 0 && new_pos.is_some() {
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::EEXIST),
                ));
            }
            let Some((old_key, old_d)) = old_pos else {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Source dentry not found",
                )));
            };
            if let Some((new_key, _)) = new_pos {
                tx.stage_delete(TREE_DENTRIES, new_key);
            }
            tx.stage_delete(TREE_DENTRIES, old_key);
            let nk = self.dentry_insert_key(&tx, new_parent, new_name).await?;
            tx.stage_put(
                TREE_DENTRIES,
                nk,
                // D1.c single-copy staging (no intermediate name Vec).
                DentryValue::encode_parts(old_d.child_ino, old_d.file_type, new_name.as_bytes())?,
            );
            // RENAME_WHITEOUT: the char-0:0 whiteout at the OLD name in
            // the same whole-tx entry (the routed impl's contract).
            if flags & libc::RENAME_WHITEOUT != 0 {
                let w_ino = self.allocate_ino();
                let now = Self::now_ns();
                let wv = InodeValue {
                    mode: libc::S_IFCHR,
                    uid: 0,
                    gid: 0,
                    nlink: 1,
                    flags: 0,
                    rdev: 0,
                    size: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                };
                tx.stage_put(TREE_INODES, inode_key(w_ino), wv.encode());
                let wk = self.dentry_insert_key(&tx, old_parent, old_name).await?;
                tx.stage_put(
                    TREE_DENTRIES,
                    wk,
                    DentryValue::encode_parts(
                        w_ino,
                        Self::ft_byte(libc::S_IFCHR),
                        old_name.as_bytes(),
                    )?,
                );
            }
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        KvMetaBackend::readdir(self, dir, offset, max).await
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        KvMetaBackend::getattr(self, ino).await
    }

    /// Mirrors the retired v2 `setattr` field-for-field, including the
    /// ctime auto-bump rule.
    #[allow(clippy::too_many_arguments)] // the trait's signature
    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.setattr_locked(ino, mode, uid, gid, size, atime, mtime, ctime, guards)
            .await
    }

    /// VAL-2: internal records read as ABSENT through the generic entry
    /// point (the FUSE boundary renders this ENODATA) — a name filtered
    /// out of [`Metadata::listxattr`] must not have its existence
    /// confirmed by a get. Daemon reads use the inherent
    /// [`KvMetaBackend::getxattr`].
    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        if !xattr_name_allowed(name) {
            return Ok(None);
        }
        KvMetaBackend::getxattr(self, ino, name).await
    }

    /// Set/overwrite one xattr (§4.2: unlimited count, value ≤ the
    /// per-volume cap `min(65,536, node_size/4)` — the capability lift
    /// over v2's 3 × 8 KiB blocks).
    ///
    /// VAL-2: screened by [`xattr_name_allowed`] — the generic entry
    /// point never writes an internal record. The daemon's own record
    /// writers use [`KvMetaBackend::setxattr_internal`].
    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        if !xattr_name_allowed(name) {
            return Err(screened_xattr_refusal(name));
        }
        self.setxattr_internal(ino, name, value).await
    }

    /// Remove one xattr; absent names fail loud with the v2 NotFound
    /// shape. VAL-2-screened (see [`Metadata::setxattr`]).
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        if !xattr_name_allowed(name) {
            return Err(screened_xattr_refusal(name));
        }
        self.removexattr_internal(ino, name).await
    }

    /// VAL-2: internal records are filtered out of the generic listing
    /// (the daemon's own listing is the inherent
    /// [`KvMetaBackend::listxattr`], which sees everything).
    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let mut names = KvMetaBackend::listxattr(self, ino).await?;
        names.retain(|n| xattr_name_allowed(n));
        Ok(names)
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        // Single destroy == a size-1 batch: one code path (the v2 shape).
        self.destroy_inodes(std::slice::from_ref(&ino)).await
    }
}
