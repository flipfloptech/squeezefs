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
use super::journal::{checkpoint_reserve_bytes, entry_len_for, untag, JournalRing};
use super::journal_core::AdmissionClass;
use super::node::{key_successor, NodeLayout};
use super::node_cache::{
    CachedNode, LiveLookup, NodeCache, NodeCacheConfig, OwnedRec, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use super::record::{
    decode_dentry_key, decode_inode_key, decode_readdir_cookie, dentry_key, dentry_name_hash54,
    encode_readdir_cookie, first_free_coll_seq, inode_key, xattr_key, xattr_name_hash56,
    DentryValue, InodeDelta, InodeValue, ReaddirPos, Record, RecordKind, XattrValue, HASH54_MAX,
    HASH56_MAX, TREE_ALLOC_RESERVED, TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
};
use super::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use super::tree::{decode_interior_value, KvTree, RootPtr, SmoContext, SmoJournal};
use super::KvError;
use crate::error::Result;
use crate::meta_backend::atomicity::META_VOLUME_ATOMICITY_COW;
use crate::meta_backend::dlm::{DlmLockManager, LockMode};
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

/// One mounted v3 metadata volume.
pub struct KvMetaBackend {
    path: PathBuf,
    sb: SuperblockV3,
    ledger: LedgerRecord,
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
    /// §4.7/§4.6 retire tag for SMO frees: always `checkpoint_seq + 1`
    /// (the NEXT ledger record); shared with the SMO hooks.
    pub(super) retire_seq: Arc<AtomicU64>,
    /// Ledger records written but not yet known durable:
    /// `(ledger_seq, tail)` — the §4.6 pt 3 pending-reclaim watermark;
    /// drained after the next barrier.
    pub(super) pending_reclaim: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Checkpoint-task lifecycle: shutdown flag + wake + join handle +
    /// liveness probe (`Weak<()>` of the token the task owns).
    shutting_down: AtomicBool,
    ckpt_wake: Arc<tokio::sync::Notify>,
    ckpt_join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    ckpt_alive: std::sync::Mutex<Weak<()>>,
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
                return Err(KvError::Busy(format!(
                    "{}: another squeezefs process holds the writer lock{} — concurrent \
                     mounts of one metadata volume are refused (single-writer guard)",
                    path.display(),
                    holder_suffix(&holder),
                )));
            }
            Err(FlockOutcome::Io(e)) => {
                return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
            }
        };

        // (2) Bootstrap replay.
        let mut inner = Self::open_inner(path).await?;
        *inner.guard_fd.get_mut().unwrap() = Some(guard_fd);
        inner.writer_id = uuid::Uuid::new_v4().to_string();
        inner.boot_id = read_boot_id();
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
        be.trace_guard_event("flock_acquired");

        // (3)+(4)+(5) The claim gate: decision, PR acquisition, claim
        // commit + barrier. Any refusal drops the Arc — flock releases,
        // nothing was spawned, nothing was written (PR registrations are
        // rolled back best-effort inside the gate).
        if let Err(e) = be.writer_guard_gate().await {
            be.release_reservation().await;
            return Err(e);
        }

        super::checkpoint::spawn_checkpoint_task(&be);
        Ok(be)
    }

    /// Open for a **read-only probe** (the format-preflight guard and
    /// volume-status reads): the full mount bootstrap — SB → ledger →
    /// bitmap → RAM journal replay — but NO checkpoint/writeback task is
    /// spawned, so nothing is ever written. Probing a volume another
    /// process has live-mounted therefore cannot corrupt it. Dropping the
    /// returned backend releases everything (there is no task to join).
    pub async fn open_probe(path: &Path) -> std::result::Result<Arc<Self>, KvError> {
        Ok(Arc::new(Self::open_inner(path).await?))
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
        let alloc = Arc::new(
            ExtentAllocator::load(
                path,
                sb.alloc_bitmap.start,
                total_extents,
                compaction_reserve_extents(total_extents),
                PENDING_FREE_CAP,
                ledger.seq,
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

        // 5b. Read-only replay into the cache: original seqs, per-key LWW
        // (§4.2 replay fold); allocator records were already consumed by
        // the K4 load. Level-tagged interior-pointer records (the K6b SMO
        // journaling, §4.6) route to their interior node — "replay applies
        // the pointer record first"; an unroutable pointer (the mounted
        // ledger predates a root growth) drops sound-and-silent (the
        // window never fails a mount loud).
        let mut max_replayed_ino: u64 = 0;
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                let tree = match tree_id {
                    TREE_INODES => &inodes,
                    TREE_DENTRIES => &dentries,
                    TREE_XATTRS => &xattrs,
                    TREE_ALLOC_RESERVED => continue,
                    _ => continue,
                };
                if level > 0 {
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
                    )
                    .await?;
                    continue;
                }
                if tree_id == TREE_INODES {
                    if let Ok(ino) = decode_inode_key(&rec.key) {
                        max_replayed_ino = max_replayed_ino.max(ino);
                    }
                }
                tree.apply_replayed(
                    &rec.key,
                    rec.seq,
                    rec.kind,
                    Bytes::copy_from_slice(&rec.value),
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
        let strict = crate::meta_backend::resolve_flush_interval_ms() == 0;
        let read_only = sb.unknown_ro() != 0;
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
        Ok(Self {
            path: path.to_path_buf(),
            sb,
            checkpoint_seq: AtomicU64::new(ledger.seq),
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
            timeout_threshold: squeezefs_timeout_env(),
            smo,
            retire_seq,
            pending_reclaim: std::sync::Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
            ckpt_wake: Arc::new(tokio::sync::Notify::new()),
            ckpt_join: std::sync::Mutex::new(None),
            ckpt_alive: std::sync::Mutex::new(Weak::new()),
            atomicity_physical: std::sync::OnceLock::new(),
            guard_fd: std::sync::Mutex::new(None),
            writer_id: String::new(),
            boot_id: String::new(),
            claimed: AtomicBool::new(false),
            reservations: None,
            pr_key: 0,
            pr_identity: std::sync::OnceLock::new(),
            pr_active: AtomicBool::new(false),
            guard_fenced: AtomicU64::new(0),
            pr_reacquires: AtomicU64::new(0),
            barrier_failures: AtomicU64::new(0),
            guard_trace: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The mounted superblock.
    pub fn superblock(&self) -> &SuperblockV3 {
        &self.sb
    }

    /// The ledger record this mount selected (newest valid).
    pub fn mounted_ledger(&self) -> &LedgerRecord {
        &self.ledger
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

    /// This volume's per-record value cap `min(65_536, node_size/4)` (§4.2) —
    /// the largest inline xattr value it can store. PR K8 (§5.3): the data
    /// path consults it (via [`crate::meta_backend::RoutedMetaBackend::xattr_value_cap`])
    /// to decide the per-volume layout inline-spill boundary, so mixed v2/v3
    /// (and mixed-`node_size` v3) sets spill per volume.
    pub fn record_value_cap(&self) -> usize {
        self.cache.config().layout.record_value_cap()
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

    /// Attributes of `ino` from the inode tree (K1 fold; Δtime deltas
    /// folded into the base record).
    pub async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
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
    /// release its pending frees, and advance the cache's durable tail
    /// (torn-tail classifier + tombstone-elision floor).
    pub(super) fn after_durable_barrier(&self) {
        let drained: Vec<(u64, u64)> = {
            let mut g = self.pending_reclaim.lock().unwrap();
            std::mem::take(&mut *g)
        };
        for (ledger_seq, tail) in drained {
            self.alloc.advance_durable(ledger_seq);
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

    /// R5 defense-in-depth gauge (follow-up C): the node cache's RAM
    /// estimate at its budget-accounting basis (cached nodes × node size)
    /// plus the dirty-node count — registered with the mem-budget
    /// authority so metadata RAM is visible to pressure accounting. The
    /// dhat attribution showed the KV core was NOT the flood, but "no
    /// per-component cap can see the sum" (§5.7) applies to it like every
    /// other consumer.
    pub fn node_cache_gauge(&self) -> (u64, u64) {
        let node_size = self.cache.config().layout.node_size() as u64;
        let mut nodes = 0u64;
        let mut dirty = 0u64;
        self.node_cache().for_each_node(|n| {
            nodes += 1;
            if n.dirty_floor() != u64::MAX {
                dirty += 1;
            }
        });
        (nodes * node_size, dirty)
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
            rsv_call(&rsv, move |c| c.register(key))
                .await
                .map_err(|e| self.pr_error("reservation register", e))?;
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
                        // holdership.
                        let key = self.pr_key;
                        let re = async {
                            rsv_call(&rsv, move |c| c.register(key)).await?;
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
}

impl KvTx {
    #[track_caller]
    fn new() -> Self {
        Self {
            staged: Vec::new(),
            site: std::panic::Location::caller(),
        }
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

    /// **The §4.4 commit pipeline.** Every mutating op stages a [`KvTx`]
    /// and lands here:
    ///
    /// 1. exact entry size from the staged records (the §4.1 writer-side
    ///    128 KiB guard);
    /// 2. **pre-lock ring admission** — parks holding NO node locks
    ///    (counted in `meta_kv_journal_full_stalls`), woken by
    ///    `reusable_upto` advances (§4.4 pt 5);
    /// 3. resolve every record's leaf, lock the deduped set in
    ///    **ascending NodeId order**, revalidate under the locks
    ///    (not superseded, key in range) — stale ⇒ unlock, re-resolve,
    ///    retry (§4.6, counted);
    /// 4. capture per-key pre-images (the rollback's undo);
    /// 5. **reserve inside the locks** (one `fetch_add`; seq_i = start+i)
    ///    and apply to the in-RAM deltas — reservation and apply share
    ///    the lock window, so per-key journal-seq order equals RAM apply
    ///    order (§4.4 pt 2) and the checkpoint's flush pass (which takes
    ///    the same node locks) can never observe a reservation whose
    ///    records it cannot see;
    /// 6. unlock; write the entry bytes — the committer's own
    ///    `write_at`/`write_at_batch` (§4.4 pt 3);
    /// 7. wait for the completed-prefix watermark to cover this entry
    ///    (chain-reachability: predecessors' bytes must be in page cache
    ///    before this tx acks — the K3 barrier observation), then barrier
    ///    per flush mode (strict = coalesced fdatasync; deferred = flag
    ///    the flusher);
    /// 8. on write failure: **seq-conditional rollback** + escalation.
    async fn commit_tx(&self, tx: KvTx) -> std::result::Result<(), KvError> {
        if tx.is_empty() {
            return Ok(());
        }
        // D4.a attribution: the construction site this (about-to-be-
        // committed) tx counts against on success.
        let site = tx.site;
        // (1) Exact size before any lock (§4.4 pt 5).
        let mut recs: Vec<(u8, Record)> = tx
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

        // (2) Admission — park holding nothing the drain needs, with the
        // D1.b escalation rung (design-metadata-throughput §5.1, audit
        // row 2): a wedged-not-failed drain never notifies, so each park
        // is time-bounded (the liveness re-check must not depend on a
        // wake) and cumulative parked time ≥ `timeout_threshold` logs
        // loud + trips `note_journal_failure` once per crossing. Repeated
        // crossings latch `failed` (JOURNAL_FAILURE_LATCH — ~3× threshold
        // for a solo committer, ~1× under real op concurrency where every
        // parked committer crosses); the loop's flag re-check then fails
        // this op with EIO and the routed layer mirrors the volume into
        // `disabled_volumes`. Self-arbitrating: any other committer's
        // entry-write success resets `journal_failures` (step 7), so a
        // merely starved-but-alive volume logs loud without fail-stopping.
        let adm = {
            let threshold = self.timeout_threshold;
            let mut parked_since: Option<std::time::Instant> = None;
            loop {
                if let Some(adm) = self.ring.try_admit(len, AdmissionClass::User) {
                    break adm;
                }
                self.stalls.fetch_add(1, Ordering::Relaxed);
                let notified = self.ring_space_notified();
                if let Some(adm) = self.ring.try_admit(len, AdmissionClass::User) {
                    break adm;
                }
                // Re-check liveness flags after each park so shutdown/failure
                // cannot strand a parked committer.
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
                        "meta volume {}: committer parked {} ms (≥ {} ms) waiting for \
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
        };

        // Test seam (PR M4 D1.b — the `TEST_TIER_PUBLISH_DELAY_MS`
        // precedent: one relaxed load per commit, zero-cost when unset, no
        // `#[cfg(test)]` fork of the production path): an artificial stall
        // INSIDE the cancellation hazard window — `adm` is held, nothing
        // reserved yet — so the watchdog suite can hold a live commit here
        // long enough to prove (a) no per-op `timeout()` drops this future
        // any more (a drop here leaks the admission's ring budget forever)
        // and (b) the overdue op is visible to the watchdog scan.
        {
            let stall = TEST_COMMIT_ADMITTED_STALL_MS.load(Ordering::Relaxed);
            if stall > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(stall)).await;
            }
        }

        // (3–5) The locked window, with revalidate/retry.
        let mut attempt = 0usize;
        let (res, undo) = loop {
            attempt += 1;
            if attempt > COMMIT_RETRY_BUDGET {
                self.ring.core().release(adm);
                return Err(KvError::Corrupt(
                    "commit retry budget exhausted (revalidation never passed — SMO \
                     protocol bug)"
                        .to_string(),
                ));
            }
            // Latch-free resolution: record index → leaf.
            let mut leaves: Vec<Arc<CachedNode>> = Vec::with_capacity(recs.len());
            for (tree_id, r) in &recs {
                leaves.push(self.tree_by_id(*tree_id).resolve_leaf(&r.key).await?);
            }
            // Deduped ascending-NodeId lock order (§4.4 pt 1).
            let mut lock_set: Vec<Arc<CachedNode>> = leaves.clone();
            lock_set.sort_by_key(|n| n.addr());
            lock_set.dedup_by_key(|n| n.addr());
            let mut guards = Vec::with_capacity(lock_set.len());
            for node in &lock_set {
                guards.push(node.lock().write().await);
            }
            // Revalidate under the locks (§4.6).
            let stale = leaves.iter().zip(&recs).any(|(leaf, (_, r))| {
                leaf.state().is_superseded()
                    || r.key[..] < *leaf.min_key()
                    || r.key[..] > *leaf.max_key()
            });
            if stale {
                drop(guards);
                super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // (4) Pre-images, once per distinct (tree, key).
            let mut undo: Vec<UndoKey> = Vec::new();
            for (i, (tree_id, r)) in recs.iter().enumerate() {
                if recs[..i]
                    .iter()
                    .any(|(t, p)| *t == *tree_id && p.key == r.key)
                {
                    continue;
                }
                let pre = leaves[i].snapshot().lookup(&r.key)?;
                undo.push(UndoKey {
                    tree_id: *tree_id,
                    key: r.key.clone(),
                    pre,
                });
            }

            // (5) Reserve inside the window; stamp seqs; RAM apply.
            let res = self.ring.reserve_registered(adm);
            for (i, (_t, r)) in recs.iter_mut().enumerate() {
                r.seq = res.start + i as u64;
            }
            // Group the applies per lock-set entry so each node gets one
            // apply + one snapshot swap.
            for node in &lock_set {
                let group: Vec<OwnedRec> = recs
                    .iter()
                    .zip(&leaves)
                    .filter(|(_, leaf)| leaf.addr() == node.addr())
                    .map(|((_, r), _)| OwnedRec {
                        key: Bytes::copy_from_slice(&r.key),
                        seq: r.seq,
                        kind: r.kind,
                        value: Bytes::copy_from_slice(&r.value),
                    })
                    .collect();
                if group.is_empty() {
                    continue;
                }
                let gi = lock_set
                    .iter()
                    .position(|n| n.addr() == node.addr())
                    .expect("node is in its own lock set");
                node.apply_locked(&mut guards[gi], group)?;
            }
            // Writeback pressure (§4.6 pt 1's SECOND trigger: "when a
            // node's dirty delta exceeds a bset worth") — flagged inside
            // the window, drained by the checkpoint task.
            let mut threshold_crossed = false;
            for (gi, node) in lock_set.iter().enumerate() {
                if guards[gi].overlay_bytes() >= self.cache.config().writeback_delta_bytes {
                    let (tree_id, _) = recs
                        .iter()
                        .zip(&leaves)
                        .find(|(_, leaf)| leaf.addr() == node.addr())
                        .map(|((t, _), _)| (*t, ()))
                        .expect("group nonempty implies a record");
                    self.tree_by_id(tree_id).enqueue_maintenance(node.addr());
                    threshold_crossed = true;
                }
            }
            drop(guards);
            if threshold_crossed {
                // Wake the task for a maintenance-only pass NOW (appends,
                // no barrier/ledger — those stay on cadence): letting the
                // open delta balloon for a whole tick makes every commit's
                // RAM apply pay O(delta) — the K7 create-row cliff. The
                // permit coalesces storms into one pending wake.
                self.ckpt_wake.notify_one();
            }
            break (res, undo);
        };

        // (6–8) The committer's own bytes, outside every lock.
        match self.ring.commit_entry(&res, &recs).await {
            Ok(()) => {
                // D4.a: one successful commit_tx = one journal entry,
                // counted against the tx's construction site (an scc
                // bucket read + relaxed fetch_add — noise against the
                // pipeline's own cost, and per COMMIT, not per FUSE op).
                super::note_commit_site(site);
                // D0 chain-reachability: this tx is findable only through
                // its predecessors — wait for the completed prefix.
                self.ring.wait_completed_upto(res.end()).await;
                self.journal_failures.store(0, Ordering::Release);
                if self.strict {
                    // Strict-0 group commit (§4.6 pt 4): the coalesced
                    // barrier covers this entry AND every prior in-flight
                    // journal write (registration follows completion —
                    // the K3 fsync-barrier ordering observation).
                    self.sync_device().await.map_err(KvError::Io)?;
                } else {
                    self.needs_flush.store(true, Ordering::Release);
                }
                Ok(())
            }
            Err(e) => {
                log::warn!(
                    "meta volume {}: journal entry write failed (seq {}): {e} — rolling back",
                    self.path.display(),
                    res.seq()
                );
                self.rollback_failed_tx(res.start, res.end(), &undo).await;
                self.note_journal_failure();
                // The reserved range is now a PERMANENT hole in the ring:
                // any acked entry that shares its page is chain-reachable
                // only through it (§4.1 discovery loses same-page
                // successors of a dead chain). Restore replay ≡ RAM by
                // forcing a checkpoint PAST the hole — the §4.4 pt 5
                // minimal-checkpoint progress theorem guarantees it needs
                // zero ring bytes, so it runs even on a sick device's
                // full ring. If the device refuses this too, the volume
                // is escalating to fail-stop anyway.
                if let Err(ck) = self.checkpoint_now().await {
                    log::error!(
                        "meta volume {}: post-failure checkpoint could not drain the \
                         journal hole: {ck} (volume escalating)",
                        self.path.display()
                    );
                    self.note_journal_failure();
                }
                Err(e)
            }
        }
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
                    vec![OwnedRec {
                        key: Bytes::copy_from_slice(&r.key),
                        seq: r.seq,
                        kind: r.kind,
                        value: Bytes::copy_from_slice(&r.value),
                    }],
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

    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
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
        let _guards = self.dlm.lock_many(&lock_plan, &[]).await;

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
        self.commit_tx(tx).await?;
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
        global_of: impl FnOnce(Ino) -> Ino,
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
        let local_ino = self.allocate_ino();
        let global_ino = global_of(local_ino);
        let now = Self::now_ns();
        let child = InodeValue {
            mode: final_mode,
            uid,
            gid: final_gid,
            nlink: if is_dir { 2 } else { 1 },
            flags: 0,
            flags2: 0,
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
            flags2: 0,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
        };
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_ino), v.encode());
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
        let now = Self::now_ns();
        child.ctime = now;
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
        self.commit_tx(tx).await?;
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
        v.ctime = Self::now_ns();
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_child), v.encode());
        self.commit_tx(tx).await?;
        Ok(v)
    }

    /// Parent-side directory nlink delta (routed rename of a directory
    /// across parents), best-effort like the v2 arm (`if let Ok`) —
    /// times untouched, exactly the v2 rename's nlink shift.
    pub async fn routed_parent_nlink_delta(&self, local_parent: Ino, delta: i64) -> Result<()> {
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
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Routed same-volume rename — ONE whole-tx entry covering the v2
    /// arm's semantics: EXCHANGE swap, NOREPLACE guard, replace with
    /// destination nlink accounting + ENOTEMPTY (when the destination
    /// inode is local: `dest_local`), directory-move parent nlink shifts.
    #[allow(clippy::too_many_arguments)] // the routed rename's parameter surface
    pub async fn routed_rename_local(
        &self,
        local_old_parent: Ino,
        old_name: &str,
        local_new_parent: Ino,
        new_name: &str,
        flags: u32,
        dest_local: Option<Ino>,
    ) -> Result<()> {
        self.write_gate()?;
        let old_pos = self.find_dentry_pos(local_old_parent, old_name).await?;
        let new_pos = self.find_dentry_pos(local_new_parent, new_name).await?;
        let mut tx = KvTx::new();
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
        let now = Self::now_ns();
        if is_dir && cross_dir {
            // Parent nlink shift, best-effort like the v2 arm.
            if let Some(mut op) = self.read_inode_value(local_old_parent).await? {
                if op.nlink > 2 {
                    op.nlink -= 1;
                }
                tx.stage_put(TREE_INODES, inode_key(local_old_parent), op.encode());
            }
            if let Some(mut np) = self.read_inode_value(local_new_parent).await? {
                np.nlink += 1;
                tx.stage_put(TREE_INODES, inode_key(local_new_parent), np.encode());
            }
        }
        if let Some((new_key, _new_d)) = new_pos {
            if let Some(dest) = dest_local {
                if let Some(mut dv) = self.read_inode_value(dest).await? {
                    if (dv.mode & libc::S_IFMT) == libc::S_IFDIR
                        && self.dir_has_entries(dest).await?
                    {
                        return Err(crate::error::SqueezefsError::Io(
                            std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                        ));
                    }
                    if dv.nlink > 0 {
                        dv.nlink -= 1;
                    }
                    dv.ctime = now;
                    tx.stage_put(TREE_INODES, inode_key(dest), dv.encode());
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
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Rename-replace destination handling when the destination inode
    /// lives on ANOTHER volume: ENOTEMPTY probe + nlink dec + ctime (the
    /// v2 arm's `if let Ok` best-effort shape).
    pub async fn routed_dest_replace(&self, local_dest: Ino) -> Result<()> {
        self.write_gate()?;
        let Some(mut dv) = self.read_inode_value(local_dest).await? else {
            return Ok(());
        };
        if (dv.mode & libc::S_IFMT) == libc::S_IFDIR && self.dir_has_entries(local_dest).await? {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
            ));
        }
        if dv.nlink > 0 {
            dv.nlink -= 1;
        }
        dv.ctime = Self::now_ns();
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(local_dest), dv.encode());
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// §5.3: layout xattr + size as ONE two-record transaction (v2's
    /// non-transactional two-write path, made atomic on v3) — the
    /// fsync/release writeback shape.
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        self.write_gate()?;
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        v.size = size;
        v.ctime = Self::now_ns();
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
        self.commit_tx(tx).await?;
        Ok(())
    }
}

impl KvMetaBackend {
    /// The setattr body with the DLM I-guard already held — the routed
    /// layer's entry (it locks through `volume_dlm`, the SAME manager, so
    /// re-locking here would self-deadlock on a stripe).
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
    ) -> Result<Inode> {
        self.write_gate()?;
        let mut v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
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
            v.ctime = Self::now_ns();
        }
        let mut tx = KvTx::new();
        tx.stage_put(TREE_INODES, inode_key(ino), v.encode());
        self.commit_tx(tx).await?;
        Ok(Self::to_inode(ino, &v))
    }

    /// The setxattr body with the DLM I-guard already held (see
    /// [`Self::setattr_locked`] for the re-entrancy rationale).
    pub async fn setxattr_locked(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        self.write_gate()?;
        let cap = self.cache.config().layout.record_value_cap();
        // The value rides an XattrValue envelope (name + lengths); keep
        // the whole record under the cap the node layer enforces.
        if value.len() + name.len() + 8 > cap {
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
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// The removexattr body with the DLM I-guard already held.
    pub async fn removexattr_locked(&self, ino: Ino, name: &str) -> Result<()> {
        self.write_gate()?;
        let tx0 = KvTx::new();
        let (existing, key) = self.xattr_slot(&tx0, ino, name).await?;
        if !existing {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Xattr not found",
            )));
        }
        let mut tx = tx0;
        tx.stage_delete(TREE_XATTRS, key);
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
    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        self.write_gate()?;
        let _parent_guard = self.dlm.lock_inode_exclusive(parent).await;
        let _dentry_guard = self.dlm.lock_dentry_exclusive(parent, name).await;

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
            flags2: 0,
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
            let _full = self
                .dlm
                .lock_many(inode_set, &[(parent, name, LockMode::Exclusive)])
                .await;
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
            self.commit_tx(tx).await?;
            return Ok(ino);
        }
    }

    /// Mirrors the retired v2 `link`: whole set locked upfront, EEXIST
    /// check, nlink+1 + ctime, parent-time full update — one entry.
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        self.write_gate()?;
        let _guards = self
            .dlm
            .lock_many(
                &[
                    (new_parent, LockMode::Exclusive),
                    (ino, LockMode::Exclusive),
                ],
                &[(new_parent, new_name, LockMode::Exclusive)],
            )
            .await;

        if self.find_dentry(new_parent, new_name).await?.is_some() {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }
        let mut child = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        child.nlink += 1;
        let now = Self::now_ns();
        child.ctime = now;

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
        self.commit_tx(tx).await?;
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
        let _guards = self
            .dlm
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
            .await;

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
        }
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
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        self.setattr_locked(ino, mode, uid, gid, size, atime, mtime, ctime)
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
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        self.setxattr_locked(ino, name, value).await
    }

    /// Remove one xattr; absent names fail loud with the v2 NotFound
    /// shape.
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        self.write_gate()?;
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        self.removexattr_locked(ino, name).await
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        KvMetaBackend::listxattr(self, ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        // Single destroy == a size-1 batch: one code path (the v2 shape).
        self.destroy_inodes(std::slice::from_ref(&ino)).await
    }
}
