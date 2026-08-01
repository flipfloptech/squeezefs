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
use crate::meta_backend::dlm::{DlmGuard, DlmLockManager, LockMode};
use crate::meta_backend::sync_coalescer::SyncCoalescer;
use crate::meta_backend::{DirEntry, Ino, Inode, Metadata};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

/// `SQUEEZEFS_META_NODE_CACHE_MB` (§5.1; default 512 — §4.5).
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
static TEST_CONVEYOR_HOLD_NOTIFY: once_cell::sync::Lazy<tokio::sync::Notify> =
    once_cell::sync::Lazy::new(tokio::sync::Notify::new);

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
const DEFAULT_COMMIT_BATCH_TXS: usize = 64;
const DEFAULT_COMMIT_BATCH_BYTES: u64 = 256 * 1024;

fn commit_batch_txs_env() -> usize {
    std::env::var(COMMIT_BATCH_TXS_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_COMMIT_BATCH_TXS)
}

fn commit_batch_bytes_env() -> u64 {
    std::env::var(COMMIT_BATCH_BYTES_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_COMMIT_BATCH_BYTES)
}

/// PR M6: pending-times drain batch — inos per DLM `lock_many` set / per
/// drain transaction (the `destroy_inodes` batching shape).
const PENDING_TIMES_DRAIN_BATCH: usize = 128;

/// PR M6: pending-times population that wakes the drain task ahead of its
/// cadence tick (bounds the map and the crash-loss window by count, not
/// just time).
const PENDING_TIMES_DRAIN_CAP: u64 = 512;

/// The single-writer mount guard's claim record: an xattr on ino 1 beside
/// the `client:{id}` registrations (design-metadata-throughput §5.0 B2).
/// JSON `{"id","ts","pid","boot"}`; staleness follows the ONE staleness
/// law ([`crate::fuse_client::CLIENT_STALE_TTL_SECS`]).
pub const WRITER_CLAIM_XATTR: &str = "writer_claim";

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
}

impl WriterClaim {
    /// Encode as the compact JSON the record stores.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::json!({
            "id": self.id,
            "ts": self.ts,
            "pid": self.pid,
            "boot": self.boot,
        })
        .to_string()
        .into_bytes()
    }

    /// Decode a stored claim. `None` for unparseable values — callers
    /// treat those as a stale *foreign* claim (never auto-taken: a value
    /// we cannot attribute cannot prove anything).
    pub fn decode(val: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(val).ok()?;
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            ts: v.get("ts")?.as_u64()?,
            pid: v.get("pid")?.as_u64()? as u32,
            boot: v.get("boot")?.as_str()?.to_string(),
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
            if *tree_id == TREE_ALLOC_RESERVED {
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
    alloc: Arc<ExtentAllocator>,
    /// §4.8 monotonic watermark, recovered at mount; the create path
    /// `fetch_add`s it.
    next_ino: AtomicU64,
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
    /// The §4.11 unknown-`features_ro` write gate (K6a hand-off): reads
    /// serve, every mutation is withheld.
    read_only: bool,
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
    layout_delta_ratchet: tokio::sync::Mutex<()>,
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
    /// `SQUEEZEFS_META_COMMIT_BATCH_TXS` (default 64), read at open.
    batch_max_txs: usize,
    /// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` (default 256 KiB) clamped to
    /// the ring's user-admissible capacity, so one Σ-admission can
    /// always eventually succeed (a batch larger than the admissible
    /// ring would park forever — the liveness clamp).
    batch_max_bytes: u64,
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
    pub(super) smo: tokio::sync::Mutex<SmoContext>,
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
    /// Ledger records written but not yet known durable, by their
    /// `journal_tail_seq` — the §4.6 pt 3 pending-reclaim watermark and,
    /// since the Option-A coverage fix, the pending-free gate's clock too
    /// (design-smo-replay-currency §2-A); drained after the next barrier.
    pub(super) pending_reclaim: std::sync::Mutex<Vec<u64>>,
    /// Checkpoint-task lifecycle: shutdown flag + wake + join handle +
    /// liveness probe (`Weak<()>` of the token the task owns).
    shutting_down: AtomicBool,
    ckpt_wake: Arc<tokio::sync::Notify>,
    ckpt_join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    times_drain_wake: Arc<tokio::sync::Notify>,
    times_drain_join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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

/// Resolve the node-cache budget knob (bytes).
fn node_cache_budget_bytes() -> u64 {
    std::env::var(NODE_CACHE_MB_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(super::node_cache::DEFAULT_CACHE_BUDGET_BYTES)
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

        // (2) Bootstrap replay (sets `boot_id` — shared with probes).
        let mut inner = Self::open_inner(path).await?;
        *inner.guard_fd.get_mut().unwrap() = Some(guard_fd);
        inner.writer_id = uuid::Uuid::new_v4().to_string();
        // Layer B1 resolution: test override first, then the real RESCAP
        // probe (control-plane ioctl — off the async runtime).
        let probe_path = path.to_path_buf();
        inner.reservations = tokio::task::spawn_blocking(move || {
            crate::meta_backend::reservation::resolve_for_mount(&probe_path)
        })
        .await
        .ok()
        .flatten();
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
                if level > 0 && matches!(tree_id, TREE_INODES | TREE_DENTRIES | TREE_XATTRS) {
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
                _ => unreachable!("phase 1 collects only the three mounted trees"),
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
                    TREE_ALLOC_RESERVED => continue,
                    _ => continue,
                };
                if tree_id == TREE_INODES {
                    if let Ok(ino) = decode_inode_key(&rec.key) {
                        match crate::meta_backend::split_guest_local(ino) {
                            Some((slot, raw)) => {
                                let e = max_replayed_guest.entry(slot).or_insert(0);
                                *e = (*e).max(raw);
                            }
                            None => max_replayed_ino = max_replayed_ino.max(ino),
                        }
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
            commit_batch_bytes_env().min(user_capacity.max(1))
        };
        let strict = crate::meta_backend::resolve_flush_interval_ms() == 0;
        let read_only = sb.unknown_ro() != 0;
        let layout_deltas_stamped =
            sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_LAYOUT_DELTAS != 0;
        let sync = Arc::new(SyncCoalescer::new());
        let retire_seq = Arc::new(AtomicU64::new(ledger.seq + 1));
        let smo = tokio::sync::Mutex::new(SmoContext::with_journal(
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
            alloc,
            next_ino: AtomicU64::new(next_ino),
            replay,
            ring,
            dlm: DlmLockManager::new(),
            sync,
            cache,
            strict,
            needs_flush: AtomicBool::new(false),
            read_only,
            failed: AtomicBool::new(false),
            journal_failures: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            layout_deltas_ok: AtomicBool::new(layout_deltas_stamped),
            layout_delta_ratchet: tokio::sync::Mutex::new(()),
            conveyor: Arc::new(ConveyorCore::new()),
            conveyor_self: std::sync::OnceLock::new(),
            batch_max_txs: commit_batch_txs_env(),
            batch_max_bytes,
            timeout_threshold: squeezefs_timeout_env(),
            smo,
            retire_seq,
            pending_reclaim: std::sync::Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
            ckpt_wake: Arc::new(tokio::sync::Notify::new()),
            ckpt_join: std::sync::Mutex::new(None),
            ckpt_alive: std::sync::Mutex::new(Weak::new()),
            pending_times: scc::HashMap::new(),
            pending_times_count: AtomicU64::new(0),
            times_drain_wake: Arc::new(tokio::sync::Notify::new()),
            times_drain_join: std::sync::Mutex::new(None),
            atomicity_physical: std::sync::OnceLock::new(),
            guard_fd: std::sync::Mutex::new(None),
            writer_id: String::new(),
            // This boot's id — needed by write mounts (the claim gate's
            // same-host dead-pid proof) AND probes (`mount_registrations`
            // classifies claim records with the same proof).
            boot_id: read_boot_id(),
            claimed: AtomicBool::new(false),
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
        Ok(be)
    }

    /// The mounted superblock.
    pub fn superblock(&self) -> &SuperblockV3 {
        &self.sb
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
            Some(st) if st.slots_hosted.contains(&0) && st.resolved_native_slot() != Some(0) => {
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
    pub fn next_ino(&self) -> u64 {
        self.next_ino.load(Ordering::Acquire)
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
    pub fn trees(&self) -> [&KvTree; 3] {
        [&self.inodes, &self.dentries, &self.xattrs]
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
                let out = crate::uring_fs::fdatasync(self.path.clone()).await;
                match &out {
                    Ok(()) => self.note_barrier_success(),
                    Err(e) => self.note_barrier_failure(e),
                }
                out
            })
            .await?;
        self.after_durable_barrier();
        Ok(())
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
        let drained: Vec<u64> = {
            let mut g = self.pending_reclaim.lock().unwrap();
            std::mem::take(&mut *g)
        };
        for tail in drained {
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
            if self.write_gate().is_err() {
                // Failing / shutting-down volume: refinements are µs-grade
                // time polish — never worth failing a barrier path over.
                return Ok(total);
            }
            // Snapshot up to a batch of inos (scan stops at the cap).
            let mut batch: Vec<Ino> = Vec::new();
            self.pending_times.iter_sync(|k, _| {
                batch.push(*k);
                batch.len() < PENDING_TIMES_DRAIN_BATCH
            });
            if batch.is_empty() {
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
            if let Err(e) = Metadata::removexattr(self, 1, WRITER_CLAIM_XATTR).await {
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
        let drain_handle = self.times_drain_join.lock().unwrap().take();
        if let Some(handle) = drain_handle {
            handle
                .await
                .map_err(|e| KvError::Corrupt(format!("pending-times drain task panicked: {e}")))?;
        }
        let handle = self.ckpt_join.lock().unwrap().take();
        if let Some(handle) = handle {
            // The task observes the flag, runs the final checkpoint, and
            // exits; joining it IS the drain.
            handle.await.map_err(|e| {
                KvError::Corrupt(format!("checkpoint task panicked during shutdown: {e}"))
            })?;
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
        handle: tokio::task::JoinHandle<()>,
        alive: Weak<()>,
    ) {
        *self.ckpt_join.lock().unwrap() = Some(handle);
        *self.ckpt_alive.lock().unwrap() = alive;
    }

    /// PR M6: the pending-times drain task's wake (cap crossings + the
    /// shutdown broadcast).
    pub(super) fn times_drain_wake_handle(&self) -> Arc<tokio::sync::Notify> {
        self.times_drain_wake.clone()
    }

    pub(super) fn install_times_drain_task(&self, handle: tokio::task::JoinHandle<()>) {
        *self.times_drain_join.lock().unwrap() = Some(handle);
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

    pub(super) fn checkpoint_wake(&self) -> Arc<tokio::sync::Notify> {
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
fn read_boot_id() -> String {
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
fn pid_provably_dead(pid: u32) -> bool {
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
        if claim.pid != std::process::id() || claim.boot != read_boot_id() {
            return None; // foreign holder: refuse instantly (unchanged posture)
        }
        let deadline = std::time::Instant::now() + TEARDOWN_FLOCK_WAIT;
        loop {
            match Self::acquire_writer_flock(path) {
                Ok(fd) => return Some(fd),
                Err(FlockOutcome::Held) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(POLL).await;
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
    fn classify_claim(&self, raw: Option<Vec<u8>>, now: u64) -> ClaimEvidence {
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
            ClaimEvidence::FreshForeign(claim)
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
        let evidence = self.classify_claim(raw, now);

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

        // Layer B2: commit our claim and make it durable — the volume's
        // first post-replay mutation, BEFORE the checkpoint task exists.
        let claim = WriterClaim {
            id: self.writer_id.clone(),
            ts: unix_now_secs(),
            pid: std::process::id(),
            boot: self.boot_id.clone(),
        };
        Metadata::setxattr(&**self, 1, WRITER_CLAIM_XATTR, &claim.encode()).await?;
        self.claimed.store(true, Ordering::Release);
        self.trace_guard_event("claim_committed");
        self.sync_device().await.map_err(KvError::Io)?;
        self.trace_guard_event("claim_barriered");
        log::info!(
            "meta volume {}: writer claim taken (id={}, mode={})",
            self.path.display(),
            claim.id,
            self.writer_guard_mode()
        );
        Ok(())
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
    /// `"flock+claim"` (detection-grade cross-host), `"flock"` (read-only
    /// mount: no claim is written), or `"unguarded"` (probe backends —
    /// never mounted, never in stats).
    pub fn writer_guard_mode(&self) -> &'static str {
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
            if stamp.slots_hosted.contains(&0) && stamp.resolved_native_slot() != Some(0) {
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
            };
            if let Err(e) = Metadata::setxattr(self, 1, WRITER_CLAIM_XATTR, &claim.encode()).await {
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
        Metadata::removexattr(&*be, 1, WRITER_CLAIM_XATTR).await?;
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
    tokio::task::spawn_blocking(move || f(rsv.as_ref()))
        .await
        .map_err(|e| std::io::Error::other(format!("reservation task join: {e}")))?
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
    done: tokio::sync::oneshot::Sender<std::result::Result<(), KvError>>,
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
            _ => unreachable!("kv commits stage only the three §4.2 trees"),
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
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "meta volume {} carries unknown read-only feature bits {:#x}: mounted \
                 read-only (§4.11); mutations withheld",
                self.path.display(),
                self.sb.unknown_ro()
            )));
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
        let (done, rx) = tokio::sync::oneshot::channel();
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
            tokio::spawn(Self::conveyor_pass_task(conveyor, weak));
        }

        // (3) Park on the fan-out. A closed channel means the pass died
        // between drain and fan-out — the panic sentinel already failed
        // the batch loud (EIO here is the belt, not the mechanism).
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
                tokio::time::sleep(std::time::Duration::from_millis(stall)).await;
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
            let _ = tokio::time::timeout(until_crossing, notified).await;
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
                Some(v) if v.nlink > 0 => {
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
        // PR VL5b: the routed layer pre-allocates BOTH — the effective
        // local key ino (native watermark or guest-namespaced cursor
        // mint) and its global encoding (which rides the mint slot, not
        // this volume's index) — so this backend stays keyspace-agnostic.
        local_ino: Ino,
        global_ino: Ino,
        guards: Arc<[DlmGuard]>,
    ) -> Result<Inode> {
        self.write_gate()?;
        if self.find_dentry(local_parent, name).await?.is_some() {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
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
        let now = Self::now_ns();
        let child = InodeValue {
            mode: final_mode,
            uid,
            gid: final_gid,
            nlink: if is_dir { 2 } else { 1 },
            flags: 0,
            rdev,
            size: 0,
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
        guards: Arc<[DlmGuard]>,
    ) -> Result<InodeValue> {
        self.write_gate()?;
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let now = Self::now_ns();
        let v = InodeValue {
            mode,
            uid,
            gid,
            nlink: if is_dir { 2 } else { 1 },
            flags: 0,
            rdev,
            size: 0,
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

    /// Parent-side dentry removal (cross-volume unlink / rename source):
    /// dentry Delete + the routed parent update.
    pub async fn routed_remove_dentry(
        &self,
        local_parent: Ino,
        name: &str,
        parent_update: RoutedParentUpdate,
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
                self.stage_parent_update(&mut tx, local_parent, false, -1, now)
                    .await?
            }
        }
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
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "Too many links".to_string(),
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
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "Too many links".to_string(),
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

    /// Parent-side directory nlink delta (routed rename of a directory
    /// across parents), best-effort like the v2 arm (`if let Ok`) —
    /// times untouched, exactly the v2 rename's nlink shift.
    pub async fn routed_parent_nlink_delta(
        &self,
        local_parent: Ino,
        delta: i64,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let Some(mut pv) = self.read_inode_value(local_parent).await? else {
            return Ok(());
        };
        match delta.cmp(&0) {
            std::cmp::Ordering::Greater => pv.nlink += delta as u32,
            std::cmp::Ordering::Less => {
                if pv.nlink > 2 {
                    pv.nlink -= (-delta) as u32;
                }
            }
            std::cmp::Ordering::Equal => return Ok(()),
        }
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_parent), pv.encode());
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
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
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
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
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: &[u8],
        size: u64,
    ) -> Result<bool> {
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
        if existing {
            if let Some(cur) = self.xattrs.lookup(&key).await? {
                let base_ok = XattrValue::decode(&cur)
                    .map(|x| !x.value.starts_with(b"{"))
                    .unwrap_or(false);
                if base_ok {
                    use_delta = self.layout_deltas_ready().await;
                }
            }
        }
        publish_phase_record(PublishPhase::CommitSlotProbe, t_slot);
        if use_delta {
            let wire = delta.encode();
            super::META_KV_LAYOUT_DELTA_BYTES.fetch_add(wire.len() as u64, Ordering::Relaxed);
            super::META_KV_LAYOUT_DELTA_COMMITS.fetch_add(1, Ordering::Relaxed);
            tx.stage_delta_raw(TREE_XATTRS, key, wire);
        } else {
            super::META_KV_LAYOUT_FULL_COMMITS.fetch_add(1, Ordering::Relaxed);
            tx.stage_put(
                TREE_XATTRS,
                key,
                XattrValue::encode_parts(b"layout", full_layout)?,
            );
        }
        tx.stage_put(TREE_INODES, inode_key(ino), v.encode());
        tx.hold_guards(guards);
        let t_tx = std::time::Instant::now();
        self.commit_tx(tx).await?;
        publish_phase_record(PublishPhase::CommitTxWait, t_tx);
        Ok(use_delta)
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
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
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
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
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

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        KvMetaBackend::getxattr(self, ino, name).await
    }

    /// Set/overwrite one xattr (§4.2: unlimited count, value ≤ the
    /// per-volume cap `min(65,536, node_size/4)` — the capability lift
    /// over v2's 3 × 8 KiB blocks).
    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.setxattr_locked(ino, name, value, guards).await
    }

    /// Remove one xattr; absent names fail loud with the v2 NotFound
    /// shape.
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.removexattr_locked(ino, name, guards).await
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        KvMetaBackend::listxattr(self, ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        // Single destroy == a size-1 batch: one code path (the v2 shape).
        self.destroy_inodes(std::slice::from_ref(&ino)).await
    }
}
