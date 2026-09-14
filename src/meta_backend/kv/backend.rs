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
//!
//! ## The two-stage commit conveyor (D-2 — e2e perf audit DLM board #2)
//!
//! The M7 conveyor (design-metadata-throughput §5.5 D5) ran every user
//! commit on a volume through ONE serialized pass: drain → Σ admission →
//! union leaf locks (RAM apply) → unlock → journal ring write → completed-
//! prefix wait → (strict) barrier → fan-out. The device write's completion
//! sat INSIDE that server's service time — on the D-1b fleet row the
//! authority's pass was 752–776 µs of which `journal_ring_write` was
//! 650–672 µs and the leaf-lock window 86–91 µs, at ρ ≈ 0.97, every
//! committer queued 764–801 µs behind it
//! (`.benchmarks/2026-09-02-d1b-publish-plane-batching.md`). Since D-2
//! the conveyor is two stages:
//!
//! * **Stage A — the apply pass** (`KvMetaBackend::conveyor_pass_task`,
//!   `KvMetaBackend::run_batch`): drain → admission → union leaf locks →
//!   revalidate → pre-images → ONE contiguous reservation → RAM apply →
//!   unlock → **encode + SUBMIT** the surviving entries (one `uring_fs`
//!   submission, no wait — [`super::journal::JournalRing::
//!   submit_entries_batch`]) → hand the `ConveyorWindow` to stage B.
//!   Stage A never waits on the device; its service time is the leaf-lock
//!   window plus the encode.
//! * **Stage B — the durability lane** (`KvMetaBackend::durability_lane_task`,
//!   `KvMetaBackend::run_windows`): one
//!   per-volume task draining windows in handoff order; per GROUP (the
//!   head window awaited + every successor whose write has already
//!   landed): complete the reservations, ONE completed-prefix wait, the
//!   §4.4 pt 4 hole checkpoints, ONE strict barrier, then the members'
//!   terminal outcomes in journal order. Stage B never touches a node lock
//!   except in the rollback arm of a FAILED write (below).
//!
//! N windows sit between A and B; the bound is the ring's admissible
//! capacity (§4.4 pt 5) — every window holds a registered reservation and
//! stage A parks on ring admission before any node lock — never a
//! constant. Engagement: `meta_conveyor_windows_inflight[_hwm]`.
//!
//! **What is unchanged, and why.** One tx = one checksummed journal entry
//! (the window's entries are the same N ordinary entries in the same
//! contiguous reservation — zero on-disk change); whole-tx atomicity and
//! torn-write immunity are per entry (§4.10) and do not see the stages. A
//! tx is acked only after its entry landed (deferred cadence: the D0 law —
//! stage B awaits the write) or after a barrier that followed it (strict).
//! Acks are in journal order: a later window is never answered while an
//! earlier one's entry is unlanded, because an entry is chain-reachable
//! only through its predecessors and a hole ahead of it strands it —
//! stage B's in-order groups + the completed-prefix wait + the hole
//! checkpoints before any ack are exactly the pre-D-2 tail's discipline,
//! applied per window. DLM guards stay co-owned by the queue entries until
//! the STAGE-B terminal outcome (never released at apply), so the Issue-13
//! same-key exclusion is untouched. The D0 fail-stop lattice fires from
//! stage B exactly as it fired from the pass: `note_barrier_failure` at
//! the strict barrier, `note_journal_failure` on a failed write and on a
//! stuck hole checkpoint, `JOURNAL_FAILURE_LATCH` consecutive failures
//! latching `failed`. Each stage is panic-guarded (`PassSentinel`,
//! `LaneSentinel`): an unwind abandons the reservations (never a
//! `completed_upto` wedge), answers every member EIO, fails out the queue
//! behind it, and fail-stops the volume when an applied window's write
//! outcome is unknown (RAM would diverge from replay).
//!
//! **Lock order 4b — why the acyclicity argument survives.** Leaf-lock
//! TAKERS are now {stage A, stage B's failed-write rollback arm, the
//! checkpoint/SMO task}. Stage A takes leaf locks only, ascending NodeId,
//! deduped, lock-then-revalidate-then-retry, and drops them BEFORE the
//! submission — the hold never spans device I/O or a ring-space wait
//! (`lock_phase_ns.leaf_lock_hold` is the tripwire). Stage B's rollback
//! arm is the §4.4 pt 4 committer rollback verbatim (`rollback_failed_tx`:
//! leaf locks only, ascending, deduped, revalidated, released before its
//! compensation commit) — the design wrote that rollback for CONCURRENT
//! committers whose writes complete out of apply order, which is exactly
//! the population two stages recreate; it is seq-conditional, so a later
//! window's apply over the same key (only Δtime merge records can share a
//! key across windows — every other same-key writer is still excluded by
//! the guards the failed window's entries hold) is never clobbered.
//! Interior locks stay the serialized SMO task's (parent-then-child).
//! Wait-for edges: A waits on leaf locks (held by A-itself ascending, by
//! B's rollback, or by the checkpoint freeze — all RAM-only, none waiting
//! on A) and on ring admission holding nothing; B waits on uring
//! completions (independent), on `completed_upto` (advanced by B itself
//! and by the checkpoint task's own SMO completions, which never wait on
//! B), on the SMO mutex for hole checkpoints (held by the checkpoint task,
//! which waits on leaf locks only — never on B), and, in the rollback arm,
//! on leaf locks under the ascending discipline. No holder of a node lock
//! ever waits on B, and B holds node locks only while waiting on other
//! node locks ascending; the checkpoint task's own admissions never park
//! (they drain-and-retry). The two populations stay acyclic without any
//! NodeId relationship between leaves and interiors, as before.
//!
//! **The checkpoint tail rule** (`checkpoint.rs`) needs no change: a
//! window's reservation stays registered from stage A's in-lock reserve
//! until stage B observes its write outcome, so `min_inflight_start`
//! holds the tail — and `reusable_upto` — behind every applied-but-
//! unlanded window, exactly the span the rule was written for.
//! `shutdown`'s final cycle drains in-flight windows through the same
//! `wait_completed_upto(head)` loop (stage B completes them as their
//! writes land).

use super::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
use super::checkpoint::{read_newest_ledger, LedgerRecord};
use super::conveyor_core::ConveyorCore;
use super::journal::{checkpoint_reserve_bytes, entry_len_for, untag, JournalRing};
use super::journal_core::{AdmissionClass, Reservation};
use super::node::{key_successor, NodeLayout};
use super::node_cache::{
    CachedNode, LiveLookup, NodeCache, NodeCacheConfig, NodeDirty, OwnedRec,
    DEFAULT_WRITEBACK_DELTA_BYTES,
};
use super::record::{
    decode_dentry_key, decode_inode_key, decode_readdir_cookie, decode_xattr_key, dentry_key,
    dentry_name_hash54, encode_readdir_cookie, first_free_coll_seq, inode_key, xattr_key,
    xattr_name_hash56, DentryValue, InodeDelta, InodeValue, ReaddirPos, Record, RecordKind,
    XattrValue, HASH54_MAX, HASH56_MAX, INODE_KEY_LEN, TREE_ALLOC_RESERVED, TREE_DENTRIES,
    TREE_INODES, TREE_XATTRS, XATTR_KEY_LEN,
};
use super::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use super::tree::{decode_interior_value, KvTree, RootPtr, SmoContext, SmoJournal};
use super::KvError;
use super::{block_map, block_refs};
use crate::error::Result;
use crate::meta_backend::atomicity::META_VOLUME_ATOMICITY_COW;
use std::borrow::Cow;
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

/// Test seam (design-symmetric-metadata PR 1, review Issue 4): make the
/// next N tree-0 root publications answer `JournalReserveExhausted` —
/// the deferral arm a full checkpoint reserve produces — so a suite can
/// prove a deferred publication keeps every unpublished root's records
/// in the replay window. `u32::MAX` = defer until cleared; `0` = off
/// (one relaxed load per publication attempt).
pub static TEST_FOREST_PUBLISH_DEFER: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Test seam (PR M7 §5.5 D5): hold the conveyor pass at a protocol stage
/// so arrivals accumulate deterministically (no sleep-based batching in
/// tests — the hold IS the scenario). `0` = off (one relaxed load per
/// pass iteration); [`TEST_CONVEYOR_HOLD_PRE_DRAIN`] parks the apply pass
/// before it drains a batch (entries queue up behind it);
/// [`TEST_CONVEYOR_HOLD_PRE_FANOUT`] parks the durability lane after a
/// group is written, prefix-waited and barriered but before its results
/// fan out (the cancel-pre-fanout stage). Release via
/// [`test_conveyor_hold_release`].
pub static TEST_CONVEYOR_HOLD_STAGE: AtomicU64 = AtomicU64::new(0);

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the apply pass before draining.
pub const TEST_CONVEYOR_HOLD_PRE_DRAIN: u64 = 1;

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the durability lane after a
/// group's write/prefix/barrier, before per-tx result fan-out.
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

/// [`TEST_CONVEYOR_HOLD_STAGE`] value: park the durability lane AFTER a
/// group's reservations are completed (step 8 — `min_inflight_start`
/// clear) and BEFORE its per-window verdicts (step 9 — the §4.4 pt 4
/// rollback and its compensation): the window is in flight with its
/// reservation closed, the shape review round 2 (Issue 23) named as the
/// one a ring growth could race.
pub const TEST_CONVEYOR_HOLD_PRE_ROLLBACK: u64 = 4;

/// Monotonic count of lane groups that PARKED on
/// [`TEST_CONVEYOR_HOLD_PRE_ROLLBACK`] — the test-side barrier that the
/// schedule formed.
static TEST_CONVEYOR_PRE_ROLLBACK_PARKED: AtomicU64 = AtomicU64::new(0);

/// Lane groups parked on [`TEST_CONVEYOR_HOLD_PRE_ROLLBACK`] so far.
pub fn test_conveyor_hold_parked() -> u64 {
    TEST_CONVEYOR_PRE_ROLLBACK_PARKED.load(Ordering::Acquire)
}

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

/// Test seam (RECLAIM-ATOMIC, the corpse-sweep record's residual A): the
/// commit that carries a reclaimed ino's durable reference RELEASES is
/// refused before it is staged — the failed-release shape the reclaim
/// batch and the sweep must answer by RETAINING the record (never
/// destroying it with its references still on the ledger). One relaxed
/// load per release commit; `false` = off.
pub static TEST_FAIL_RECLAIM_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// [`TEST_FAIL_RECLAIM_RELEASE`]'s setter (the `test_set_*` idiom).
pub fn test_set_fail_reclaim_release(on: bool) {
    TEST_FAIL_RECLAIM_RELEASE.store(on, Ordering::Relaxed);
}

/// Test seam (RECLAIM-ATOMIC, residual B): a single-ino chunked destroy
/// STOPS after this many committed entries — the durable state of a crash
/// between two of its chunks, produced deterministically. `0` = off (one
/// relaxed load per chunk). The daemon reads the registered knob
/// `SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER` into the same word at open.
pub static TEST_DESTROY_CHUNK_STOP_AFTER: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// [`TEST_DESTROY_CHUNK_STOP_AFTER`]'s setter (`None` = off).
pub fn test_set_destroy_chunk_stop_after(after: Option<u32>) {
    TEST_DESTROY_CHUNK_STOP_AFTER.store(after.unwrap_or(0), Ordering::Relaxed);
}

/// Test seam (review round 2, Issue 16): cap the shutdown's final-cycle
/// FIXPOINT at this many cycles (`0` = the product bound). `1` is the
/// defective ring-0-only loop's exact shape — a leased slot tree's SMO in
/// the final flush pass leaves its region's ring UNCOVERED at the leave —
/// so the leave's belt (never a `Free` page over an uncovered window) is
/// observable. One relaxed load per shutdown.
pub static TEST_SHUTDOWN_FIXPOINT_CYCLES: AtomicU64 = AtomicU64::new(0);

/// Test seam (design-symmetric-metadata §5.3.5 / KD-SYM-7, review round 1
/// Issue 3): `JoinAppender` FAILS after its page went `Live` (both
/// directory slots barriered) and before its initial extent grant — the
/// durable state of a manager killed in that window. The replay of the
/// join must complete it: `already`, AND the grant the interrupted join
/// owed. One relaxed load per join (a control-plane verb); `false` = off.
pub static TEST_JOIN_HOLD_AFTER_PAGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test seam (design-symmetric-metadata §5.3.4 row 5, PR 4): the holder
/// DIES mid-handover after its page named the slot `Releasing` and before
/// tree 0 was written — the next open of its identity completes the
/// release from the page's `Releasing` entry. `false` = off.
pub static TEST_HANDOVER_HOLD_AFTER_PAGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test seam (review round 2, Issue 6's contract): PARK a handover after
/// its page named the slot `Releasing` and before tree 0 is written —
/// the window a commit on the departing holder is issued into. Released
/// by [`test_handover_park_release`]; one relaxed load per handover.
pub static TEST_HANDOVER_PARK_AFTER_PAGE: AtomicBool = AtomicBool::new(false);

/// Handovers that PARKED on [`TEST_HANDOVER_PARK_AFTER_PAGE`] so far (the
/// test-side barrier that the schedule formed).
static TEST_HANDOVER_PARKED: AtomicU64 = AtomicU64::new(0);

/// Handovers parked on [`TEST_HANDOVER_PARK_AFTER_PAGE`] so far.
pub fn test_handover_parked() -> u64 {
    TEST_HANDOVER_PARKED.load(Ordering::Acquire)
}

static TEST_HANDOVER_PARK_NOTIFY: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Release every handover parked on [`TEST_HANDOVER_PARK_AFTER_PAGE`]
/// (the flag is stored `false` first; the notify wakes the loop).
pub fn test_handover_park_release() {
    TEST_HANDOVER_PARK_AFTER_PAGE.store(false, Ordering::Relaxed);
    TEST_HANDOVER_PARK_NOTIFY.notify_waiters();
}

/// Test seam (§5.3.4 row 6, PR 4): the holder DIES after the manager's
/// tree-0 `Unleased` landed and before it dropped the slot from its page
/// — page `Releasing` ∧ tree 0 `Unleased`: tree 0 wins, the entry is
/// dropped at the next open. `false` = off.
pub static TEST_HANDOVER_HOLD_AFTER_TREE0: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test seam (review round 2, Issue 8): the mount DIES during its clean
/// leave AFTER every region's leases went `Unleased` in tree 0 and BEFORE
/// its pages went `Free` — the pages still attest the released slots. A
/// remount over that history (another appender having leased one of the
/// slots since) must read the attestations as STALE, never as a C14
/// conflict. `false` = off.
pub static TEST_LEAVE_HOLD_AFTER_RELEASES: std::sync::atomic::AtomicBool =
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
            // does not belong to. The block-map tree IS ino-keyed, but
            // its migration story is PR 4/5's (the VL5b engine copies the
            // three user trees only) — skipped like the other structural
            // trees until the walkers land.
            if *tree_id == TREE_ALLOC_RESERVED
                || *tree_id == super::record::TREE_BLOCK_REFS
                || *tree_id == super::record::TREE_BLOCK_MAP
            {
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

/// A forest volume's live census ([`KvMetaBackend::forest_census`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForestCensus {
    /// Slot trees that exist (the native one included).
    pub slot_trees: u64,
    /// Guest slot roots tree 0 currently names.
    pub control_records: u64,
    /// Guest slot trees minted this mount.
    pub minted: u64,
    /// `slot_state` publications this mount.
    pub root_publishes: u64,
}

/// The trees of one mounted volume — the two on-disk layouts this binary
/// serves. Exactly the sites that match on it are the layout-dependent
/// ones; everything else routes through [`KvMetaBackend`]'s kind-keyed
/// helpers.
enum TreeSet {
    /// The shipped layout: one tree per record kind (§4.2), plus the
    /// bit-9 block-reference tree and the bit-16 block-map tree when
    /// engaged.
    Flat {
        /// Shared node cache behind the three trees (§4.5). Held for tree
        /// lifetime; the trees clone the `Arc`.
        inodes: Arc<KvTree>,
        dentries: Arc<KvTree>,
        xattrs: Arc<KvTree>,
        /// Pre-RC spec §6.2 item 1 (incompat bit 9): the **durable
        /// block-reference tree** — `Some` exactly when this volume carries
        /// [`super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS`] and
        /// the mount may write (a read-only mount never mints a root).
        /// `None` means derived accounting, i.e. pre-item-1 behavior
        /// verbatim. Not one of the three §4.2 user trees: the §4.10
        /// digest walk / slot-migration keyspace / fsck tree walk are
        /// defined over those; structural consumers (checkpoint flush,
        /// ledger roots, maintenance) use [`KvMetaBackend::all_trees`].
        block_refs: Option<Arc<KvTree>>,
        /// PB-class files, PR 1 (docs/design-kvmap-block-map-tree.md): the
        /// **block-map tree** — `Some` exactly when this volume carries
        /// [`super::superblock::FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE`] (bit
        /// 16) and the mount may write. `None` means inline/`indirect:`
        /// layout heads only — and, unlike `block_refs`, staging into an
        /// absent tree REFUSES loud (map records ARE the mapping; a silent
        /// skip is data loss). A `OnceLock` (PR 2), not an `Option`: the
        /// bit-16 ratchet ([`KvMetaBackend::block_map_tree_ready`]) can
        /// stamp+mint AT RUNTIME on the owner-served crossing path.
        block_map: std::sync::OnceLock<Arc<KvTree>>,
    },
    /// The slot-tree forest (docs/design-symmetric-metadata.md §5.2,
    /// incompat bit 17): one mixed-kind tree per routing slot + the
    /// control tree. Block references and block maps are RECORD KINDS
    /// inside the slot trees here, so their engagement is a flag, not a
    /// tree: refs ⇔ bit 9 present and the mount may write; the block map
    /// ⇔ bit 16 stamped (ratchetable at runtime like the flat OnceLock).
    Forest {
        forest: super::forest::SlotTrees,
        block_refs: bool,
        block_map: AtomicBool,
    },
}

impl TreeSet {
    /// Every tree of the set ([`KvMetaBackend::all_trees`]'s body).
    fn all(&self) -> Vec<Arc<KvTree>> {
        match self {
            TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                block_refs,
                block_map,
            } => {
                let mut v = vec![Arc::clone(inodes), Arc::clone(dentries), Arc::clone(xattrs)];
                if let Some(t) = block_refs {
                    v.push(Arc::clone(t));
                }
                if let Some(t) = block_map.get() {
                    v.push(Arc::clone(t));
                }
                v
            }
            TreeSet::Forest { forest, .. } => forest.all(),
        }
    }

    /// The heap extents the set's roots occupy — the mounted-root set
    /// the replayed-free carve-out keys on.
    fn root_extents(&self, cache: &NodeCache) -> std::collections::BTreeSet<u64> {
        self.all()
            .iter()
            .map(|t| cache.addr_extent(t.root().addr))
            .collect()
    }
}

/// A resolved tree handle: BORROWED from the backend's own fields on a
/// flat volume (the shipped hot path — no refcount traffic), OWNED on a
/// forest volume (a lazily minted slot tree lives in the router's map).
pub(super) enum TreeRef<'a> {
    Borrowed(&'a KvTree),
    Owned(Arc<KvTree>),
}

impl std::ops::Deref for TreeRef<'_> {
    type Target = KvTree;
    fn deref(&self) -> &KvTree {
        match self {
            TreeRef::Borrowed(t) => t,
            TreeRef::Owned(t) => t,
        }
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
    /// [`super::slot_cursor_core::SlotCursor`] per hosted guest slot, seeded
    /// from the mounted stamp's `slot_cursors` (+ the replayed per-slot
    /// maxima) and
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
    /// The volume's trees: the shipped per-kind layout, or — under
    /// incompat bit 17 — the slot-tree forest. Every record access goes
    /// through the routing helpers below ([`Self::lookup_kind`],
    /// [`Self::range_kind`], [`Self::tree_for_record`], …), which are the
    /// ONE place the two layouts diverge.
    trees: TreeSet,
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
    /// The appender regions of a forest volume (design-symmetric-metadata
    /// §5.3, PR 2): region 0 rides `ring`; declared regions carry their
    /// own rings and pages. `None` on every bit-17-absent volume.
    appenders: Option<Arc<super::appender::AppenderSet>>,
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
    /// The open's posture as the forest replay saw it: `true` for every
    /// door but the write mount's (`open_read_only`, `open_co_writer`,
    /// `open_peer_owned`, `open_probe`) and for a writer degraded by §4.11
    /// unknown-ro bits — the mount that skipped the window records of
    /// unpublished slot trees at replay, and therefore the mount whose
    /// directory listings filter children of those slots
    /// ([`Self::readdir_page`]). Never true on a write mount.
    non_writer: bool,
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
    /// PR 2 (kvmap): serializes the one-time bit-16 stamp + tree-7 mint
    /// ([`Self::block_map_tree_ready`]) — the `layout_delta_ratchet`
    /// discipline, its own mutex so the two one-shot ratchets never
    /// serialize each other.
    block_map_ratchet: crate::sqz_sync::SqzMutex<()>,
    /// PR 2 (kvmap): inos whose crossing train is IN FLIGHT on this
    /// volume (design A3 — the future fsck C11 exemption registry, the
    /// C2/C3 `inflight_exempted` pattern). Registered before the A1
    /// sweep, deregistered after the head flip; keyed on the volume-local
    /// ino the map records key on.
    crossing_inflight: scc::HashMap<Ino, ()>,
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
    /// D-2 (e2e audit DLM #2): the conveyor's DURABILITY stage — the
    /// in-order queue of applied-and-submitted windows the apply pass hands
    /// off, drained by the per-volume durability lane task (same leader-
    /// elect / Weak-upgrade lifecycle as `conveyor`; module docs, "The
    /// two-stage commit conveyor"). Ungauged by the core: its population
    /// is `META_CONVEYOR_WINDOWS_INFLIGHT`, handoff → terminal outcome.
    durability_lane: Arc<ConveyorCore<ConveyorWindow>>,
    /// C-2 (e2e audit DLM #3): the volume's own journal lane — the OS
    /// thread both conveyor stages run on, which owns the volume's
    /// journal io_uring and parks in it ([`super::journal_lane`]).
    /// Spawned on the volume's FIRST commit (a reader / co-writer /
    /// peer-owned volume never commits and never gets one); `None` inside
    /// = `SQUEEZEFS_JOURNAL_LANE=0`, the shipped D-2 shape on the shared
    /// `sqz-meta` pool.
    journal_lane: std::sync::OnceLock<Option<Arc<super::journal_lane::JournalLane>>>,
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
    /// Serializes the manager's verbs (grant / return / join): each
    /// reads tree 0's durable witness, decides, and writes ONE control
    /// entry; two in flight would decide on the same witness.
    manager_verbs: crate::sqz_sync::SqzMutex<()>,
    /// Serializes slot HANDOVERS (`release_slot_handover` — the cadence's
    /// LRU / forced-shrink releases and a requester's accepted offer
    /// recall the same slots): flush-then-transfer is one sequence per
    /// slot, so two in flight over one slot would both attest the release
    /// and the later one would be refused by tree 0's witness.
    handover: crate::sqz_sync::SqzMutex<()>,
    /// The slot-lease cadence's single-flight latch (PR 4, review round
    /// 2 Issue 6): the checkpoint task SPAWNS the cadence on its own task
    /// instead of running it inline — a handover drains the commit door
    /// (waits for admitted commits' terminal outcomes), and an admitted
    /// commit parked at ring admission needs the TICK to free ring space,
    /// so the tick must never wait behind a handover. One cadence run at
    /// a time; a tick that finds one running skips.
    slot_cadence_running: std::sync::atomic::AtomicBool,
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
    /// The metadata namespace's reservation TYPE this mount holds
    /// (design-symmetric-metadata §5.8.1): `true` = Write Exclusive –
    /// Registrants Only (rtype 3 — the manager of an ARMED forest volume,
    /// every other appender a registrant), `false` = the shipped Write
    /// Exclusive (rtype 1). Decided at open; a bit-17-absent mount and an
    /// unarmed forest mount read `false`.
    meta_wero: bool,
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
    /// §4.7 heap-full posture (`meta_kv_heap_full`): set when a growth
    /// commit was refused for space or a flush pass deferred a node for
    /// `NoSpace`; cleared by a cycle that deferred nothing with the growth
    /// floor clear again. The SPACE standstill class — loud once per
    /// transition, never terminal.
    pub(super) heap_full: AtomicBool,
    /// `meta_kv_enospc_refusals`: user commits refused `NoSpace` at heap
    /// admission (each is one `ENOSPC` to userspace).
    pub(super) enospc_refusals: AtomicU64,
    /// `meta_kv_heap_full_cycles`: checkpoint cycles whose flush pass
    /// deferred ≥ 1 node because the allocator answered `NoSpace`.
    pub(super) heap_full_cycles: AtomicU64,
    /// §4.6a (e): `meta_kv_merge_sweeps` — heap-full / backlog merge
    /// sweeps the checkpoint cycle ran.
    pub(super) merge_sweeps: AtomicU64,
    /// §4.6a (h): `meta_kv_merge_candidates` — underfull leaves the last
    /// sweep or census found (LIVE).
    pub(super) merge_candidates: AtomicU64,
    /// §4.6a (d): a sweep was refused a merge at the compaction floor (or
    /// the budget cut its lap) — the recovery wave is not done; the next
    /// cycle sweeps again even once the heap-full posture clears.
    pub(super) merge_backlog: AtomicBool,
    /// §4.6a (e) finalized: the sweep's per-cycle work budget, ms —
    /// `checkpoint::merge_sweep_budget_ms` of the flush interval in force
    /// at open (one tick period, finding 49's drain law).
    pub(super) merge_sweep_budget_ms: u64,
    /// The volume's merge LAP across its trees (`run_merge_sweep`): which
    /// trees completed their lap since the last publish, and the exact
    /// candidate count they reported. Guarded by the SMO mutex's callers;
    /// the std mutex is the `Sync` face.
    pub(super) merge_lap: std::sync::Mutex<VolumeLap>,
    /// `meta_kv_merge_laps`: whole-volume sweep laps completed (every tree
    /// walked, collapsed, counted) — the exact-candidates publish instant.
    pub(super) merge_laps: AtomicU64,
    /// The durable tail `merge_candidates` was counted under (the
    /// `merge_candidates_audit` law's equal-tails premise).
    pub(super) merge_candidates_tail: AtomicU64,
    /// `meta_kv_merge_sweep_ns`: wall ns the sweep calls spent (sum).
    pub(super) merge_sweep_ns: AtomicU64,
    /// `meta_kv_merge_sweep_projections`: nodes projected by the sweep
    /// calls (count) — `merge_sweep_ns ÷ merge_sweep_projections` is the
    /// live ns/leaf the `kv_merge_sweep` bench prices offline.
    pub(super) merge_sweep_projections: AtomicU64,
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
        let open_started = std::time::Instant::now();
        // (1) Layer A first: kernel-arbitrated, cheapest, and the refusal
        // the measured incident (two same-host daemons) needs.
        let guard_fd = match Self::acquire_writer_flock(path) {
            Ok(fd) => fd,
            Err(FlockOutcome::Held) => {
                let holder = Self::probe_claim_best_effort(path).await;
                // Transient-holder absorption (the three waitable arms —
                // see `await_transient_flock_release`): a SAME-PROCESS
                // teardown pin (2026-07-26 — `drop`-without-`shutdown`
                // releases the flock only when the LAST Arc dies, and the
                // detached checkpoint / times-drain / conveyor pass tasks
                // each pin an upgraded Arc for one pass), a dead same-host
                // holder under a transient probe (rung-9 finding #5), or
                // an ANONYMOUS claim-less holder (udev's change-event
                // flock — the mw_fleet --owners finding). A live FOREIGN
                // claimed holder never waits; a live same-process double
                // mount waits once and still refuses at the bound.
                match Self::await_transient_flock_release(path, &holder).await {
                    Some(fd) => fd,
                    None => {
                        // A claim-less first probe re-probes once: a
                        // concurrent mount that won the flock inside its
                        // pre-claim window has committed its claim by now,
                        // and the refusal should name it, never guess.
                        let holder = if holder.is_some() {
                            holder
                        } else {
                            Self::probe_claim_best_effort(path).await
                        };
                        return Err(KvError::Busy(if holder.is_some() {
                            format!(
                                "{}: another squeezefs process holds the writer lock{} — \
                                 concurrent mounts of one metadata volume are refused \
                                 (single-writer guard)",
                                path.display(),
                                holder_suffix(&holder),
                            )
                        } else {
                            format!(
                                "{}: the writer lock is held by a process that left no \
                                 on-volume writer_claim and did not release within {}s — \
                                 squeezefs writers commit their claim within milliseconds \
                                 of taking the lock, so the holder is an external flock on \
                                 the device node (udevd's change-event probe releases \
                                 quickly; this one did not). Concurrent mounts of one \
                                 metadata volume are refused (single-writer guard)",
                                path.display(),
                                TRANSIENT_FLOCK_WAIT.as_secs(),
                            )
                        }));
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
        let mut inner = Self::open_inner(path, OpenPosture::Writer).await?;
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
        // KD-SYM-13 (design-symmetric-metadata §5.8.1 / §5.8.2): the
        // symmetric arm refuses a substrate that cannot fence — and a
        // detection-grade posture on one that can — without the loud
        // opt-in. Decided BEFORE the gate: nothing is registered yet.
        // A refusal drops `inner` — and with it the flock it carries.
        inner.meta_wero = inner.decide_meta_fence_posture()?;
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

        // (7) The appender JOIN (design-symmetric-metadata §5.3.2, PR 2):
        // every region of ours goes Live under this mount's identity in
        // one barriered cycle — a no-op on a flat volume. A refusal tears
        // down like the gate's.
        if let Err(e) = be
            .join_appender_regions(open_started.elapsed().as_millis() as u64)
            .await
        {
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
        let be = Arc::new(Self::open_inner(path, OpenPosture::NonWriter).await?);
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
        let mut inner = Self::open_inner(path, OpenPosture::NonWriter).await?;
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
        let mut inner = Self::open_inner(path, OpenPosture::NonWriter).await?;
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
    /// | Layer B2 claim classification | **evaluated, through the `PeerAuthority` arm**: where the volume HAS a claim it must be the ADMITTED holder's, resolved to a durable member id by the volume's own attestation — a claim from anyone else, or one nothing attests, refuses. A volume with NO claim opens DEGRADED (its owner is not up; nothing is adopted) — §5.1.1's `Peer` column |
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
        let mut inner = Self::open_inner(path, OpenPosture::NonWriter).await?;
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
            // **The DEGRADED open** (§5.1.1's corrected `Peer` +
            // `Reclaimable` row): nothing claims the volume, because its
            // owner has not started yet or has gone. This open takes no
            // lock, writes no claim and appends nowhere, so admitting
            // adopts nothing — it degrades exactly ONE owner's subtree,
            // which is the blast radius `docs/operations.md`'s "ownership
            // does not fail over" states. Refusing degraded the whole
            // namespace instead, and at a cold fleet start (no volume
            // carries a claim) it made an assigned set unmountable by any
            // node at all.
            ClaimEvidence::Reclaimable => {
                crate::fuse_client::METRICS
                    .peer_volume_unclaimed_admits
                    .fetch_add(1, Ordering::Relaxed);
                be.trace_guard_event("peer_owned_unclaimed_admitted");
                log::warn!(
                    "meta volume {} ({vol_id}): mounted PEER-OWNED and DEGRADED — the \
                     assignment names '{admitted_holder}' as its owner and NOTHING claims it, \
                     so that owner has not started yet or is down. This mount neither takes the \
                     claim nor appends here; every verb about this volume refuses loud at the \
                     ship site until its owner arrives (`meta_ship.volumes_peer_unclaimed`, \
                     `peer_volume_unclaimed_admits`; `squeezefs volume get-owners` prints \
                     assignment beside evidence). Guarantee class: {}",
                    path.display(),
                    be.writer_guard_mode()
                );
                Ok(be)
            }
            // A TTL-stale claim is evidence that something appended here,
            // so it must still ATTRIBUTE to the holder the admission
            // admitted (the same `recognizes` predicate the fresh arm
            // takes, which age does not change). Attributable ⇒ the owner
            // is down and this is the degraded open again — never a
            // preempt, since nothing here takes the claim.
            ClaimEvidence::StaleForeign(claim) => {
                if let Some(claim) = claim.as_ref().filter(|c| witness.recognizes(c)) {
                    crate::fuse_client::METRICS
                        .peer_volume_unclaimed_admits
                        .fetch_add(1, Ordering::Relaxed);
                    be.trace_guard_event("peer_owned_unclaimed_admitted");
                    log::warn!(
                        "meta volume {} ({vol_id}): mounted PEER-OWNED and DEGRADED — its owner \
                         '{admitted_holder}' left a TTL-stale writer_claim (id {}, era {}), so \
                         it is down or partitioned. This mount does NOT preempt it and appends \
                         nowhere here; the volume's verbs refuse loud at the ship site until \
                         that owner returns (`meta_ship.volumes_peer_unclaimed`). Guarantee \
                         class: {}",
                        path.display(),
                        claim.id,
                        claim.term,
                        be.writer_guard_mode()
                    );
                    return Ok(be);
                }
                crate::fuse_client::METRICS
                    .peer_volume_unclaimed_refusals
                    .fetch_add(1, Ordering::Relaxed);
                Err(KvError::Busy(format!(
                    "{} ({vol_id}): this volume carries a TTL-stale writer_claim{} that does \
                     NOT attribute to '{admitted_holder}', the owner the assignment names — no \
                     KD-PV-17 attestation binds it, or it binds another node. Something \
                     appended here that the record cannot account for, and silence about WHO \
                     is never resolved in a peer's favour (KD-PV-3): a volume with NO claim is \
                     the degraded not-yet-up state and mounts, this one does not. Verify the \
                     holder is gone and run `squeezefs claim clear`, or re-assign offline with \
                     `squeezefs volume set-owners` (`squeezefs volume get-owners` prints \
                     assignment beside evidence)",
                    path.display(),
                    holder_suffix(&claim.map(|c| (c, unix_now_secs())))
                )))
            }
        }
    }

    /// This volume's durable `vol-{hex}` identity — see
    /// [`durable_volume_id_of`], which the mount path calls over the
    /// discovery's uuids before any backend exists.
    pub fn durable_volume_id(&self) -> String {
        durable_volume_id_of(&self.sb.uuid)
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

    async fn open_inner(path: &Path, posture: OpenPosture) -> std::result::Result<Self, KvError> {
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
        if posture == OpenPosture::Writer
            && super::slot_lease::symmetric_meta_requested()
            && !sb.symmetric_forest_stamped()
        {
            return Err(KvError::Corrupt(format!(
                "{}: SQUEEZEFS_SYMMETRIC_META=1 but this volume is not symmetric-forest capable \
                 (incompat bit 17 absent) — run `squeezefs volume enable-symmetric <sqmeta-uri>` \
                 offline (design-symmetric-metadata §7.2, PR 11), or format it `--symmetric`; \
                 the plane arms nothing on a bit-17-absent volume",
                path.display()
            )));
        }
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
        // the checkpoint task's reclamation watermark. On a forest volume
        // the fixed extent's first four pages are appender 0's page slots
        // (design-symmetric-metadata §5.3.2) and its ring is the rest.
        let ring_extent = Self::fixed_ring_extent(&sb);
        let (ring, recovery) = JournalRing::recover(
            path,
            ring_extent.start,
            ring_extent.len / super::journal::JOURNAL_PAGE_LEN,
            checkpoint_reserve_bytes(ring_extent.len),
            ledger.journal_tail_seq,
        )
        .await?;
        // The seq-space law (§5.8.2): the window's own stamps attest the
        // offset in force (0 on every ring nothing was ever handed to —
        // the shipped ring's seqs stay its positions); appender 0's page
        // raises it further once read (`open_appender_regions`).
        ring.recover_seq_offset(super::journal::seq_offset_of_window(&recovery.entries));
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
        // The elision tail: the ledger's on a flat volume; on a forest
        // volume the MIN over every appender region's tail, set once the
        // regions are read (`open_appender_regions`) — a tombstone is
        // elidable only below its OWN ring's tail.
        if !sb.symmetric_forest_stamped() {
            cache.set_durable_tail(ledger.journal_tail_seq);
        }
        // Node-seq mint floor: the persisted watermark keeps mints
        // strictly above every seq ever stamped into a frame this
        // generation (Finding A — re-minted seqs made recycled-extent
        // residue admissible). The root/replay fetch_max floors below
        // stay as the crash-window belt-and-braces.
        let seq = Arc::new(AtomicU64::new(ledger.seq.max(ledger.node_seq_watermark)));

        // §4.11's unknown-ro bits degrade a writer's open to a non-writer
        // for the replay too: a mount that may not write mints nothing.
        let replay_posture = if sb.unknown_ro() != 0 {
            OpenPosture::NonWriter
        } else {
            posture
        };
        let (trees, max_replayed_ino, max_replayed_guest) = if sb.symmetric_forest_stamped() {
            Self::open_forest_and_replay(
                path,
                &sb,
                &ledger,
                &cache,
                &seq,
                &alloc,
                &recovery,
                replay_posture,
            )
            .await?
        } else {
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
                Arc::new(opened.next().expect("three trees")),
                Arc::new(opened.next().expect("three trees")),
                Arc::new(opened.next().expect("three trees")),
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
                        Some(Arc::new(tree))
                    }
                    None => {
                        log::info!(
                            "meta volume {}: incompat bit 8 (durable block refcounts) is \
                             stamped but the ledger names no block-reference root — minting \
                             an empty one (the post-stamp first mount)",
                            path.display()
                        );
                        let mut mint_ctx = SmoContext::new(alloc.clone());
                        Some(Arc::new(
                            KvTree::create(
                                cache.clone(),
                                &mut mint_ctx,
                                super::record::TREE_BLOCK_REFS,
                                seq.clone(),
                            )
                            .await?,
                        ))
                    }
                }
            } else {
                None
            };

            // 5a″. PB-class files, PR 1 (docs/design-kvmap-block-map-tree.md):
            // the block-map tree, under incompat bit 16 — the bit-9 arm's
            // discipline verbatim: a stamped volume whose ledger names no
            // tree-7 root mints one (idempotent across a crash — the claimed
            // extent's bitmap bit only becomes durable at a checkpoint), the
            // mint happens BEFORE replay so any map record still in the
            // journal window folds into the fresh root by key, and a
            // read-only-degraded mount (unknown-ro bits) never mints.
            let block_map = if sb.block_map_tree_stamped() && sb.unknown_ro() == 0 {
                match ledger
                    .tree_roots
                    .iter()
                    .find(|r| r.tree_id == super::record::TREE_BLOCK_MAP)
                {
                    Some(root) => {
                        let tree = KvTree::open(
                            cache.clone(),
                            super::record::TREE_BLOCK_MAP,
                            RootPtr {
                                addr: root.node_addr,
                                seq: root.node_seq,
                            },
                            seq.clone(),
                        )
                        .await?;
                        seq.fetch_max(root.node_seq, Ordering::AcqRel);
                        Some(Arc::new(tree))
                    }
                    None => {
                        log::info!(
                            "meta volume {}: incompat bit 16 (block-map tree) is stamped \
                             but the ledger names no block-map root — minting an empty \
                             one (the post-stamp first mount)",
                            path.display()
                        );
                        let mut mint_ctx = SmoContext::new(alloc.clone());
                        Some(Arc::new(
                            KvTree::create(
                                cache.clone(),
                                &mut mint_ctx,
                                super::record::TREE_BLOCK_MAP,
                                seq.clone(),
                            )
                            .await?,
                        ))
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
                        || (tree_id == super::record::TREE_BLOCK_REFS && block_refs.is_some())
                        || (tree_id == super::record::TREE_BLOCK_MAP && block_map.is_some());
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
                    super::record::TREE_BLOCK_MAP => block_map
                        .as_ref()
                        .expect("phase 1 collects the block-map tree only when it is mounted"),
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
                        // PB-class files, PR 1: map records replay like any
                        // other content record — routed by key into the
                        // (possibly freshly minted) block-map root. An
                        // un-engaged volume cannot have them.
                        super::record::TREE_BLOCK_MAP => match block_map.as_ref() {
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

            let trees = TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                block_refs,
                block_map: {
                    let cell = std::sync::OnceLock::new();
                    if let Some(t) = block_map {
                        let _ = cell.set(t);
                    }
                    cell
                },
            };
            (trees, max_replayed_ino, max_replayed_guest)
        };

        // 5b. The appender regions of a forest volume (design-symmetric-
        // metadata §5.3, PR 2): identity binding, own-residue recovery,
        // the declared regions' rings, the per-ring violation classes.
        // `None` on a flat volume — nothing of it exists there.
        let boot_id = read_boot_id();
        let appenders = match &trees {
            TreeSet::Forest { forest, .. } => Some(Arc::new(
                Self::open_appender_regions(
                    path,
                    &sb,
                    &ledger,
                    &ring,
                    forest,
                    &cache,
                    &seq,
                    &alloc,
                    &recovery,
                    replay_posture,
                    &boot_id,
                )
                .await?,
            )),
            TreeSet::Flat { .. } => None,
        };

        // 5b′. The roots are final: park the replayed in-window frees the
        // allocator load deferred — EXCEPT a free of a mounted root, the
        // unpublished root swap's retirement of the very node the mount
        // replays through (`ExtentAllocator::park_replayed_frees`; the
        // same law for every declared region's grant). Before any
        // post-mount SMO: the bring-up cover below is the first.
        {
            let roots = trees.root_extents(&cache);
            let (parked, dropped) = alloc.park_replayed_frees(&roots);
            let mut grant_dropped = 0usize;
            if let Some(set) = appenders.as_ref() {
                for r in set.regions.iter().skip(1) {
                    grant_dropped += r.grant().unpark_live_roots(&roots);
                }
            }
            if grant_dropped > 0 {
                super::META_KV_REPLAY_ROOT_FREES_DROPPED
                    .fetch_add(grant_dropped as u64, Ordering::Relaxed);
                log::warn!(
                    "meta volume {}: {grant_dropped} replayed grant free(s) named a LIVE slot-tree \
                     root — unpublished root swaps; the roots stay claimed \
                     (meta_kv_replay_root_frees_dropped)",
                    path.display()
                );
            }
            if parked > 0 || dropped > 0 || grant_dropped > 0 {
                log::info!(
                    "meta volume {}: replay parked {parked} in-window free(s), dropped {} of a \
                     live root",
                    path.display(),
                    dropped + grant_dropped as u64
                );
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
            let user_capacity = (ring_extent.len / super::journal::JOURNAL_PAGE_LEN
                * super::journal::JOURNAL_PAGE_DATA_LEN)
                .saturating_sub(checkpoint_reserve_bytes(ring_extent.len));
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
        let mut smo_ctx = SmoContext::with_journal(
            alloc.clone(),
            SmoJournal {
                ring: ring.clone(),
                retire_seq: retire_seq.clone(),
                sync: sync.clone(),
                path: path.to_path_buf(),
            },
        );
        // A forest's SMO context counts every slot tree's image claims and
        // retirements on the per-slot extent ledger (`slot_tree_extents`,
        // the affinity cap's input — PR 4).
        if let TreeSet::Forest { forest, .. } = &trees {
            smo_ctx.set_extent_ledger(Arc::clone(forest.extent_ledger()));
        }
        // A partitioned forest's SMOs scope themselves by slot (§5.2.3):
        // a leased slot tree's ring and grant are its lessee's. The
        // resolver holds the set weakly — the set outlives every SMO by
        // construction, and the backend owns both.
        if let Some(set) = appenders.as_ref().filter(|a| a.is_partitioned()) {
            let weak = Arc::downgrade(set);
            smo_ctx.set_region_resolver(Arc::new(move |slot| {
                let set = weak.upgrade()?;
                let id = set.region_of_slot(slot);
                if id == 0 {
                    return None;
                }
                let r = set.region(id)?;
                Some(super::tree::SmoRegion {
                    appender_id: id,
                    ring: r.ring(),
                    grant: Arc::clone(&r.grant),
                })
            }));
        }
        let smo = crate::sqz_sync::SqzMutex::new(smo_ctx);
        let be = Self {
            path: path.to_path_buf(),
            sb,
            // DUR-4's resume law, made true here: the bitmap's newest page
            // generation can exceed the ledger's seq — a failed cycle's
            // raise, a cycle that wrote its pages and died before its
            // ledger record, or (PR 2) the leave's consumed seq, which no
            // ledger record carries — and resuming from the ledger alone
            // made the first bitmap write of the next mount TIE that copy
            // and take the loud raise on every clean partitioned remount
            // (review round 2, Issue 18). Ledger slots are `seq % 32`, so
            // the gap the max opens is harmless; on a flat volume the two
            // agree except after a genuine failed-cycle raise.
            checkpoint_seq: AtomicU64::new(ledger.seq.max(alloc.resume_generation())),
            last_ledger_tail: AtomicU64::new(ledger.journal_tail_seq),
            // PR VL5a (§5.5.1a): seed the live stamp from the mounted
            // record — every checkpoint re-writes it, so a slot-mapped
            // volume's newest ledger slot always carries its membership.
            membership_stamp: std::sync::Mutex::new(ledger.membership_stamp.clone()),
            guest_cursors: scc::HashMap::new(),
            migration_tee: arc_swap::ArcSwapOption::empty(),
            ledger,
            trees,
            alloc,
            next_ino: AtomicU64::new(next_ino),
            era_ino_floor_native: next_ino,
            era_ino_floor_guest: std::sync::OnceLock::new(),
            lane_cursors: scc::HashMap::new(),
            lanes_live: AtomicBool::new(false),
            destroyed_inodes: AtomicU64::new(0),
            replay,
            ring,
            appenders,
            dlm: DlmLockManager::new(),
            sync,
            cache,
            strict,
            needs_flush: AtomicBool::new(false),
            read_only,
            non_writer: replay_posture == OpenPosture::NonWriter,
            ro_cause,
            failed: AtomicBool::new(false),
            journal_failures: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            layout_deltas_ok: AtomicBool::new(layout_deltas_stamped),
            layout_delta_ratchet: crate::sqz_sync::SqzMutex::new(()),
            block_map_ratchet: crate::sqz_sync::SqzMutex::new(()),
            crossing_inflight: scc::HashMap::new(),
            conveyor: Arc::new(ConveyorCore::new()),
            durability_lane: Arc::new(ConveyorCore::with_gauge(None)),
            journal_lane: std::sync::OnceLock::new(),
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
            manager_verbs: crate::sqz_sync::SqzMutex::new(()),
            handover: crate::sqz_sync::SqzMutex::new(()),
            slot_cadence_running: std::sync::atomic::AtomicBool::new(false),
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
            boot_id,
            claimed: AtomicBool::new(false),
            writer_term: AtomicU64::new(0),
            durable_term_enabled,
            reservations: None,
            pr_key: 0,
            meta_wero: false,
            pr_identity: std::sync::OnceLock::new(),
            pr_active: AtomicBool::new(false),
            guard_fenced: AtomicU64::new(0),
            pr_reacquires: AtomicU64::new(0),
            barrier_failures: AtomicU64::new(0),
            pending_free_stalled_cycles: AtomicU64::new(0),
            heap_full: AtomicBool::new(false),
            enospc_refusals: AtomicU64::new(0),
            heap_full_cycles: AtomicU64::new(0),
            merge_sweeps: AtomicU64::new(0),
            merge_candidates: AtomicU64::new(0),
            merge_backlog: AtomicBool::new(false),
            merge_sweep_budget_ms: super::checkpoint::merge_sweep_budget_ms(
                crate::meta_backend::resolve_flush_interval_ms(),
            ),
            merge_lap: std::sync::Mutex::new(VolumeLap::default()),
            merge_laps: AtomicU64::new(0),
            merge_candidates_tail: AtomicU64::new(0),
            merge_sweep_ns: AtomicU64::new(0),
            merge_sweep_projections: AtomicU64::new(0),
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

    /// The Appender family's snapshot (design-symmetric-metadata §11) —
    /// `None` on a bit-17-absent volume, where no region exists.
    pub fn appender_stats(&self) -> Option<super::appender::AppenderStats> {
        self.appenders.as_ref().map(|a| a.stats())
    }

    /// The appender set of a forest volume.
    pub(super) fn appenders(&self) -> Option<&Arc<super::appender::AppenderSet>> {
        self.appenders.as_ref()
    }

    /// The region that journals a slot tree's content: the declared
    /// lessee, else the manager (0). 0 on a flat volume.
    pub(super) fn region_of_slot(&self, slot: super::record::ForestSlot) -> u32 {
        self.appenders
            .as_ref()
            .filter(|a| a.is_partitioned())
            .map_or(0, |a| a.region_of_slot(slot))
    }

    /// The region a cached node's floor clamps and whose ring its SMOs
    /// journal into: a slot-stamped node's — leaf or interior — is its
    /// slot's lessee's (§5.2.3: every SMO of a slot tree is the owner's
    /// own, in its own ring, with a grant); tree 0 and every flat node
    /// are the manager's (ring 0).
    pub(super) fn region_of_node(&self, node: &CachedNode) -> u32 {
        node.forest_slot()
            .map_or(0, |slot| self.region_of_slot(slot))
    }

    /// The ring region `id` journals into (region 0's is the fixed ring).
    /// Public for the crash harness, like [`Self::journal_ring`]: a fault
    /// armed by ring position needs THAT ring's logical→physical map.
    pub fn ring_of_region(&self, id: u32) -> Arc<JournalRing> {
        self.appenders
            .as_ref()
            .and_then(|a| a.region(id))
            .map_or_else(|| Arc::clone(&self.ring), |r| r.ring())
    }

    /// The region that journals a staged tx: every slot-tree record's
    /// slot must resolve to ONE region (a tx spanning two appenders is a
    /// cross-owner operation — PR 6's intents — and refuses here); records
    /// of no slot (tree 0, allocator) are the manager's.
    fn region_of_records(&self, recs: &[(u8, Record)]) -> std::result::Result<u32, KvError> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Ok(0);
        };
        let mut region: Option<u32> = None;
        for (tag, r) in recs {
            let (kind, level) = untag(*tag);
            if level > 0 || !super::record::is_slot_tree_kind(kind) {
                continue;
            }
            let slot = super::record::forest_key_slot(&r.key)?;
            let this = set.region_of_slot(slot);
            match region {
                None => region = Some(this),
                Some(prev) if prev != this => {
                    return Err(KvError::Corrupt(format!(
                        "transaction spans appenders {prev} and {this} (slot {slot}) — a \
                         cross-appender mutation is a cross-owner operation \
                         (design-symmetric-metadata §5.6, PR 6), never one entry in two rings"
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(region.unwrap_or(0))
    }

    /// **The appender JOIN** (design-symmetric-metadata §5.3.2 identity
    /// binding; PR 2 — the writer open's last step before its tasks
    /// spawn): every region this mount holds goes `Live` under this
    /// mount's identity with a bumped term (a `Free` page's first join is
    /// term 1; an own `Live` / `Recovered` page re-adopts at term + 1),
    /// its segments named, and the join is made durable by ONE barriered
    /// checkpoint cycle — which also lands the bitmap bits of any ring
    /// extents claimed at open BEFORE a page names them. Refuses on a
    /// non-writer (a probe / reader never writes a page).
    pub(super) async fn join_appender_regions(
        &self,
        open_wall_ms: u64,
    ) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        // A writer open degraded to read-only (§4.11 unknown-ro bits)
        // joins nothing: it writes no page, as it writes nothing else.
        if self.read_only || self.non_writer {
            return Ok(());
        }
        // The refusal the open deferred (a foreign Live / a Recovering
        // page): the claim gate has run, so a live foreign holder was
        // already refused by D0's own message — what is left is a dead
        // appender's page, PR 10's.
        if let Some(why) = &set.join_refusal {
            return Err(KvError::Busy(why.clone()));
        }
        // §5.9's successor arm: page 0 `Live` under a FOREIGN node here
        // means the D0 ladder was WON over its holder (a fresh claim
        // refused at the gate; a stale one was preempted at the device
        // or attested clear) — the dead manager's ring, the fixed ring,
        // was replayed at step (2) of this open, before this first
        // control write (KD-SYM-3). The role passes with the page.
        if let Some(r0) = set.regions.first() {
            let foreign = {
                let page = r0.page.lock().unwrap_or_else(|e| e.into_inner());
                page.state == super::appender::AppenderState::Live
                    && !page.identity.owned_by_node(set.identity.node_token)
            };
            if foreign {
                let page = r0.page.lock().unwrap_or_else(|e| e.into_inner());
                log::warn!(
                    "meta volume {}: appender 0's page is LIVE under a foreign node ({:#018x}, \
                     term {}) whose writer_claim the D0 ladder took over — the manager role \
                     passes to this mount; the dead manager's ring was replayed before this \
                     first control write (design-symmetric-metadata §5.9 / KD-SYM-3; \
                     manager_lease: held)",
                    self.path.display(),
                    page.identity.node_token,
                    page.term
                );
            }
        }
        let writer_id = uuid::Uuid::parse_str(&self.writer_id)
            .map(|u| u.as_u128())
            .unwrap_or_else(|_| u128::from(xxhash_rust::xxh3::xxh3_64(self.writer_id.as_bytes())));
        for region in &set.regions {
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            page.term += 1;
            page.state = super::appender::AppenderState::Live;
            page.recovered_by_term = 0;
            page.identity = super::appender::AppenderIdentity {
                node_token: set.identity.node_token,
                mount_slot: set.identity.mount_slot,
                writer_id,
            };
            page.is_manager = region.id == 0;
            page.home_volume = 0;
            page.appender_id = region.id;
            set.joins.fetch_add(1, Ordering::Relaxed);
        }
        // The failover bound's two measured terms: the replay this open
        // paid and the rest of the open's wall (the ladder).
        let replay_ms = self.replay.replay_ms;
        set.failover_bound_ms.store(
            super::appender::manager_failover_bound_ms(
                crate::fuse_client::CLIENT_STALE_TTL_SECS,
                open_wall_ms.saturating_sub(replay_ms),
                replay_ms,
            ),
            Ordering::Release,
        );
        *set.manager_lease.lock().unwrap_or_else(|e| e.into_inner()) =
            super::appender::ManagerLease::Held;
        set.wero_meta.store(
            self.meta_wero && self.pr_active.load(Ordering::Acquire),
            Ordering::Release,
        );
        set.joined.store(true, Ordering::Release);
        // One barriered cycle: bitmap pages (the rings' extents), the
        // ledger, then every region's page in its Live form.
        self.checkpoint_now().await?;
        // `appenders_known`: the directory's Live count now that this
        // mount's pages are Live (foreign joiners included).
        let live = super::appender::read_directory(&self.path, &self.sb)
            .await?
            .iter()
            .filter(|e| {
                e.page
                    .as_ref()
                    .is_some_and(|p| p.state == super::appender::AppenderState::Live)
            })
            .count() as u64;
        set.appenders_known
            .store(live.max(set.regions.len() as u64), Ordering::Relaxed);
        // The initial grant of every declared region whose UNCLAIMED
        // remainder is empty (§5.3.3 — the join's grant): a recovered
        // region's claimed images (tree 0's record, review round 1 Issue
        // 1) are not a remainder to compact into — before the fix the
        // condition also required `claimed() == 0`, so a region recovered
        // with images and no remainder never got its grant.
        for r in set.regions.iter().skip(1) {
            if r.grant().unclaimed() == 0 {
                self.manager_extent_grant(r.id, 0).await?;
            }
        }
        // PR 4: the slot leases — native + rotor for the manager, the
        // seam's slots for every declared region; the S4 table; the gate.
        if let Some(plane) = set.slot_leases().cloned() {
            self.arm_slot_leases(set, &plane).await?;
        }
        Ok(())
    }

    /// **The appender LEAVE** (§5.1.3 region release, the clean-unmount
    /// arm): after the final checkpoint every region's page goes `Free`
    /// — its id and term kept (ids are stable for the volume's life) — and
    /// a declared region's ring extents return to the heap. Region 0
    /// keeps naming the fixed ring. The order is PAGES, barrier, RELEASE,
    /// BITMAP, barrier: the `Free` page is a table change, so it lands in
    /// BOTH directory slots (a torn one falls back to the other `Free`,
    /// never to a `Live` page naming extents about to be freed), and only
    /// once no durable page names the extents are their bits cleared and
    /// written — the release must reach the durable bitmap before the
    /// process exits, or every clean unmount leaks the ring (review round
    /// 1, Issue 2). The reverse order (release before the final
    /// checkpoint so its bitmap write carries it) would leave a `Live`
    /// page naming FREED extents across that checkpoint's barrier. A
    /// crash between the two barriers here leaves the extents claimed —
    /// the C13 orphan-extent class fsck reclaims, never a loss.
    pub(super) async fn leave_appender_regions(&self) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        if self.read_only
            || self.non_writer
            || self.is_failed()
            || !set.joined.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let node_size = u64::from(self.sb.node_size);
        // The belt (review round 2, Issue 16): a declared region whose
        // ring is UNCOVERED at the leave — its window holds records no
        // durable tail passed (the final fixpoint did not converge, or a
        // seam capped it) — is NOT released: a `Free` page over that
        // window would declare it covered and free the ring, and the next
        // open would route through the predecessor images (acked records
        // lost). Its page stays `Live` with its roots and tail, its ring
        // and grant stay claimed, and the next open of this identity
        // recovers it as own residue. Loud — this is the shutdown
        // guarantee missed, never a silent loss.
        // Region 0's ring is the fixed ring: its page names the roots of
        // the guest slots the manager holds (a moved root reaches tree 0
        // only through the next cycle), so the same law governs it. The
        // verdict is taken ONCE, here, before the leave's own control
        // entries (the grant returns below ride ring 0 — barriered,
        // root-free, replayed idempotently; they are not the window this
        // law guards).
        let uncovered_ids: std::collections::BTreeSet<u32> = set
            .regions
            .iter()
            .filter(|r| {
                let ring = r.ring();
                let core = ring.core();
                core.head() > core.reusable_upto()
            })
            .map(|r| r.id)
            .collect();
        let uncovered = |r: &super::appender::AppenderRegion| uncovered_ids.contains(&r.id);
        for region in set.regions.iter().filter(|r| uncovered(r)) {
            let ring = region.ring();
            log::warn!(
                "meta volume {}: appender {}'s ring is UNCOVERED at the leave (head {}, \
                 reusable_upto {}) — its page stays Live with its roots and its ring stays \
                 claimed; the next mount of this identity recovers the window as own residue \
                 (the shutdown fixpoint did not cover every ring)",
                self.path.display(),
                region.id,
                ring.core().head(),
                ring.core().reusable_upto()
            );
        }
        // §5.1.3 (PR 4): every slot a covered region leases is RELEASED
        // to tree 0 at the clean unmount — `Unleased { root, cursor, g,
        // extents, tails }` for each, one control entry per region — so
        // the next joiner's `prefer: unleased-then-idle` sees them and a
        // successor manager takes the native slot at `g + 1` (KD-SYM-2).
        // An uncovered region keeps its leases with its Live page.
        if let Some(plane) = set.slot_leases().cloned() {
            // The leave IS a release: under the handover mutex, so a
            // cadence handover in flight on its own task completes (or
            // the leave takes the mutex first and the cadence finds the
            // slot released) — never both on one slot.
            let _handover = self.handover.lock().await;
            for region in set.regions.iter().filter(|r| !uncovered(r)) {
                if region.released.load(Ordering::Acquire) {
                    continue;
                }
                self.release_leases_at_leave(set, &plane, region).await?;
            }
            // The membership carriage no longer answers for this plane.
            super::slot_lease::unregister_carriage_plane(&plane);
            if TEST_LEAVE_HOLD_AFTER_RELEASES.load(Ordering::Relaxed) {
                return Err(KvError::Busy(format!(
                    "{}: TEST_LEAVE_HOLD_AFTER_RELEASES — the mount died after its leases went \
                     Unleased and before its pages went Free",
                    self.path.display()
                )));
            }
        }
        // §5.1.3: a released region's UNCLAIMED grant (and every free its
        // tail already covered) returns to the heap — one control entry
        // per region, before its page goes Free.
        for region in set.regions.iter().skip(1).filter(|r| !uncovered(r)) {
            let mut back: Vec<u64> = {
                let mut g = region.grant();
                let mut v = g.take_returnable();
                v.extend(g.take_unclaimed());
                v
            };
            back.sort_unstable();
            back.dedup();
            if !back.is_empty() {
                if let Err(e) = self.return_extents_inner(region.id, &back, true).await {
                    log::warn!(
                        "meta volume {}: appender {}'s grant return at the leave failed ({e}) — \
                         the extents stay granted (fsck C13 reclaims)",
                        self.path.display(),
                        region.id
                    );
                }
            }
        }
        let mut released: Vec<super::superblock::ExtentRef> = Vec::new();
        for region in set
            .regions
            .iter()
            .filter(|r| !uncovered(r) && !r.released.load(Ordering::Acquire))
        {
            {
                let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
                page.state = super::appender::AppenderState::Free;
                page.slots.clear();
                page.grant.clear();
                if region.id != 0 {
                    released.append(&mut page.segments);
                    // The `Free` page KEEPS the ring's final head: the
                    // region's seq-space watermark, which the next carve
                    // continues from (`JournalRing::new_segments_at`) —
                    // record seqs are ring positions and per-key LWW
                    // compares them raw across incarnations.
                    let head = region.ring().core().head();
                    page.head_hint = head;
                    page.ledger_tail_seq = head;
                }
            }
            // A table change: the directory pair only — a released
            // region's ring-side pages sit in extents about to return to
            // the heap.
            region.dir_named.store(0, Ordering::Release);
            self.write_region_page(region).await?;
            set.leaves.fetch_add(1, Ordering::Relaxed);
        }
        crate::uring_fs::fdatasync(self.path.clone()).await?;
        if released.is_empty() {
            return Ok(());
        }
        // No durable page names the ring extents any more; no journal
        // record ever did (a ring is referenced by its page alone), so the
        // §4.7 pending-free gate has nothing to cover and the immediate
        // release is the right op. The bits ride their own bitmap write
        // and barrier before the process exits.
        for ext in &released {
            let mut off = ext.start;
            while off < ext.end() {
                self.alloc
                    .release_unpublished((off - self.sb.heap.start) / node_size);
                off += node_size;
            }
        }
        let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.alloc
            .write_dirty_pages(&self.path, self.sb.alloc_bitmap.start, ckpt_seq)
            .await?;
        crate::uring_fs::fdatasync(self.path.clone()).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // The slot-lease plane (design-symmetric-metadata §5.1 / §5.4.1 / §5.3.5,
    // PR 4): the manager's slot verbs (tree 0's `Leased` / `Unleased`
    // records through the ONE control entry), the holder's flush-then-
    // transfer, the mint policy, first-writer-takes-it at commit, the
    // cadence (offer expiry, LRU release, forced shrink, region release).
    // Every RAM transition is `slot_lease_core`'s; every durable act is
    // here. Armed by `SQUEEZEFS_SYMMETRIC_META=1` at a writer's open —
    // absent, every method below is a no-op / `None` and the PR 1–3
    // forest runs verbatim.
    // -----------------------------------------------------------------------

    /// The slot-lease plane, if ARMED — the join's last step raised the
    /// gate; before it (the open's own control commits — the D0 claim,
    /// the heartbeat) the mount is the PR 1–3 forest, every slot its own.
    pub fn slot_leases(&self) -> Option<&Arc<super::slot_lease::SlotLeasePlane>> {
        self.appenders
            .as_ref()
            .and_then(|a| a.slot_leases())
            .filter(|p| p.gate.is_armed())
    }

    /// Whether the symmetric plane is armed on this mount.
    pub fn slot_lease_armed(&self) -> bool {
        self.slot_leases().is_some()
    }

    /// Tree 0 (the control tree) of a forest volume — the contracts' and
    /// probes' read of `slot_state` / `extent_grant` records; `None` flat.
    pub fn forest_control_tree(&self) -> Option<Arc<KvTree>> {
        self.forest().map(|f| Arc::clone(f.control()))
    }

    /// The Slot-lease family (§11), `None` unarmed.
    pub fn slot_lease_stats(&self) -> Option<super::slot_lease::SlotLeaseStats> {
        let plane = self.slot_leases()?;
        let native = self.appenders.as_ref()?.native_slot;
        Some(plane.stats(u64::from(self.sb.node_size), &|slot| {
            match super::appender::page_slot_of_forest_slot(slot, native) {
                Ok(r) if r != native => self
                    .guest_cursor_snapshot(r)
                    .map_or(0, |c| c.saturating_sub(super::ino_lane::LOCAL_INO_BASE)),
                _ => self
                    .next_ino()
                    .saturating_sub(super::ino_lane::LOCAL_INO_BASE),
            }
        }))
    }

    /// The forest slot of routing slot `slot` on this volume.
    pub fn forest_slot_of_routing(&self, slot: u16) -> super::record::ForestSlot {
        let native = self.appenders.as_ref().map_or(0, |a| a.native_slot);
        super::appender::forest_slot_of_page_slot(slot, native)
    }

    /// The routing slot of forest slot `slot` on this volume.
    pub fn routing_slot_of_forest(
        &self,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<u16, KvError> {
        let native = self.appenders.as_ref().map_or(0, |a| a.native_slot);
        super::appender::page_slot_of_forest_slot(slot, native)
    }

    /// Every NON-NATIVE forest slot this volume hosts, ascending — the
    /// rotor's candidate set (a volume with no membership stamp hosts the
    /// whole width).
    fn hosted_rotor_candidates(&self) -> Box<dyn Iterator<Item = super::record::ForestSlot> + '_> {
        let native = self.appenders.as_ref().map_or(0, |a| a.native_slot);
        // A stamped volume's hosted set (cloned — small on a fleet volume);
        // an unstamped one hosts the whole width, walked LAZILY (Issue 15:
        // never a 65,536-entry vector per grant).
        let hosted: Option<Vec<u16>> = self
            .membership_stamp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|stamp| stamp.slots_hosted.iter().collect());
        match hosted {
            Some(v) => Box::new(
                v.into_iter()
                    .filter(move |r| *r != native)
                    .map(super::record::guest_forest_slot),
            ),
            None => Box::new(
                (0..=u16::MAX)
                    .filter(move |r| *r != native)
                    .map(super::record::guest_forest_slot),
            ),
        }
    }

    /// The manager's seq the table stamps releases with (`last_written`):
    /// ring 0's head — monotone for the volume's life.
    fn lease_seq(&self) -> u64 {
        self.ring.core().head()
    }

    /// The expiry of an offer made now: one renewal beat past it (§5.1.4)
    /// — `min(the shipped 10 s beat, T_idle / 3)`, the membership plane's
    /// own renewal law over the window in force.
    fn offer_expiry_ns(&self, plane: &super::slot_lease::SlotLeasePlane) -> u64 {
        let beat_ms = (crate::fuse_client::CLIENT_HEARTBEAT_INTERVAL_SECS * 1000)
            .min(plane.t_idle_ms / 3)
            .max(1);
        crate::mono_core::monotonic_ns_u64().saturating_add(beat_ms.saturating_mul(1_000_000))
    }

    /// The directory-slot-A offset of appender `id`'s page: an in-process
    /// region's, else read off the directory (a wire joiner's).
    async fn page_addr_of(&self, appender_id: u32) -> std::result::Result<u64, KvError> {
        if let Some(r) = self.appenders.as_ref().and_then(|a| a.region(appender_id)) {
            return Ok(r.page_offsets[0]);
        }
        let entries = super::appender::read_directory(&self.path, &self.sb).await?;
        entries
            .iter()
            .find(|e| e.appender_id == appender_id)
            .map(|e| e.dir_offsets[0])
            .ok_or_else(|| {
                KvError::Busy(format!(
                    "{}: appender {appender_id} has no page in the directory — join first \
                     (JoinAppender)",
                    self.path.display()
                ))
            })
    }

    /// The words a slot's tree carries right now, as this mount holds it.
    fn slot_words_now(
        &self,
        plane: &super::slot_lease::SlotLeasePlane,
        slot: super::record::ForestSlot,
    ) -> crate::slot_lease_core::SlotWords {
        let root = self
            .forest()
            .and_then(|f| f.tree(slot))
            .map_or((0, 0), |t| {
                let r = t.root();
                (r.addr, r.seq)
            });
        let cursor = self
            .routing_slot_of_forest(slot)
            .ok()
            .and_then(|r| self.guest_cursor_snapshot(r))
            .unwrap_or(0);
        // The seq-space floor (§5.8.2): the stamp frontier of the ring the
        // slot's records journal into — every record of the slot carries
        // a seq strictly below it — `max`ed with the floor the table
        // already carries (a slot's floor never regresses across leases).
        let ring = self
            .appenders
            .as_ref()
            .map(|set| set.region_of_slot(slot))
            .map_or_else(|| Arc::clone(&self.ring), |r| self.ring_of_region(r));
        let seq_floor = ring
            .seq_frontier()
            .max(plane.table.get(slot).map_or(0, |l| l.words.seq_floor));
        crate::slot_lease_core::SlotWords {
            root,
            cursor,
            extents: u32::try_from(plane.extents.get(slot)).unwrap_or(u32::MAX),
            seq_floor,
        }
    }

    /// Load tree 0's `slot_state` population into the plane's table (the
    /// manager's RAM mirror) — at the arm.
    async fn load_slot_leases(
        &self,
        plane: &super::slot_lease::SlotLeasePlane,
    ) -> std::result::Result<(), KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(());
        };
        let (mut cursor, end) = super::slot_state::slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                let slot = super::slot_state::decode_slot_state_key(k)?;
                let lease = match super::slot_state::SlotState::decode(v)? {
                    super::slot_state::SlotState::Unleased {
                        root,
                        cursor,
                        g,
                        slot_tree_extents,
                        last_written,
                        seq_floor,
                        ..
                    } => crate::slot_lease_core::SlotLease::unleased(
                        g,
                        last_written,
                        crate::slot_lease_core::SlotWords {
                            root: (root.addr, root.seq),
                            cursor,
                            extents: slot_tree_extents,
                            seq_floor,
                        },
                    ),
                    super::slot_state::SlotState::Leased {
                        appender_id,
                        g,
                        root,
                        cursor,
                        slot_tree_extents,
                        seq_floor,
                        ..
                    } => crate::slot_lease_core::SlotLease::leased_with(
                        appender_id,
                        g,
                        crate::slot_lease_core::SlotWords {
                            root: (root.addr, root.seq),
                            cursor,
                            extents: slot_tree_extents,
                            seq_floor,
                        },
                    ),
                };
                plane.table.load(slot, lease);
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(())
    }

    /// **The ARM** (§5.1.2, KD-SYM-2/3; the join's last step on a writer
    /// with `SQUEEZEFS_SYMMETRIC_META=1`): load tree 0's lease population,
    /// lease the native slot to the manager (KD-SYM-2), acquire the rotor
    /// (`AcquireSlots { want: M }`, `prefer: unleased-then-idle`), acquire
    /// every declared region's seam slots for it, publish the lease map
    /// into the S4 lock plane (`dlm_mode` = `slot-homed`) and arm the
    /// gate. A re-mount of this identity finds its slots `Leased` by its
    /// own appender id in tree 0 and re-adopts them (`Already`).
    async fn arm_slot_leases(
        &self,
        set: &super::appender::AppenderSet,
        plane: &Arc<super::slot_lease::SlotLeasePlane>,
    ) -> std::result::Result<(), KvError> {
        self.load_slot_leases(plane).await?;
        self.settle_page_entries_against_tree0(set, plane).await?;
        let m = plane.mint_slots();
        // Slots tree 0 already names as OURS (a crash-remount of this
        // identity): re-adopted without a write. Region 0's re-adopted
        // non-native slots fill its rotor up to `M` (tree 0 does not
        // distinguish a rotor slot from a handover-acquired one; the
        // rest are held non-rotor slots the page-budget LRU governs).
        for r in &set.regions {
            let mut readopted: Vec<super::record::ForestSlot> = Vec::new();
            for slot in plane.table.held_by(r.id) {
                // A re-adoption reads what the forest holds; the record's
                // seq floor is the one word the install must still carry
                // (the lessee's ring is raised above it again).
                let words = crate::slot_lease_core::SlotWords {
                    seq_floor: plane.table.get(slot).map_or(0, |l| l.words.seq_floor),
                    ..Default::default()
                };
                self.install_lease(set, plane, r.id, slot, words, false)
                    .await?;
                if r.id == 0 && slot != super::record::NATIVE_FOREST_SLOT {
                    readopted.push(slot);
                }
            }
            if r.id == 0 && !readopted.is_empty() {
                readopted.sort_unstable();
                readopted.truncate(m as usize);
                plane.rotor_update(|rotor| *rotor = readopted);
            }
        }
        // KD-SYM-2: the manager's native slot.
        self.manager_acquire_slots(
            0,
            0,
            &[super::record::NATIVE_FOREST_SLOT],
            ControlAdmit::Try,
        )
        .await?;
        // The seam's declared slots for every declared region — a wish-list
        // reconciled against tree 0 (unleased or already the region's;
        // another appender's holding is left to it) — BEFORE the manager's
        // rotor pick, which takes the unleased remainder.
        for r in set.regions.iter().skip(1) {
            let Some(declared) = plane.declared.get(&r.id) else {
                continue;
            };
            let wanted: Vec<super::record::ForestSlot> = declared
                .iter()
                .copied()
                .filter(|s| match plane.table.resolve(*s) {
                    crate::slot_lease_core::Resolved::Unleased { .. } => true,
                    crate::slot_lease_core::Resolved::Holder { holder, .. } => holder == r.id,
                })
                .collect();
            if wanted.len() < declared.len() {
                log::info!(
                    "meta volume {}: declared appender {}'s seam slots {:?} — {} held by another \
                     appender in tree 0, left to its holder",
                    self.path.display(),
                    r.id,
                    declared,
                    declared.len() - wanted.len()
                );
            }
            if !wanted.is_empty() {
                self.manager_acquire_slots(r.id, 0, &wanted, ControlAdmit::Try)
                    .await?;
            }
        }
        // The rotor: `M` slots for region 0.
        let rotor_now = plane.rotor.load().len() as u64;
        if rotor_now < m {
            let grants = self
                .manager_acquire_slots(0, (m - rotor_now) as u16, &[], ControlAdmit::Try)
                .await?;
            plane.rotor_update(|rotor| {
                for g in grants {
                    if !rotor.contains(&g.slot) {
                        rotor.push(g.slot);
                    }
                }
            });
        }
        self.publish_slot_owners(set, plane);
        plane.refresh_holders();
        // `N_floor`'s inputs from the mount's always-on EWMAs (§5.1.4's
        // cold start — Issue 7): a handover is priced before the first
        // one runs, a ship before the first one is served.
        plane.seed_n_floor_inputs();
        plane.gate.arm();
        // The membership carriage (§5.9): every member's renewal grant
        // now names the slots it leases here.
        super::slot_lease::register_carriage_plane(plane);
        log::info!(
            "meta volume {}: symmetric plane ARMED — {} slot(s) leased (native + {} rotor), \
             M = {m}, T_idle = {} ms, dlm_mode = {}",
            self.path.display(),
            plane.gate.leased_count(),
            plane.rotor.load().len(),
            plane.t_idle_ms,
            crate::dlm_slot::dlm_mode()
        );
        Ok(())
    }

    /// The §5.3.4 handover crash rows at the arm, and C14's live face
    /// (§5.8.5): every `Live` page's `Live` slot entries across the
    /// directory must name each slot ONCE at its CURRENT `g`, and a `Live`
    /// entry must agree with tree 0's lessee — two live attestations of
    /// one slot at tree 0's `g` REFUSE the mount loud
    /// (`slot_lease_conflicts`); a `Live` entry whose `g` is BELOW tree
    /// 0's for the slot is STALE residue (`g` is strictly monotone per
    /// slot and tree 0 is the durable witness — the leave-crash history of
    /// review round 2 Issue 8: released in tree 0, the page never went
    /// `Free`), dropped and counted (`slot_lease_stale_entries`), never a
    /// conflict; a `Releasing` entry never counts (tree 0 wins): on OUR
    /// page it is a handover this identity died inside — row 5 (tree 0
    /// still `Leased` by us at that `g`) COMPLETES the release from the
    /// page's words; row 6 (tree 0 already `Unleased` / another's) just
    /// drops it.
    async fn settle_page_entries_against_tree0(
        &self,
        set: &super::appender::AppenderSet,
        plane: &super::slot_lease::SlotLeasePlane,
    ) -> std::result::Result<(), KvError> {
        use super::appender::{AppenderState, SlotEntryState};
        // The attestations: OUR pages as LOADED (the join's checkpoint has
        // rewritten them since), every other Live page as the directory
        // holds it now.
        let loaded: std::collections::BTreeMap<u32, Vec<super::appender::SlotEntry>> =
            std::mem::take(
                &mut *plane
                    .loaded_page_entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
        let in_process: std::collections::BTreeSet<u32> =
            set.regions.iter().map(|r| r.id).collect();
        let mut attestations: Vec<(u32, Vec<super::appender::SlotEntry>)> = loaded
            .iter()
            .map(|(id, slots)| (*id, slots.clone()))
            .collect();
        for e in super::appender::read_directory(&self.path, &self.sb).await? {
            let Some(page) = e.page.as_ref() else {
                continue;
            };
            if page.state != AppenderState::Live || in_process.contains(&page.appender_id) {
                continue;
            }
            attestations.push((page.appender_id, page.slots.clone()));
        }
        let mut live_by_slot: std::collections::BTreeMap<super::record::ForestSlot, u32> =
            std::collections::BTreeMap::new();
        for (appender_id, slots) in &attestations {
            for se in slots {
                if se.state != SlotEntryState::Live {
                    continue;
                }
                let slot = super::appender::forest_slot_of_page_slot(se.slot, set.native_slot);
                // A `Live` attestation BELOW tree 0's `g` for the slot is
                // stale residue, not custody: it neither contends for the
                // slot nor contradicts the witness.
                if plane.table.get(slot).is_some_and(|l| l.g > se.g) {
                    plane.stale_entries.fetch_add(1, Ordering::Relaxed);
                    log::info!(
                        "meta volume {}: appender {appender_id}'s page attests slot {slot} LIVE \
                         at g {} below tree 0's g {} — stale residue of a leave or handover the \
                         page never recorded, dropped (slot_lease_stale_entries)",
                        self.path.display(),
                        se.g,
                        plane.table.get(slot).map_or(0, |l| l.g)
                    );
                    continue;
                }
                if let Some(other) = live_by_slot.insert(slot, *appender_id) {
                    if other != *appender_id {
                        plane.conflicts.fetch_add(1, Ordering::Relaxed);
                        return Err(KvError::Corrupt(format!(
                            "{}: slot {slot} is attested LIVE on two appender pages ({other} and \
                             {appender_id}) — a slot custody conflict (design-symmetric-metadata \
                             §5.8.5 C14; slot_lease_conflicts). Refusing the mount; the remedy \
                             is `squeezefs appender clear` (PR 10)",
                            self.path.display()
                        )));
                    }
                }
                if let Some(l) = plane.table.get(slot) {
                    // At tree 0's `g` (or ahead of it — a page the witness
                    // never caught up with) under ANOTHER lessee: the
                    // conflict class.
                    if l.state != crate::slot_lease_core::LeaseState::Unleased
                        && l.holder != *appender_id
                    {
                        plane.conflicts.fetch_add(1, Ordering::Relaxed);
                        return Err(KvError::Corrupt(format!(
                            "{}: slot {slot} is LIVE on appender {appender_id}'s page at g {} \
                             while tree 0 leases it to appender {} at g {} — a slot custody \
                             conflict (C14). Refusing the mount",
                            self.path.display(),
                            se.g,
                            l.holder,
                            l.g
                        )));
                    }
                    // OUR surviving attestation names the tree's durable
                    // extent count: the ledger is seeded from it, so the
                    // re-adoption walks only a slot no entry names (review
                    // round 2, Issue 17).
                    if in_process.contains(appender_id)
                        && l.holder == *appender_id
                        && se.slot_tree_extents != 0
                        && plane.extents.get(slot) == 0
                    {
                        plane.extents.set(slot, u64::from(se.slot_tree_extents));
                    }
                }
            }
        }
        // OUR pages' `Releasing` entries as loaded: complete or drop.
        for r in &set.regions {
            let Some(slots) = loaded.get(&r.id) else {
                continue;
            };
            for se in slots
                .iter()
                .filter(|se| se.state == SlotEntryState::Releasing)
            {
                let slot = super::appender::forest_slot_of_page_slot(se.slot, set.native_slot);
                match plane.table.get(slot) {
                    Some(l)
                        if l.state != crate::slot_lease_core::LeaseState::Unleased
                            && l.holder == r.id
                            && l.g == se.g =>
                    {
                        // Row 5: the release this identity died inside —
                        // tree 0 from the page's Releasing words.
                        let words = crate::slot_lease_core::SlotWords {
                            root: (se.root.addr, se.root.seq),
                            cursor: se.cursor,
                            extents: se.slot_tree_extents,
                            // The departing ring's frontier at this open
                            // bounds every stamp it ever made for the slot
                            // (the recovered head + offset never fall).
                            seq_floor: self
                                .ring_of_region(r.id)
                                .seq_frontier()
                                .max(l.words.seq_floor),
                        };
                        let tails = match self.forest().and_then(|f| f.tree(slot)) {
                            Some(t) => self.leaf_tails(&t).await?,
                            None => Vec::new(),
                        };
                        self.manager_release_slot(r.id, slot, words, se.g, tails)
                            .await?;
                        log::warn!(
                            "meta volume {}: appender {}'s page named slot {slot} RELEASING at g \
                             {} with tree 0 still leasing it — the handover this identity died \
                             inside is completed from the page (design-symmetric-metadata \
                             §5.3.4 row 5)",
                            self.path.display(),
                            r.id,
                            se.g
                        );
                    }
                    _ => {
                        // Row 6: tree 0 wins over a stale Releasing entry.
                        log::info!(
                            "meta volume {}: appender {}'s page named slot {slot} RELEASING at g \
                             {} — tree 0 no longer leases it to this appender; the entry is \
                             dropped (tree 0 wins, §5.3.4 row 6)",
                            self.path.display(),
                            r.id,
                            se.g
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Install the S4 lock-plane table from the lease map (§5.1.5 —
    /// `dlm_mode` = `slot-homed`): every routing slot is local except one
    /// a FOREIGN appender leases (whose token server is that holder). On
    /// a solo mount nothing is foreign, so `dlm_rpcs` stays 0 by
    /// construction while the mode reads `slot-homed`.
    fn publish_slot_owners(
        &self,
        set: &super::appender::AppenderSet,
        plane: &super::slot_lease::SlotLeasePlane,
    ) {
        let in_process: std::collections::BTreeSet<u32> =
            set.regions.iter().map(|r| r.id).collect();
        // The FOREIGN set off the holder index (O(leased)); this volume's
        // contribution to the process table is MERGED with the other
        // armed volumes' and S8's (review round 2, Issue 13 — the
        // wholesale store clobbered them).
        let foreign: Vec<u16> = plane
            .table
            .held_outside(&in_process)
            .into_iter()
            .filter_map(|s| self.routing_slot_of_forest(s).ok())
            .collect();
        crate::dlm_slot::install_lease_foreign_slots(self.volume_uuid(), Some(&foreign));
    }

    /// This volume's superblock uuid as one word — the process-global
    /// owner table's per-volume key.
    fn volume_uuid(&self) -> u128 {
        u128::from_le_bytes(self.sb.uuid)
    }

    /// Adopt a lease of `slot` for in-process region `region_id`: the
    /// gate bit, the region's lease set, the extent count, the cursor
    /// floor (§5.1.8 — the record's cursor is a floor the RAM cursor
    /// never drops below), the tree opened from the recorded root when
    /// this mount has never seen it. `fresh` = a grant made now (the
    /// words are the record's); a re-adoption reads what the forest holds.
    async fn install_lease(
        &self,
        set: &super::appender::AppenderSet,
        plane: &super::slot_lease::SlotLeasePlane,
        region_id: u32,
        slot: super::record::ForestSlot,
        words: crate::slot_lease_core::SlotWords,
        fresh: bool,
    ) -> std::result::Result<(), KvError> {
        let Some(region) = set.region(region_id) else {
            return Ok(());
        };
        // The seq-space law (§5.8.2): the lessee's ring stamps ABOVE every
        // record the slot already carries — raised at the grant and again
        // at every re-adoption (idempotent, `max`).
        if words.seq_floor != 0 {
            region.ring().raise_seq_floor(words.seq_floor);
        }
        if fresh && words.root.0 != 0 {
            if let Some(forest) = self.forest() {
                let root = RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                };
                match forest.tree(slot) {
                    Some(t) => {
                        if root.seq > t.root().seq {
                            t.adopt_root(root)?;
                        }
                    }
                    None => {
                        let tree = KvTree::open_slot_tree(
                            Arc::clone(&self.cache),
                            slot,
                            root,
                            self.seq_handle(),
                        )
                        .await?;
                        forest.adopt_guest(slot, Arc::new(tree));
                    }
                }
                forest.note_published(slot, root);
            }
        }
        if fresh && words.extents != 0 {
            plane.extents.set(slot, u64::from(words.extents));
        } else if plane.extents.get(slot) == 0 {
            if let Some(t) = self.forest().and_then(|f| f.tree(slot)) {
                // A tree this mount holds with no recorded count (a
                // PR 1–3 volume's): one paged walk seeds it.
                let n = t.reachable_node_addrs().await?.len() as u64;
                plane.extents.set(slot, n);
            }
        }
        if fresh && words.cursor != 0 {
            if let Ok(r) = self.routing_slot_of_forest(slot) {
                if slot != super::record::NATIVE_FOREST_SLOT {
                    self.install_guest_cursor(r, words.cursor);
                }
            }
        }
        // Every record the slot may already carry in the region's ring
        // (a re-adoption's replayed window, the grant's own control
        // entry) sits below the head: the frontier a release must cover.
        self.cache
            .note_slot_record_frontier(slot, region.ring().core().head());
        region.add_lease(slot);
        plane.gate.grant(slot);
        Ok(())
    }

    /// The manager's `AcquireSlots` / `AcquireSlot` executor for
    /// `appender_id` (§5.1.2 / §5.3.5): `explicit` names the slots (first-
    /// writer-takes-it, the native slot, a declared region's seam slots),
    /// else `want` rotor slots picked `prefer: unleased-then-idle` (0 =
    /// the derived `M`). Every grant is ONE tree-0 put `Leased { id, g,
    /// page_addr }` with `g` incremented; every put of the call rides ONE
    /// control entry. A slot the caller already holds answers `Already`
    /// (KD-SYM-7); a slot another appender holds is skipped for a rotor
    /// pick and REFUSED for an explicit ask. The manager refuses a rotor
    /// ask that would take the appender past `2 × M`.
    pub async fn manager_acquire_slots(
        &self,
        appender_id: u32,
        want: u16,
        explicit: &[super::record::ForestSlot],
        admit: ControlAdmit,
    ) -> std::result::Result<Vec<SlotGrant>, KvError> {
        let set = self.manager_gate(false)?;
        let plane = Arc::clone(set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed (SQUEEZEFS_SYMMETRIC_META=0) — no slot \
                 lease exists",
                self.path.display()
            ))
        })?);
        let _g = self.manager_verbs.lock().await;
        plane.acquires.fetch_add(1, Ordering::Relaxed);
        let now = crate::mono_core::monotonic_ns_u64();
        let m = plane.mint_slots();
        let rotor_ask = explicit.is_empty();
        let candidates: Vec<super::record::ForestSlot> = if rotor_ask {
            // The rotor cap counts the appender's ROTOR grants (review
            // round 2, Issue 14 — never every non-native lease: the page
            // budget governs holdings; the native slot is KD-SYM-2's).
            let held = plane.table.rotor_held_by(appender_id);
            let want = if want == 0 { m } else { u64::from(want) };
            if held.saturating_add(want) > 2 * m {
                plane.rotor_cap_refusals.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::RotorAtCap {
                    appender: appender_id,
                    held,
                    want,
                    cap: 2 * m,
                });
            }
            plane
                .table
                .pick_unleased(want as usize, self.hosted_rotor_candidates())
        } else {
            explicit.to_vec()
        };
        let page_addr = self.page_addr_of(appender_id).await?;
        let mut grants: Vec<SlotGrant> = Vec::with_capacity(candidates.len());
        let mut fresh: Vec<super::record::ForestSlot> = Vec::new();
        let mut puts: Vec<(u8, Record)> = Vec::new();
        let tag = super::journal::tag_for(super::record::TREE_CONTROL, 0);
        for slot in candidates {
            match plane.table.acquire(slot, appender_id, now, rotor_ask) {
                crate::slot_lease_core::AcquireOutcome::Granted { g, words } => {
                    let value = super::slot_state::SlotState::Leased {
                        appender_id,
                        g,
                        page_addr,
                        root: RootPtr {
                            addr: words.root.0,
                            seq: words.root.1,
                        },
                        cursor: words.cursor,
                        slot_tree_extents: words.extents,
                        seq_floor: words.seq_floor,
                    }
                    .encode()?;
                    puts.push((
                        tag,
                        Record::put(super::slot_state::slot_state_key(slot), 0, value),
                    ));
                    fresh.push(slot);
                    grants.push(SlotGrant {
                        slot,
                        g,
                        words,
                        already: false,
                    });
                }
                crate::slot_lease_core::AcquireOutcome::Already { g } => {
                    set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                    grants.push(SlotGrant {
                        slot,
                        g,
                        words: self.slot_words_now(&plane, slot),
                        already: true,
                    });
                }
                crate::slot_lease_core::AcquireOutcome::Refused { holder, g }
                | crate::slot_lease_core::AcquireOutcome::Recall { holder, g } => {
                    if !rotor_ask {
                        // Roll back the grants of this call that were not
                        // yet written: the RAM table must not run ahead
                        // of tree 0. The rolled-back entry keeps the `g`
                        // the grant minted (one ahead of tree 0's) and
                        // `last_written = 0` — harmless by construction:
                        // `g` only ever needs to be strictly monotone per
                        // slot (the next grant mints above it), and a
                        // never-written rank puts the slot first for
                        // `prefer: unleased-then-idle`, which is where a
                        // slot nobody wrote belongs (Issue 18).
                        for s in &fresh {
                            let l = plane.table.get(*s);
                            let _ = plane.table.release(
                                *s,
                                appender_id,
                                l.map_or(0, |l| l.g),
                                l.map_or_else(Default::default, |l| l.words),
                                0,
                            );
                        }
                        // The "ship to the holder" answer — §5.1.4's
                        // normal reply, never a witness contradiction
                        // (Issue 14: its own gauge).
                        plane.acquire_refusals.fetch_add(1, Ordering::Relaxed);
                        return Err(KvError::SlotBusy { slot, holder, g });
                    }
                }
            }
        }
        if !puts.is_empty() {
            // A fresh grant to a declared appender (never the manager,
            // whose images are untracked) claims the granted trees' live
            // images: its grant record gains them in the SAME entry (the
            // custody transfer's second half — `manager_release_slot` took
            // them off the departing grant), so an image it later retires
            // through its own context is one its grant claims.
            let mut arriving: Vec<u64> = Vec::new();
            if appender_id != 0 {
                for s in &fresh {
                    arriving.extend(self.slot_tree_image_extents(*s).await?);
                }
                arriving.sort_unstable();
                arriving.dedup();
                if !arriving.is_empty() {
                    let record = self.extent_grant_record(appender_id).await?;
                    let merged = super::slot_state::ExtentGrantRecord::from_extents(
                        record.extents().chain(arriving.iter().copied()),
                    );
                    puts.push((
                        tag,
                        Record::put(
                            super::slot_state::extent_grant_key(appender_id),
                            0,
                            merged.encode()?,
                        ),
                    ));
                }
            }
            if let Err(e) = self.write_control_entry(puts, admit).await {
                for s in &fresh {
                    let l = plane.table.get(*s);
                    let _ = plane.table.release(
                        *s,
                        appender_id,
                        l.map_or(0, |l| l.g),
                        l.map_or_else(Default::default, |l| l.words),
                        0,
                    );
                }
                return Err(e);
            }
            if let Some(r) = set.region(appender_id).filter(|_| !arriving.is_empty()) {
                r.grant().transfer_in(&arriving);
            }
            plane
                .grants
                .fetch_add(fresh.len() as u64, Ordering::Relaxed);
        }
        for g in &grants {
            if !g.already || set.region(appender_id).is_some() {
                self.install_lease(set, &plane, appender_id, g.slot, g.words, !g.already)
                    .await?;
            }
            if !g.already {
                // The holder cache learns the grant (one entry, never a
                // wholesale rebuild per grant — Issue 9/17).
                plane.holders.learn(
                    g.slot,
                    crate::slot_holder_cache::SlotHolder {
                        appender_id,
                        g: g.g,
                    },
                );
            }
        }
        if set.region(appender_id).is_none() && !fresh.is_empty() {
            self.write_wire_joiner_page_slots(appender_id, &plane)
                .await?;
            // A wire appender's grant makes the slots foreign to the S4
            // plane (an in-process region's leaves the table untouched).
            self.publish_slot_owners(set, &plane);
        }
        Ok(grants)
    }

    /// A WIRE joiner's page names its leased slots — the manager writes
    /// them (its in-process regions' pages are written at the checkpoint).
    async fn write_wire_joiner_page_slots(
        &self,
        appender_id: u32,
        plane: &super::slot_lease::SlotLeasePlane,
    ) -> std::result::Result<(), KvError> {
        let entries = super::appender::read_directory(&self.path, &self.sb).await?;
        let Some(e) = entries.iter().find(|e| e.appender_id == appender_id) else {
            return Ok(());
        };
        let Some(mut page) = e.page.clone() else {
            return Ok(());
        };
        let mut slots: Vec<super::appender::SlotEntry> = Vec::new();
        for slot in plane.table.held_by(appender_id) {
            let Ok(routing) = self.routing_slot_of_forest(slot) else {
                continue;
            };
            let lease = plane
                .table
                .get(slot)
                .unwrap_or_else(|| crate::slot_lease_core::SlotLease::leased(appender_id, 0));
            slots.push(super::appender::SlotEntry {
                slot: routing,
                state: super::appender::SlotEntryState::Live,
                g: lease.g,
                slot_tree_extents: lease.words.extents,
                root: RootPtr {
                    addr: lease.words.root.0,
                    seq: lease.words.root.1,
                },
                cursor: lease.words.cursor,
            });
        }
        slots.sort_by_key(|e| e.slot);
        slots.truncate(super::appender::SLOT_PAGE_BUDGET);
        page.slots = slots;
        for off in e.dir_offsets {
            page.generation += 1;
            super::appender::write_page(&self.path, off, page.encode()?).await?;
        }
        self.sync_device().await.map_err(KvError::Io)
    }

    /// The wire face of [`Self::manager_acquire_slots`].
    pub async fn manager_acquire_slots_wire(
        &self,
        appender_id: u32,
        want: u16,
    ) -> std::result::Result<(Vec<crate::meta_ship::manager::WireSlotGrant>, bool), KvError> {
        let grants = self
            .manager_acquire_slots(appender_id, want, &[], ControlAdmit::Try)
            .await?;
        let already = !grants.is_empty() && grants.iter().all(|g| g.already);
        let mut out = Vec::with_capacity(grants.len());
        for g in grants {
            out.push(crate::meta_ship::manager::WireSlotGrant {
                slot: self.routing_slot_of_forest(g.slot)?,
                g: g.g,
                words: g.words.into(),
            });
        }
        Ok((out, already))
    }

    /// `AcquireSlot` (§5.1.4): one named slot for `appender_id` — a grant
    /// if unleased, `Already` if the caller holds it, `Refused { holder }`
    /// if another does (ship to it), and the ACCEPT of an offer: the
    /// manager recalls the holder (flush-then-transfer, in-process) and
    /// grants the requester once the release landed (`g + 1`).
    pub async fn manager_acquire_slot(
        &self,
        appender_id: u32,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<AcquireSlotReply, KvError> {
        let set = self.manager_gate(false)?;
        let plane = Arc::clone(set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed — no slot lease exists",
                self.path.display()
            ))
        })?);
        let now = crate::mono_core::monotonic_ns_u64();
        let Some(lease) = plane.table.get(slot) else {
            let g = self.grant_one_slot(appender_id, slot).await?;
            return Ok(AcquireSlotReply::Granted(g));
        };
        use crate::slot_lease_core::LeaseState;
        match lease.state {
            LeaseState::Unleased => {
                let g = self.grant_one_slot(appender_id, slot).await?;
                Ok(AcquireSlotReply::Granted(g))
            }
            _ if lease.holder == appender_id => Ok(AcquireSlotReply::Already(SlotGrant {
                slot,
                g: lease.g,
                words: self.slot_words_now(&plane, slot),
                already: true,
            })),
            LeaseState::Offered
                if lease.offered_to == appender_id && now < lease.offer_expires_ns =>
            {
                // The accept. The holder is an in-process region: recalled
                // here (`RecallForOffer`). A WIRE holder serves no push
                // channel: the recall rides its next membership renewal
                // grant (`slot_release_notices`, §5.9's carriage), the
                // holder runs flush-then-transfer + `ReleaseSlot`, and the
                // requester's retry finds the slot unleased — so the
                // requester is answered `Refused { holder }` now (retry
                // after the release), never a refusal counted against the
                // manager.
                if set.region(lease.holder).is_none() {
                    plane.note_recall(lease.holder, slot);
                    return Ok(AcquireSlotReply::Refused {
                        holder: lease.holder,
                        g: lease.g,
                    });
                }
                // The release AND the grant under the one handover mutex:
                // a door parked on the slot wakes only once the requester
                // holds it, so it reads the requester as the holder
                // (`SlotBusy`) — never a first-touch re-acquire by the
                // departing holder in the window between the two.
                let t0 = std::time::Instant::now();
                let out = {
                    let _handover = self.handover.lock().await;
                    match self.release_slot_handover_locked(lease.holder, slot).await {
                        Ok(()) => {
                            let t_grant = std::time::Instant::now();
                            let g = self.grant_one_slot(appender_id, slot).await;
                            let grant_ns = t_grant.elapsed().as_nanos() as u64;
                            plane.phases.grant_ns.fetch_add(grant_ns, Ordering::Relaxed);
                            plane.phases.total_ns.fetch_add(grant_ns, Ordering::Relaxed);
                            g
                        }
                        Err(e) => Err(e),
                    }
                };
                plane.handover_done.notify_waiters();
                let g = out?;
                plane.handovers.fetch_add(1, Ordering::Relaxed);
                plane.fold_handover_ns(t0.elapsed().as_nanos() as u64);
                plane.note_handover(slot, crate::mono_core::monotonic_ns_u64());
                Ok(AcquireSlotReply::Granted(g))
            }
            LeaseState::Offered if now >= lease.offer_expires_ns => {
                plane.table.expire_offers(now);
                Ok(AcquireSlotReply::Refused {
                    holder: lease.holder,
                    g: lease.g,
                })
            }
            _ => Ok(AcquireSlotReply::Refused {
                holder: lease.holder,
                g: lease.g,
            }),
        }
    }

    /// [`Self::manager_acquire_slots`] for exactly one named slot.
    async fn grant_one_slot(
        &self,
        appender_id: u32,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<SlotGrant, KvError> {
        let mut grants = self
            .manager_acquire_slots(appender_id, 0, &[slot], ControlAdmit::Try)
            .await?;
        grants.pop().ok_or_else(|| {
            KvError::Corrupt(format!(
                "{}: AcquireSlot {slot} answered no grant",
                self.path.display()
            ))
        })
    }

    /// The wire face of [`Self::manager_acquire_slot`].
    pub async fn manager_acquire_slot_wire(
        &self,
        appender_id: u32,
        slot: u16,
    ) -> std::result::Result<crate::meta_ship::manager::ManagerReply, KvError> {
        use crate::meta_ship::manager::{ManagerReply, WireSlotGrant};
        let fslot = self.forest_slot_of_routing(slot);
        Ok(match self.manager_acquire_slot(appender_id, fslot).await? {
            AcquireSlotReply::Granted(g) | AcquireSlotReply::Already(g) => {
                ManagerReply::SlotsGranted {
                    slots: vec![WireSlotGrant {
                        slot,
                        g: g.g,
                        words: g.words.into(),
                    }],
                    already: g.already,
                }
            }
            AcquireSlotReply::Refused { holder, g } => {
                ManagerReply::SlotRefused { slot, holder, g }
            }
        })
    }

    /// `OfferSlot` (§5.1.4): the HOLDER `appender_id` offers `slot` to
    /// `to`, recorded in RAM at the manager until one renewal beat passes.
    pub async fn manager_offer_slot(
        &self,
        appender_id: u32,
        slot: super::record::ForestSlot,
        to: u32,
    ) -> std::result::Result<(), KvError> {
        let set = self.manager_gate(false)?;
        let plane = set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed — no slot lease exists",
                self.path.display()
            ))
        })?;
        let expires = self.offer_expiry_ns(plane);
        plane
            .table
            .offer(slot, appender_id, to, expires)
            .map_err(|e| {
                // A transition already in flight (a second dominating
                // ship before the accept, a recall) is the LEGAL busy
                // class (`slot_offers_busy`); only a caller that is not
                // the holder contradicts the witness (Issue 14).
                match e {
                    crate::slot_lease_core::LeaseRefusal::Busy { .. } => {
                        plane.offers_busy.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                    }
                }
                KvError::Busy(format!(
                    "{}: OfferSlot {slot} by appender {appender_id} refused: {e:?}",
                    self.path.display()
                ))
            })
    }

    /// The wire face of [`Self::manager_offer_slot`].
    pub async fn manager_offer_slot_wire(
        &self,
        appender_id: u32,
        slot: u16,
        to: u32,
    ) -> std::result::Result<(), KvError> {
        let fslot = self.forest_slot_of_routing(slot);
        self.manager_offer_slot(appender_id, fslot, to).await
    }

    /// `ReleaseSlot` (§5.1.4 / §5.3.5): the durable half of flush-then-
    /// transfer — tree 0 `slot_state:{s} → Unleased { root, cursor, g,
    /// extents, last_written, tails }` as ONE control entry (barriered),
    /// then the RAM table. `Ok(true)` = a replay (tree 0 already said so).
    pub async fn manager_release_slot(
        &self,
        appender_id: u32,
        slot: super::record::ForestSlot,
        words: crate::slot_lease_core::SlotWords,
        g: u32,
        tails: Vec<(u64, u32)>,
    ) -> std::result::Result<bool, KvError> {
        let set = self.manager_gate(true)?;
        let plane = Arc::clone(set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed — no slot lease exists",
                self.path.display()
            ))
        })?);
        let _guard = self.manager_verbs.lock().await;
        // The durable witness first — the TABLE's own law, consulted
        // BEFORE any write (review round 2, Issue 3: an `Unleased` at
        // another `g` or with other words fell through and REWROTE tree 0
        // with the caller's stale words before the table refused; the
        // verdict now precedes the durable act): `Unleased` at this `g`
        // with these words is the replay, every contradiction refuses.
        match plane.table.check_release(slot, appender_id, g, words) {
            crate::slot_lease_core::ReleaseOutcome::Already => {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                return Ok(true);
            }
            crate::slot_lease_core::ReleaseOutcome::Refused { holder, g: have } => {
                set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::Busy(format!(
                    "{}: ReleaseSlot {slot} by appender {appender_id} at g {g} refused — tree 0 \
                     names {} at g {have} (the durable witness contradicts the caller)",
                    self.path.display(),
                    if holder == 0 {
                        "no holder (Unleased)".to_string()
                    } else {
                        format!("appender {holder}")
                    }
                )));
            }
            crate::slot_lease_core::ReleaseOutcome::Released => {}
        }
        let last_written = self.lease_seq();
        let value = super::slot_state::SlotState::Unleased {
            root: RootPtr {
                addr: words.root.0,
                seq: words.root.1,
            },
            cursor: words.cursor,
            g,
            slot_tree_extents: words.extents,
            last_written,
            seq_floor: words.seq_floor,
            tails,
        }
        .encode()?;
        let tag = super::journal::tag_for(super::record::TREE_CONTROL, 0);
        let mut recs = vec![(
            tag,
            Record::put(super::slot_state::slot_state_key(slot), 0, value),
        )];
        // The custody of the tree's live images leaves the departing
        // grant with the slot (C13's candidate set is the grant-claimed
        // unreachable extents — an image the requester later retires
        // through ITS context must not stay claimed here): the record is
        // rewritten without them in the SAME entry, bits untouched.
        let (rewrite, leaving) = self.grant_record_minus_images(appender_id, &[slot]).await?;
        recs.extend(rewrite);
        self.write_control_entry(recs, ControlAdmit::Try).await?;
        if let Some(r) = set.region(appender_id).filter(|_| !leaving.is_empty()) {
            r.grant().transfer_out(&leaving);
        }
        plane.clear_recall(appender_id, slot);
        plane.holders.forget(slot);
        match plane
            .table
            .release(slot, appender_id, g, words, last_written)
        {
            crate::slot_lease_core::ReleaseOutcome::Released => {}
            crate::slot_lease_core::ReleaseOutcome::Already => {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            }
            crate::slot_lease_core::ReleaseOutcome::Refused { holder, g: have } => {
                // Unreachable behind the witness check above — a race the
                // verb mutex excludes; loud anyway.
                set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::Corrupt(format!(
                    "{}: ReleaseSlot {slot} landed in tree 0 but the table names appender \
                     {holder} at g {have}",
                    self.path.display()
                )));
            }
        }
        if let Some(forest) = self.forest() {
            if words.root.0 != 0 {
                forest.note_published(
                    slot,
                    RootPtr {
                        addr: words.root.0,
                        seq: words.root.1,
                    },
                );
            }
        }
        if set.region(appender_id).is_none() {
            self.write_wire_joiner_page_slots(appender_id, &plane)
                .await?;
            // A wire appender's release returns the slot to the S4
            // plane's local set (an in-process region's changes nothing).
            self.publish_slot_owners(set, &plane);
        }
        Ok(false)
    }

    /// The wire face of [`Self::manager_release_slot`].
    pub async fn manager_release_slot_wire(
        &self,
        appender_id: u32,
        slot: u16,
        g: u32,
        words: crate::meta_ship::manager::WireSlotWords,
        tails: &[(u64, u32)],
    ) -> std::result::Result<bool, KvError> {
        if tails.len() > usize::from(u16::MAX) {
            return Err(KvError::Rejected(format!(
                "ReleaseSlot names {} tails — the record's count is a u16",
                tails.len()
            )));
        }
        let fslot = self.forest_slot_of_routing(slot);
        self.manager_release_slot(appender_id, fslot, words.into(), g, tails.to_vec())
            .await
    }

    /// `ResolveSlot` (§5.1.6): the holder of `slot` as the manager's
    /// table records it.
    pub fn manager_resolve_slot(
        &self,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<crate::slot_lease_core::Resolved, KvError> {
        let set = self.manager_gate(false)?;
        let plane = set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed — no slot lease exists",
                self.path.display()
            ))
        })?;
        // The served verb IS the stale-view fallback's round trip
        // (`slot_resolve_rpcs` — never `dlm_rpcs`, which keeps its S4
        // meaning); a mount whose holder cache is warm never issues one.
        plane.resolve_rpcs.fetch_add(1, Ordering::Relaxed);
        Ok(plane.table.resolve(slot))
    }

    /// The wire face of [`Self::manager_resolve_slot`].
    pub fn manager_resolve_slot_wire(
        &self,
        slot: u16,
    ) -> std::result::Result<crate::meta_ship::manager::ManagerReply, KvError> {
        use crate::meta_ship::manager::ManagerReply;
        let fslot = self.forest_slot_of_routing(slot);
        Ok(match self.manager_resolve_slot(fslot)? {
            crate::slot_lease_core::Resolved::Unleased { g } => ManagerReply::Unleased { g },
            crate::slot_lease_core::Resolved::Holder { holder, g } => ManagerReply::Holder {
                appender_id: holder,
                g,
            },
        })
    }

    /// The `(leaf addr, log tail)` of every leaf `tree` reaches — what a
    /// release records for PR 5's frame screen (§5.8.2).
    /// COMPLETE over the tree (review round 2, Issue 12): a leaf the node
    /// cache evicted since its flush — or never loaded, a re-adopted
    /// tree's — is paged in for its tail; the record's consumer reads
    /// every leaf, never a cached subset.
    async fn leaf_tails(&self, tree: &KvTree) -> std::result::Result<Vec<(u64, u32)>, KvError> {
        let mut out = Vec::new();
        for addr in tree.reachable_node_addrs().await? {
            let node = self.cache.get(addr).await?;
            if node.level() != 0 {
                continue;
            }
            let tail = node.lock().read().await.tail_offset();
            out.push((addr, u32::try_from(tail).unwrap_or(u32::MAX)));
        }
        Ok(out)
    }

    /// The page entries of in-process region `region` under the armed
    /// plane: every slot it leases with its `g`, extent count, root and
    /// cursor (`releasing` = the one entry mid-handover, in `Releasing`).
    fn lease_page_entries(
        &self,
        set: &super::appender::AppenderSet,
        plane: &super::slot_lease::SlotLeasePlane,
        region: &super::appender::AppenderRegion,
        releasing: &[super::record::ForestSlot],
    ) -> Vec<super::appender::SlotEntry> {
        let mut entries: Vec<super::appender::SlotEntry> = Vec::new();
        for slot in region.leases().iter().copied() {
            let Ok(page_slot) = super::appender::page_slot_of_forest_slot(slot, set.native_slot)
            else {
                continue;
            };
            let words = self.slot_words_now(plane, slot);
            let g = plane.table.get(slot).map_or(0, |l| l.g);
            entries.push(super::appender::SlotEntry {
                slot: page_slot,
                state: if releasing.contains(&slot) {
                    super::appender::SlotEntryState::Releasing
                } else {
                    super::appender::SlotEntryState::Live
                },
                g,
                slot_tree_extents: words.extents,
                root: RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                },
                cursor: words.cursor,
            });
        }
        entries.sort_by_key(|e| e.slot);
        entries.truncate(super::appender::SLOT_PAGE_BUDGET);
        entries
    }

    /// The slots of `region` its page CANNOT name — the leases past
    /// `SLOT_PAGE_BUDGET` in the page's own order (`lease_page_entries`
    /// sorts by page slot and truncates); their roots ride tree 0 (the
    /// page-budget overflow law, `publish_forest_roots`).
    fn region_page_overflow(
        set: &super::appender::AppenderSet,
        region: &super::appender::AppenderRegion,
    ) -> Vec<super::record::ForestSlot> {
        let leases = region.leases();
        if leases.len() <= super::appender::SLOT_PAGE_BUDGET {
            return Vec::new();
        }
        let mut by_page: Vec<(u16, super::record::ForestSlot)> = leases
            .iter()
            .filter_map(|s| {
                super::appender::page_slot_of_forest_slot(*s, set.native_slot)
                    .ok()
                    .map(|p| (p, *s))
            })
            .collect();
        by_page.sort_unstable();
        by_page
            .into_iter()
            .skip(super::appender::SLOT_PAGE_BUDGET)
            .map(|(_, s)| s)
            .collect()
    }

    /// **Flush-then-transfer** (KD-SYM-4, §5.1.4) of `slot` held by
    /// in-process region `region_id` — the public face: one handover at a
    /// time per volume (the `handover` mutex), the door's parkers woken
    /// when it completes or aborts. The accept path runs the same body
    /// under its own hold of the mutex so the requester's grant lands
    /// before any parker re-reads the slot.
    pub async fn release_slot_handover(
        &self,
        region_id: u32,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<(), KvError> {
        let out = {
            // One handover at a time per volume: a concurrent release of
            // the same slot (the cadence against an accepted offer) finds
            // it already released and is refused here, never at tree 0's
            // witness.
            let _handover = self.handover.lock().await;
            self.release_slot_handover_locked(region_id, slot).await
        };
        if let Some(plane) = self.slot_leases() {
            plane.handover_done.notify_waiters();
        }
        out
    }

    /// Cycle the volume's checkpoint until region `region`'s durable tail
    /// covers every record of `slot` (its record frontier — the door is
    /// closed and drained, so only the flush pass's own SMOs on the tree
    /// can add to it, and each of those raises the frontier the next
    /// cycle must pass) AND the tree's root is published (its floor
    /// lifted). The `checkpoint_past_region` discipline (review round 2,
    /// Issue 11): a post-condition, not a fixed cycle count — bounded,
    /// loud on a stuck tail. `slot_handover_phase_ns.flush` then measures
    /// what the clearing costs.
    async fn flush_slot_clear_of_region(
        &self,
        region: &super::appender::AppenderRegion,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<(), KvError> {
        const HANDOVER_FLUSH_CYCLES_MAX: u32 = 64;
        for cycle in 1..=HANDOVER_FLUSH_CYCLES_MAX {
            self.checkpoint_now().await?;
            if let Some(forest) = self.forest() {
                if let Some(tree) = forest.tree(slot) {
                    forest.note_page_published(slot, tree.root());
                }
            }
            let tail = region.ring().core().reusable_upto();
            let frontier = self.cache.slot_record_frontier(slot);
            let root_unpublished = self.unpublished_root_floors().contains_key(&slot);
            if tail >= frontier && !root_unpublished {
                if cycle > 2 {
                    log::info!(
                        "meta volume {}: slot {slot}'s records cleared from appender {}'s window \
                         after {cycle} checkpoint cycles (frontier {frontier}, tail {tail})",
                        self.path.display(),
                        region.id
                    );
                }
                return Ok(());
            }
        }
        Err(KvError::Corrupt(format!(
            "{}: slot {slot}'s records did not clear appender {}'s window in \
             {HANDOVER_FLUSH_CYCLES_MAX} checkpoint cycles (frontier {}, tail {}) — a stuck \
             tail is a defect, never a longer wait",
            self.path.display(),
            region.id,
            self.cache.slot_record_frontier(slot),
            region.ring().core().reusable_upto()
        )))
    }

    /// The handover body, under the caller's hold of `handover`, in the
    /// exact order: `Releasing` raised on the gate (no new commit passes
    /// the door) → the door DRAINED (every admitted commit of the slot at
    /// its terminal outcome — Issue 6) → flush cycles until the region's
    /// window is clear of the slot (Issue 11) → the page with the slot in
    /// `Releasing { root, cursor, g }` + barrier → `ReleaseSlot` (tree 0
    /// `Unleased`, one tx, barriered) → the page without the slot. Two
    /// durable homes at every instant; `slot_handover_phase_ns` records
    /// `flush / page / tree0`.
    async fn release_slot_handover_locked(
        &self,
        region_id: u32,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<(), KvError> {
        let set = self.manager_gate(true)?;
        let plane = Arc::clone(set.slot_leases().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the symmetric plane is not armed — no slot lease exists",
                self.path.display()
            ))
        })?);
        let region = set.region(region_id).ok_or_else(|| {
            KvError::Busy(format!(
                "{}: appender {region_id} is not an in-process region",
                self.path.display()
            ))
        })?;
        if slot == super::record::NATIVE_FOREST_SLOT && region_id == 0 {
            return Err(KvError::Busy(format!(
                "{}: the manager's native slot is non-transferable while it holds the role \
                 (KD-SYM-2)",
                self.path.display()
            )));
        }
        // 1. Releasing FIRST: the gate stops new commits at the door
        // before the flush takes any node lock (the loom-pinned order),
        // then the door is drained — the release's half of the Dekker
        // pair with `LeaseGate::enter`.
        let g = match plane.table.get(slot) {
            Some(l)
                if l.holder == region_id
                    && l.state != crate::slot_lease_core::LeaseState::Unleased
                    && l.state != crate::slot_lease_core::LeaseState::Releasing =>
            {
                plane.gate.begin_release(slot);
                match plane.table.begin_release(slot, region_id) {
                    Ok(g) => g,
                    Err(e) => {
                        plane.gate.end_release(slot);
                        return Err(KvError::Busy(format!(
                            "{}: release of slot {slot} by appender {region_id} refused: {e:?}",
                            self.path.display()
                        )));
                    }
                }
            }
            other => {
                return Err(KvError::Busy(format!(
                    "{}: release of slot {slot} by appender {region_id} refused — the slot is \
                     {} (a concurrent release or transfer took it)",
                    self.path.display(),
                    other.map_or_else(
                        || "unknown to tree 0".to_string(),
                        |l| format!("{:?} under appender {} at g {}", l.state, l.holder, l.g)
                    )
                )));
            }
        };
        let abort = |e: KvError| {
            plane.table.abort_release(slot, region_id);
            plane.gate.end_release(slot);
            e
        };
        let t_flush = std::time::Instant::now();
        self.drain_door(&plane, slot).await;
        // 2. Flush until the departing ring's window is CLEAR of the slot
        // before any other home says it moved (KD-SYM-4's one ring per
        // key; the `Lease` detector at the next open judges the window by
        // tree 0's lessee).
        if let Err(e) = self.flush_slot_clear_of_region(region, slot).await {
            return Err(abort(e));
        }
        let flush_ns = t_flush.elapsed().as_nanos() as u64;
        let words = self.slot_words_now(&plane, slot);
        let tails = match self.forest().and_then(|f| f.tree(slot)) {
            Some(t) => match self.leaf_tails(&t).await {
                Ok(t) => t,
                Err(e) => return Err(abort(e)),
            },
            None => Vec::new(),
        };
        // 3. The page: the slot in `Releasing` with its final words.
        let t_page = std::time::Instant::now();
        {
            let entries = self.lease_page_entries(set, &plane, region, &[slot]);
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            page.slots = entries;
        }
        self.write_region_page(region).await?;
        self.sync_device().await.map_err(KvError::Io)?;
        let page_ns = t_page.elapsed().as_nanos() as u64;
        if TEST_HANDOVER_HOLD_AFTER_PAGE.load(Ordering::Relaxed) {
            return Err(KvError::Busy(format!(
                "{}: TEST_HANDOVER_HOLD_AFTER_PAGE — the holder died after its page named slot \
                 {slot} Releasing and before tree 0 was written",
                self.path.display()
            )));
        }
        // Test seam: PARK here (the page names the slot Releasing, tree 0
        // does not yet) until released — the window the door contract
        // issues its commits into.
        if TEST_HANDOVER_PARK_AFTER_PAGE.load(Ordering::Relaxed) {
            TEST_HANDOVER_PARKED.fetch_add(1, Ordering::AcqRel);
            while TEST_HANDOVER_PARK_AFTER_PAGE.load(Ordering::Relaxed) {
                let notified = TEST_HANDOVER_PARK_NOTIFY.notified();
                if !TEST_HANDOVER_PARK_AFTER_PAGE.load(Ordering::Relaxed) {
                    break;
                }
                notified.await;
            }
        }
        // 4. Tree 0: Unleased { root, cursor, g, extents, tails } — one tx.
        let t_tree0 = std::time::Instant::now();
        if let Err(e) = self
            .manager_release_slot(region_id, slot, words, g, tails)
            .await
        {
            return Err(abort(e));
        }
        let tree0_ns = t_tree0.elapsed().as_nanos() as u64;
        if TEST_HANDOVER_HOLD_AFTER_TREE0.load(Ordering::Relaxed) {
            return Err(KvError::Busy(format!(
                "{}: TEST_HANDOVER_HOLD_AFTER_TREE0 — the holder died after tree 0 recorded slot \
                 {slot} Unleased and before its page dropped the entry",
                self.path.display()
            )));
        }
        // 5. Drop the slot: RAM first (the region no longer journals it),
        // then the page without it. The lease set's and the rotor's RMWs
        // run under their one-writer mutexes (Issue 5).
        region.drop_lease(slot);
        plane.gate.revoke(slot);
        plane.dominance.forget(slot);
        plane.extents.forget(slot);
        plane.rotor_update(|rotor| rotor.retain(|s| *s != slot));
        if let Ok(r) = self.routing_slot_of_forest(slot) {
            if slot != super::record::NATIVE_FOREST_SLOT {
                self.remove_guest_cursor(r);
            }
        }
        {
            let entries = self.lease_page_entries(set, &plane, region, &[]);
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            page.slots = entries;
        }
        self.write_region_page(region).await?;
        plane.phases.record(flush_ns, page_ns, tree0_ns, 0);
        log::info!(
            "meta volume {}: slot {slot} released by appender {region_id} (g {g}, root {:#x}, \
             cursor {}, {} extents) — flush {} µs, page {} µs, tree 0 {} µs",
            self.path.display(),
            words.root.0,
            words.cursor,
            words.extents,
            flush_ns / 1000,
            page_ns / 1000,
            tree0_ns / 1000
        );
        Ok(())
    }

    /// The holder's dominance evaluation at one served SHIP of `slot` by
    /// `requester` (§5.1.4): count it, and when ONE requester dominates
    /// over the common window — `ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor`
    /// — offer the slot to it (`slot_offers`, idle / dominated). `Serve`
    /// on every other ship, on an unarmed mount, and on a slot this
    /// mount does not hold. `requester` is the appender id of the
    /// shipping mount (the offer's `to`).
    ///
    /// `ship_ns` is the served ship's MEASURED wall (the S8 owner-side
    /// `meta_ship_owner_phase_ns.total` of the frame that carried it —
    /// the caller's clock; `0` = unmeasured, nothing folded): it feeds
    /// `N_floor`'s ship-cost EWMA (review round 2, Issue 7 — before the
    /// feed, `ewma_ship_ns` stayed 0 for the mount's life and one
    /// handover made `N_floor` its wall in nanoseconds).
    pub async fn note_slot_ship(
        &self,
        slot: super::record::ForestSlot,
        requester: u32,
        ship_ns: u64,
    ) -> crate::slot_lease_core::ShipVerdict {
        use crate::slot_lease_core::ShipVerdict;
        let Some(set) = self.appenders.as_ref() else {
            return ShipVerdict::Serve;
        };
        let Some(plane) = self.slot_leases() else {
            return ShipVerdict::Serve;
        };
        if !plane.gate.is_leased(slot) {
            return ShipVerdict::Serve;
        }
        if ship_ns != 0 {
            plane.fold_ship_ns(ship_ns);
        }
        let holder = set.region_of_slot(slot);
        plane.ships.fetch_add(1, Ordering::Relaxed);
        let now = crate::mono_core::monotonic_ns_u64();
        let t_idle = plane.t_idle_ns();
        let ops_h = plane.holder_ops(slot, now);
        let verdict = plane.dominance.note_ship(
            slot,
            u64::from(requester),
            ops_h,
            now,
            t_idle,
            plane.n_floor(),
        );
        match verdict {
            ShipVerdict::Serve => {}
            ShipVerdict::OfferIdle { .. } | ShipVerdict::OfferDominated { .. } => {
                // The requester-side cooldown (§5.1.4 — the S10 never-
                // thrash valve): a slot handed over inside the window is
                // served, never re-offered, so an alternating pair
                // converges on one holder.
                if plane.in_cooldown(slot, now) {
                    return ShipVerdict::Serve;
                }
                // Under the manager's mutex: an offer already in flight (a
                // second dominating ship before the accept) is `Busy` and
                // not a second offer.
                if let Ok(()) = self.manager_offer_slot(holder, slot, requester).await {
                    if matches!(verdict, ShipVerdict::OfferIdle { .. }) {
                        plane.offers_idle.fetch_add(1, Ordering::Relaxed);
                    } else {
                        plane.offers_dominated.fetch_add(1, Ordering::Relaxed);
                    }
                    return verdict;
                }
                return ShipVerdict::Serve;
            }
        }
        verdict
    }

    /// One own commit's slots noted on the holder's window (called per
    /// staged tx on an armed mount — a lock-free bump per distinct slot).
    fn note_slot_ops(&self, recs: &[(u8, Record)]) {
        let Some(plane) = self.slot_leases() else {
            return;
        };
        let now = crate::mono_core::monotonic_ns_u64();
        let t_idle = plane.t_idle_ns();
        let mut last: Option<super::record::ForestSlot> = None;
        for (tag, r) in recs {
            let (kind, level) = untag(*tag);
            if level > 0 || !super::record::is_slot_tree_kind(kind) {
                continue;
            }
            let Ok(slot) = super::record::forest_key_slot(&r.key) else {
                continue;
            };
            if last == Some(slot) {
                continue;
            }
            last = Some(slot);
            plane.note_holder_op(slot, now, t_idle);
        }
    }

    /// **The commit door** (§5.1.2 first-writer-takes-it + §5.4.1's
    /// door law; review round 2, Issue 6): every slot a tx's records name
    /// must be this mount's before the tx is queued, and the tx holds a
    /// door TOKEN per slot ([`LeaseGate::enter`]) until its terminal
    /// outcome — a release drains them before its flush, so the belt in
    /// `apply_locked` is reached by no legal schedule. A slot mid-
    /// handover PARKS here until the handover completes (bounded,
    /// serialized per volume; `slot_door_parks`) and is re-read; a slot
    /// another appender leases refuses [`KvError::SlotBusy`] (EAGAIN —
    /// the ship to the holder is the S8 verb path, PR 6/12's;
    /// `slot_door_refusals`); a slot nobody leases is acquired here (one
    /// tree-0 write with a PARKING ring admission, like the user commit it
    /// is — Issue 15). A no-op on an unarmed mount (one `Option` test).
    async fn ensure_leases_for_tx(
        &self,
        tx: &KvTx,
    ) -> std::result::Result<Option<super::slot_lease::DoorPass>, KvError> {
        let Some(plane) = self.slot_leases().cloned() else {
            return Ok(None);
        };
        let Some(set) = self.appenders.as_ref() else {
            return Ok(None);
        };
        let mut slots: Vec<super::record::ForestSlot> = Vec::new();
        for (kind, key, _, _) in &tx.staged {
            if !super::record::is_slot_tree_kind(*kind) {
                continue;
            }
            let slot = super::record::legacy_key_slot(*kind, key)?;
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        if slots.is_empty() {
            return Ok(None);
        }
        let mut entered: Vec<super::record::ForestSlot> = Vec::with_capacity(slots.len());
        for slot in slots {
            loop {
                if plane.gate.is_leased(slot) {
                    // Ours (possibly mid-handover): the token BEFORE the
                    // releasing read — the door's half of the Dekker pair.
                    if plane.gate.enter(slot) {
                        entered.push(slot);
                        break;
                    }
                    // Mid-handover: park until it completes or aborts,
                    // then re-read (register-recheck-await).
                    let done = plane.handover_done.notified();
                    if !plane.gate.is_releasing(slot) {
                        continue;
                    }
                    plane.door_parks.fetch_add(1, Ordering::Relaxed);
                    done.await;
                    continue;
                }
                match plane.table.resolve(slot) {
                    crate::slot_lease_core::Resolved::Holder { holder, g }
                        if set.region(holder).is_none() =>
                    {
                        // The tokens taken so far return with the pass.
                        drop(super::slot_lease::DoorPass::new(
                            Arc::clone(&plane),
                            std::mem::take(&mut entered),
                        ));
                        plane.door_refusals.fetch_add(1, Ordering::Relaxed);
                        return Err(KvError::SlotBusy { slot, holder, g });
                    }
                    _ => {
                        // Unleased, or ours in tree 0 without the gate bit
                        // (a window between the arm and the install):
                        // acquire for region 0 — first-writer-takes-it —
                        // then loop to take the token.
                        self.manager_acquire_slots(0, 0, &[slot], ControlAdmit::Park)
                            .await?;
                    }
                }
            }
        }
        Ok(Some(super::slot_lease::DoorPass::new(plane, entered)))
    }

    /// Wait until every door token of `slot` is back (the release's half
    /// of the door law, after `Releasing` was raised): the admitted
    /// commits of the slot reach their terminal outcome — apply included
    /// — before the flush snapshots the tree. Bounded by the conveyor's
    /// own latency; the last token out wakes it.
    async fn drain_door(
        &self,
        plane: &super::slot_lease::SlotLeasePlane,
        slot: super::record::ForestSlot,
    ) {
        loop {
            let drained = plane.door_drained.notified();
            if plane.gate.inflight(slot) == 0 {
                return;
            }
            drained.await;
        }
    }

    /// **The mint policy** (KD-SYM-11 / KD-SYM-16, §5.1.2) on an armed
    /// mount: where a NEW inode under `parent_slot` mints — the parent's
    /// slot iff this mount leases it, it is not native and its tree is
    /// below `A_max(t)`; else the rotor slot with the most headroom; when
    /// EVERY rotor tree is strictly over the cap by ≥ 1 extent the
    /// overflow ask (`may_overflow` = the rotor is below `2 × M`). `None`
    /// unarmed.
    pub fn lease_mint_choice(
        &self,
        parent_slot: Option<super::record::ForestSlot>,
    ) -> Option<crate::slot_lease_core::MintChoice> {
        let plane = self.slot_leases()?;
        let node_size = u64::from(self.sb.node_size);
        // `used_leaf_bytes` (§5.1.2) off the slot-tree extent ledger — the
        // images of every slot tree this mount holds a count for (leaves
        // and interior; the interior share is under 1 % at the shipped
        // fan-out), never the used HEAP (rings, the directory, pages,
        // tree 0 — review round 2, Issue 10). A wire lessee's input is
        // PR 12's ledger field.
        let used = plane.extents.total() * node_size;
        let a_max = super::slot_lease::affinity_ceiling_in_force(
            plane.affinity_static_mb,
            used,
            node_size,
            self.alloc.total_extents() * node_size,
        );
        plane.a_max_bytes.store(a_max, Ordering::Relaxed);
        let parent = parent_slot.map(|s| crate::slot_lease_core::ParentStanding {
            slot: s,
            leased: plane.gate.is_leased(s),
            native: s == super::record::NATIVE_FOREST_SLOT,
            tree_bytes: plane.extents.get(s) * node_size,
        });
        let rotor = plane.rotor.load();
        let rotors: Vec<(super::record::ForestSlot, u64)> = rotor
            .iter()
            .map(|s| (*s, plane.extents.get(*s) * node_size))
            .collect();
        let rr = plane.rr.fetch_add(1, Ordering::Relaxed);
        let may_overflow = (rotor.len() as u64) < 2 * plane.mint_slots();
        let choice = crate::slot_lease_core::mint_choice(
            parent,
            &rotors,
            a_max,
            node_size,
            rr,
            may_overflow,
        )?;
        match choice {
            crate::slot_lease_core::MintChoice::Affinity(_) => {
                plane.affinity_mints.fetch_add(1, Ordering::Relaxed);
            }
            crate::slot_lease_core::MintChoice::Rotor(_)
            | crate::slot_lease_core::MintChoice::Smallest(_) => {
                plane.rotor_mints.fetch_add(1, Ordering::Relaxed);
                if parent.is_some_and(|p| p.leased && !p.native) {
                    plane.ceiling_spills.fetch_add(1, Ordering::Relaxed);
                }
            }
            crate::slot_lease_core::MintChoice::Overflow => {}
        }
        Some(choice)
    }

    /// The overflow arm of the mint policy (§5.1.2): every rotor tree is
    /// strictly over the cap — ask the manager for ONE more rotor slot
    /// (`affinity_ceiling_overflows`) and mint there; past `2 × M` (the
    /// manager refuses) the smallest rotor tree.
    pub async fn lease_mint_overflow(
        &self,
    ) -> std::result::Result<Option<super::record::ForestSlot>, KvError> {
        let Some(plane) = self.slot_leases() else {
            return Ok(None);
        };
        match self
            .manager_acquire_slots(0, 1, &[], ControlAdmit::Park)
            .await
        {
            Ok(grants) => {
                let Some(g) = grants.first() else {
                    return Ok(None);
                };
                let slot = g.slot;
                plane.rotor_update(|rotor| {
                    if !rotor.contains(&slot) {
                        rotor.push(slot);
                    }
                });
                plane.ceiling_overflows.fetch_add(1, Ordering::Relaxed);
                plane.rotor_mints.fetch_add(1, Ordering::Relaxed);
                Ok(Some(slot))
            }
            Err(KvError::RotorAtCap { .. }) => {
                // Past 2 × M: the smallest rotor tree.
                let rotor = plane.rotor.load();
                let smallest = rotor
                    .iter()
                    .min_by_key(|s| (plane.extents.get(**s), **s))
                    .copied();
                if smallest.is_some() {
                    plane.rotor_mints.fetch_add(1, Ordering::Relaxed);
                }
                Ok(smallest)
            }
            Err(e) => Err(e),
        }
    }

    /// One cadence release: `Ok(true)` released the slot; `Ok(false)` = a
    /// concurrent release or transfer already took it (the `Busy` class —
    /// skipped, the pass goes on); any other failure propagates.
    async fn release_for_cadence(
        &self,
        region_id: u32,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<bool, KvError> {
        // A shutdown in progress owns the remaining releases (the leave).
        if self.is_shutting_down() {
            return Ok(false);
        }
        match self.release_slot_handover(region_id, slot).await {
            Ok(()) => Ok(true),
            Err(KvError::Busy(why)) => {
                log::debug!("slot-lease cadence: slot {slot} not released — {why}");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Run [`Self::slot_lease_cadence`] on its own detached task, single-
    /// flight (the checkpoint task's caller — see `slot_cadence_running`).
    /// A no-op on an unarmed mount (one `Option` test) and while a run is
    /// in flight or the volume shuts down. The task holds the backend
    /// `Arc` for its run; the shutdown's leave serializes with a run in
    /// flight through the `handover` mutex.
    pub(super) fn spawn_slot_lease_cadence(self: &Arc<Self>) {
        if self.slot_leases().is_none() || self.is_shutting_down() {
            return;
        }
        if self
            .slot_cadence_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // The latch clears on EVERY exit, a contained panic included
        // (`spawn_meta` counts it on `detached_task_panics`; the next tick
        // must still be able to run a cadence).
        struct Running(Arc<KvMetaBackend>);
        impl Drop for Running {
            fn drop(&mut self) {
                self.0.slot_cadence_running.store(false, Ordering::Release);
            }
        }
        let running = Running(Arc::clone(self));
        crate::meta_exec::spawn_meta("kv_slot_lease_cadence", async move {
            let be = &running.0;
            if let Err(e) = be.slot_lease_cadence().await {
                log::warn!(
                    "slot-lease cadence failed on {:?}: {e} (the next tick retries)",
                    be.device_path()
                );
            }
        });
    }

    /// The slot-lease CADENCE of one checkpoint cycle (§5.1.3 / §5.1.4):
    /// lapse expired offers; LRU-release inherited slots past the page
    /// budget; forced-shrink the rotor to the derived `M` when the
    /// membership census grew; release a declared region whose last
    /// slot went and whose ring is drained. The checkpoint task runs it
    /// after every cadence tick; the contracts run it directly.
    pub async fn slot_lease_cadence(&self) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        let Some(plane) = self.slot_leases().cloned() else {
            return Ok(());
        };
        if !set.joined.load(Ordering::Acquire) || self.read_only || self.non_writer {
            return Ok(());
        }
        let now = crate::mono_core::monotonic_ns_u64();
        let expired = plane.table.expire_offers(now);
        for slot in &expired {
            plane.gate.end_release(*slot);
        }
        // Forced shrink (§5.1.3): the derived M over the census in force;
        // idle rotor slots beyond it released, least-recently-written
        // first, after a covering flush (the handover's own sequence).
        let writers = match plane.test_writers_known.load(Ordering::Relaxed) {
            0 => set.appenders_known.load(Ordering::Relaxed).max(1),
            n => n,
        };
        let m = super::slot_lease::mint_slots_in_force(
            plane.mint_slots_knob,
            u64::from(crate::meta_backend::DERIVED_ROUTING_WIDTH),
            writers,
        );
        if m != plane.mint_slots() {
            plane.mint_slots.store(m, Ordering::Relaxed);
        }
        let rotor = plane.rotor.load_full();
        if rotor.len() as u64 > m {
            let mut idle: Vec<(u64, super::record::ForestSlot)> = rotor
                .iter()
                .filter(|s| plane.holder_ops(**s, now) == 0)
                .map(|s| (plane.extents.get(*s), *s))
                .collect();
            idle.sort_unstable();
            for (_, slot) in idle {
                // Re-read per release: a concurrent pass (an accepted
                // offer's recall, the cadence tick beside a harness call)
                // may have brought the rotor to `M` already.
                if plane.rotor.load().len() as u64 <= m {
                    break;
                }
                if self.release_for_cadence(0, slot).await? {
                    plane.forced_shrinks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // LRU release at the page budget (§5.1.3): a region holding more
        // slots than its page names releases idle NON-rotor slots first.
        for r in &set.regions {
            let held = r.leases();
            if held.len() <= super::appender::SLOT_PAGE_BUDGET {
                continue;
            }
            let rotor = plane.rotor.load_full();
            let mut idle: Vec<super::record::ForestSlot> = held
                .iter()
                .copied()
                .filter(|s| {
                    *s != super::record::NATIVE_FOREST_SLOT
                        && !rotor.contains(s)
                        && plane.holder_ops(*s, now) == 0
                })
                .collect();
            idle.sort_unstable();
            for slot in idle {
                if r.leases().len() <= super::appender::SLOT_PAGE_BUDGET {
                    break;
                }
                if self.release_for_cadence(r.id, slot).await? {
                    plane.lru_releases.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Region release (§5.1.3): a declared region with no lease left
        // and a drained ring goes `Free` — the leave's law for one region.
        for r in set.regions.iter().skip(1) {
            if r.released.load(Ordering::Acquire) || !r.leases().is_empty() {
                continue;
            }
            let ring = r.ring();
            let core = ring.core();
            if core.head() > core.reusable_upto()
                || r.passes_inside.load(Ordering::Acquire) != 0
                || r.windows_inflight.load(Ordering::Acquire) != 0
            {
                continue;
            }
            if self.release_region(r).await? {
                plane.region_releases.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// The clean unmount's lease release for one region (§5.1.3 — the
    /// design's release ORDER, review round 2 Issue 8a): the region's page
    /// with every held slot `Releasing` + barrier FIRST (a crash past this
    /// point leaves attestations the next open drops by the row-6 rule,
    /// never `Live` entries a later lessee's page would contradict), then
    /// every slot `Unleased` in tree 0 with its final words and tails — ONE
    /// control entry that also moves the trees' live images out of the
    /// region's grant record (Issue 4 — the handover's custody step, on
    /// the leave path too) — then the RAM plane.
    async fn release_leases_at_leave(
        &self,
        set: &super::appender::AppenderSet,
        plane: &super::slot_lease::SlotLeasePlane,
        region: &super::appender::AppenderRegion,
    ) -> std::result::Result<(), KvError> {
        let held: Vec<super::record::ForestSlot> = region.leases().iter().copied().collect();
        if held.is_empty() {
            return Ok(());
        }
        {
            let entries = self.lease_page_entries(set, plane, region, &held);
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            page.slots = entries;
        }
        self.write_region_page(region).await?;
        self.sync_device().await.map_err(KvError::Io)?;
        let last_written = self.lease_seq();
        let tag = super::journal::tag_for(super::record::TREE_CONTROL, 0);
        let mut puts: Vec<(u8, Record)> = Vec::with_capacity(held.len());
        let mut releases: Vec<(
            super::record::ForestSlot,
            u32,
            crate::slot_lease_core::SlotWords,
        )> = Vec::with_capacity(held.len());
        for slot in &held {
            let Some(lease) = plane.table.get(*slot) else {
                continue;
            };
            if lease.holder != region.id
                || lease.state == crate::slot_lease_core::LeaseState::Unleased
            {
                continue;
            }
            let words = self.slot_words_now(plane, *slot);
            let tails = match self.forest().and_then(|f| f.tree(*slot)) {
                Some(t) => self.leaf_tails(&t).await?,
                None => Vec::new(),
            };
            let value = super::slot_state::SlotState::Unleased {
                root: RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                },
                cursor: words.cursor,
                g: lease.g,
                slot_tree_extents: words.extents,
                last_written,
                seq_floor: words.seq_floor,
                tails,
            }
            .encode()?;
            puts.push((
                tag,
                Record::put(super::slot_state::slot_state_key(*slot), 0, value),
            ));
            releases.push((*slot, lease.g, words));
        }
        if puts.is_empty() {
            return Ok(());
        }
        let released_slots: Vec<super::record::ForestSlot> =
            releases.iter().map(|(s, _, _)| *s).collect();
        let (rewrite, leaving) = self
            .grant_record_minus_images(region.id, &released_slots)
            .await?;
        puts.extend(rewrite);
        self.write_control_entry(puts, ControlAdmit::Try).await?;
        if !leaving.is_empty() {
            region.grant().transfer_out(&leaving);
        }
        for (slot, g, words) in releases {
            let _ = plane.table.release(slot, region.id, g, words, last_written);
            if words.root.0 != 0 {
                if let Some(forest) = self.forest() {
                    forest.note_published(
                        slot,
                        RootPtr {
                            addr: words.root.0,
                            seq: words.root.1,
                        },
                    );
                }
            }
            region.drop_lease(slot);
            plane.gate.revoke(slot);
        }
        plane.rotor_update(Vec::clear);
        // The lease map is gone with the leave: this volume's contribution
        // to the S4 plane is withdrawn (solo again once the last armed
        // volume leaves).
        crate::dlm_slot::install_lease_foreign_slots(self.volume_uuid(), None);
        plane.refresh_holders();
        Ok(())
    }

    /// Release one declared region (§5.1.3 — its last slot went, its
    /// window is empty): the grant's remainder returns, the page goes
    /// `Free` into both directory slots, the ring's extents return to
    /// the heap and their bits are written (the leave's order for one
    /// region); the region is marked released and skipped by every later
    /// cycle.
    async fn release_region(
        &self,
        region: &super::appender::AppenderRegion,
    ) -> std::result::Result<bool, KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(false);
        };
        // One release per region: the cadence tick and a harness-driven
        // cadence may both find the region empty and drained.
        let _handover = self.handover.lock().await;
        if region.released.load(Ordering::Acquire) || !region.leases().is_empty() {
            return Ok(false);
        }
        let node_size = u64::from(self.sb.node_size);
        let mut back: Vec<u64> = {
            let mut g = region.grant();
            let mut v = g.take_returnable();
            v.extend(g.take_unclaimed());
            v
        };
        back.sort_unstable();
        back.dedup();
        if !back.is_empty() {
            self.return_extents_inner(region.id, &back, false).await?;
        }
        let released: Vec<super::superblock::ExtentRef> = {
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            page.state = super::appender::AppenderState::Free;
            page.slots.clear();
            page.grant.clear();
            let head = region.ring().core().head();
            page.head_hint = head;
            page.ledger_tail_seq = head;
            std::mem::take(&mut page.segments)
        };
        region.dir_named.store(0, Ordering::Release);
        self.write_region_page(region).await?;
        crate::uring_fs::fdatasync(self.path.clone()).await?;
        for ext in &released {
            let mut off = ext.start;
            while off < ext.end() {
                self.alloc
                    .release_unpublished((off - self.sb.heap.start) / node_size);
                off += node_size;
            }
        }
        let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.alloc
            .write_dirty_pages(&self.path, self.sb.alloc_bitmap.start, ckpt_seq)
            .await?;
        crate::uring_fs::fdatasync(self.path.clone()).await?;
        region.released.store(true, Ordering::Release);
        set.leaves.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "meta volume {}: appender region {} RELEASED — its last slot went and its window \
             was empty (design-symmetric-metadata §5.1.3)",
            self.path.display(),
            region.id
        );
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // The manager's verbs (design-symmetric-metadata §5.3.3 / §5.3.5, PR 3):
    // extent grants and returns, the appender join. Every verb mutates
    // DURABLE state through ONE control entry in ring 0 (tree-0 records +
    // allocator deltas, one checksummed entry, barriered before the reply)
    // and is idempotent against that state — never against a RAM window.
    // -----------------------------------------------------------------------

    /// The manager's durable-state precondition: a joined writer holding
    /// the manager lease of a forest volume. `at_leave` = the clean
    /// unmount's own return, which runs under the shutdown latch the
    /// write gate refuses everything else on (a failed volume still
    /// refuses).
    fn manager_gate(
        &self,
        at_leave: bool,
    ) -> std::result::Result<&Arc<super::appender::AppenderSet>, KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume (bit 17 absent) — no manager lease exists",
                self.path.display()
            ))
        })?;
        if self.read_only || self.non_writer || !set.joined.load(Ordering::Acquire) {
            return Err(KvError::Busy(format!(
                "{}: this mount does not hold the manager lease ({}) — only the manager \
                 grants, returns and joins (design-symmetric-metadata KD-SYM-3)",
                self.path.display(),
                set.manager_lease
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .word()
            )));
        }
        if at_leave {
            if self.is_failed() {
                return Err(KvError::Busy(format!(
                    "{}: volume failed — the leave returns nothing",
                    self.path.display()
                )));
            }
        } else {
            self.write_gate()?;
        }
        Ok(set)
    }

    /// ONE control entry in ring 0 carrying `recs` (tree-0 puts,
    /// allocator deltas), applied to tree 0 in RAM after the reservation,
    /// written, then BARRIERED — a manager verb answers only from durable
    /// state (§5.3.5; the S9 `BlockGrant` law: a grant a peer holds is
    /// always journaled). Admitted in the USER class (review round 1,
    /// Issue 15): a verb storm from the wire venue then competes with user
    /// commits for the ring's admissible window and can never eat the
    /// §4.4 pt 5 reserve the checkpoint task's own publication relies on
    /// — the rate bound IS the class. A refused admission is the caller's
    /// retry (the cadence's next cycle, the peer's resend); a failed write
    /// is the journal-failure class.
    async fn write_control_entry(
        &self,
        mut recs: Vec<(u8, Record)>,
        admit: ControlAdmit,
    ) -> std::result::Result<(), KvError> {
        let forest = self.forest().ok_or_else(|| {
            KvError::Corrupt(format!(
                "{}: a control entry on a volume without tree 0",
                self.path.display()
            ))
        })?;
        let len = entry_len_for(&recs)?;
        let adm = match admit {
            ControlAdmit::Try => self
                .ring
                .try_admit(len, AdmissionClass::User)
                .ok_or(KvError::JournalReserveExhausted { needed: len })?,
            // The door's first-touch acquire runs INSIDE a user commit:
            // it parks for ring space exactly as the pass would for that
            // commit (review round 2, Issue 15 — `try_admit` refused on a
            // full ring where every other user commit parks, and the
            // user's first touch of a slot with history failed EINVAL).
            ControlAdmit::Park => self.admit_user_budget(&self.ring, 0, len).await?,
        };
        let (res, seq_base) = self.ring.reserve_registered(adm);
        for (i, (_, r)) in recs.iter_mut().enumerate() {
            r.seq = seq_base + i as u64;
        }
        let control = Arc::clone(forest.control());
        for (tag, r) in &recs {
            if untag(*tag).0 != super::record::TREE_CONTROL {
                continue;
            }
            control
                .apply_replayed(
                    &r.key,
                    r.seq,
                    r.kind,
                    Bytes::copy_from_slice(&r.value),
                    res.start,
                )
                .await?;
        }
        if let Err(e) = self.ring.commit_entry(&res, &recs).await {
            log::error!(
                "meta volume {}: manager control entry write failed ({e}) — the verb's durable \
                 step did not land",
                self.path.display()
            );
            self.note_journal_failure();
            return Err(e);
        }
        self.sync_device().await.map_err(KvError::Io)
    }

    /// Tree 0's lessee map for content appenders (ids ≥ 1): `appender →
    /// the forest slots it leases` — the replay detector's input under
    /// the armed plane.
    async fn read_tree0_lease_map(
        control: &Arc<KvTree>,
    ) -> std::result::Result<
        std::collections::BTreeMap<u32, std::collections::BTreeSet<super::record::ForestSlot>>,
        KvError,
    > {
        let mut out: std::collections::BTreeMap<
            u32,
            std::collections::BTreeSet<super::record::ForestSlot>,
        > = std::collections::BTreeMap::new();
        let (mut cursor, end) = super::slot_state::slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                let slot = super::slot_state::decode_slot_state_key(k)?;
                if let super::slot_state::SlotState::Leased { appender_id, .. } =
                    super::slot_state::SlotState::decode(v)?
                {
                    if appender_id != 0 {
                        out.entry(appender_id).or_default().insert(slot);
                    }
                }
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
    }

    /// Appender `id`'s CURRENT grant record in tree 0 (empty when none).
    pub async fn extent_grant_record(
        &self,
        appender_id: u32,
    ) -> std::result::Result<super::slot_state::ExtentGrantRecord, KvError> {
        let Some(forest) = self.forest() else {
            return Ok(Default::default());
        };
        match forest
            .control()
            .lookup(&super::slot_state::extent_grant_key(appender_id))
            .await?
        {
            Some(v) => super::slot_state::ExtentGrantRecord::decode(&v),
            None => Ok(Default::default()),
        }
    }

    /// Every appender's grant record in tree 0 — the violation detector's
    /// grant map at open (`read_extent_grant_records`). First product
    /// caller of this public face: PR 10's dead-appender recovery driver
    /// (a dead region's whole grant is what it returns); the flat-mount
    /// contract (`a_flat_mount_has_no_manager_lease_and_no_grants`) is its
    /// consumer today.
    pub async fn extent_grant_records(
        &self,
    ) -> std::result::Result<Vec<(u32, super::slot_state::ExtentGrantRecord)>, KvError> {
        let Some(forest) = self.forest() else {
            return Ok(Vec::new());
        };
        Self::read_extent_grant_records(forest.control()).await
    }

    async fn read_extent_grant_records(
        control: &KvTree,
    ) -> std::result::Result<Vec<(u32, super::slot_state::ExtentGrantRecord)>, KvError> {
        let (mut cursor, end) = super::slot_state::extent_grant_key_range();
        let mut out = Vec::new();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                out.push((
                    super::slot_state::decode_extent_grant_key(k)?,
                    super::slot_state::ExtentGrantRecord::decode(v)?,
                ));
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
    }

    /// The grant size the manager answers appender `id` with right now:
    /// the knob, else the §5.3.3 derivation over the region's measured
    /// SMO rate, the failover bound, the free heap and the directory's
    /// LIVE appender count (`appenders_known` — in-process regions AND
    /// wire joiners; review round 1, Issue 12).
    pub(super) fn grant_extents_for(
        &self,
        set: &super::appender::AppenderSet,
        appender_id: u32,
    ) -> u64 {
        let ewma = set
            .region(appender_id)
            .map_or(0, |r| r.smo_ewma_milli.load(Ordering::Relaxed));
        super::appender::resolve_grant_extents(
            ewma,
            set.failover_bound_ms.load(Ordering::Relaxed),
            self.alloc.free_extents(),
            set.appenders_known
                .load(Ordering::Relaxed)
                .max(set.regions.len() as u64)
                .max(1),
        )
    }

    /// Appender `id`'s UNCLAIMED remainder as the durable state has it:
    /// an in-process region's RAM grant (the page mirrors it), a wire
    /// joiner's page `grant` field (the manager writes it at every grant).
    async fn unclaimed_remainder_of(
        &self,
        set: &super::appender::AppenderSet,
        appender_id: u32,
    ) -> std::result::Result<Vec<super::appender::GrantRun>, KvError> {
        if let Some(r) = set.region(appender_id) {
            return Ok(r.grant().unclaimed_runs());
        }
        let entries = super::appender::read_directory(&self.path, &self.sb).await?;
        Ok(entries
            .iter()
            .find(|e| e.appender_id == appender_id)
            .and_then(|e| e.page.as_ref())
            .filter(|p| p.state == super::appender::AppenderState::Live)
            .map(|p| p.grant.clone())
            .unwrap_or_default())
    }

    /// **`ExtentGrant { want }`** (§5.3.3) in the USER class — a grant
    /// never eats the manager's compaction reserve. See
    /// [`Self::manager_extent_grant_class`].
    pub async fn manager_extent_grant(
        &self,
        appender_id: u32,
        want: u32,
    ) -> std::result::Result<Vec<super::appender::GrantRun>, KvError> {
        self.manager_extent_grant_class(appender_id, want, super::alloc_ext_core::AllocClass::User)
            .await
    }

    /// **`ExtentGrant { want }`** (§5.3.3 / §5.3.5): carve up to `want`
    /// extents from the free heap in `class`, coalesced into ≤
    /// `GRANT_RUNS_MAX` runs; journal their allocator deltas and the
    /// appender's rewritten `extent_grant` record as ONE control entry;
    /// barrier; answer the runs. `want == 0` = the derived size; an
    /// explicit `want` is CLAMPED to the derivation's cap — a wire integer
    /// is never an allocation authority (review round 1, Issue 2). The
    /// verb is idempotent against DURABLE state (§5.3.5): a caller whose
    /// unclaimed remainder already covers `want` is answered that
    /// remainder VERBATIM (a replayed frame after a lost reply carves
    /// nothing — `manager_verb_replays`); the 50 % refill law reaches a
    /// fresh carve because a remainder below half the reference is below
    /// the derived size. `class`: USER for the cadence and the join (the
    /// compaction reserve stays the manager's); INTERNAL for the flush
    /// pass's compactions of a leased slot tree — the SMOs that RETURN
    /// extents draw down to the compaction floor like the manager's own,
    /// so the heap-full recovery makes progress on leased trees too
    /// (Issue 6). A heap that cannot serve one extent answers `NoSpace`
    /// — the SPACE class, never `Ok(empty)` counted as a manager stall.
    pub async fn manager_extent_grant_class(
        &self,
        appender_id: u32,
        want: u32,
        class: super::alloc_ext_core::AllocClass,
    ) -> std::result::Result<Vec<super::appender::GrantRun>, KvError> {
        let set = self.manager_gate(false)?;
        if appender_id == 0 {
            set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Busy(format!(
                "{}: appender 0 is the manager and claims from the bitmap it owns — it takes \
                 no grant (manager_verb_refusals)",
                self.path.display()
            )));
        }
        let _g = self.manager_verbs.lock().await;
        let cap = self.grant_extents_for(set, appender_id);
        let want = super::appender::clamp_grant_want(want, cap);
        // §5.3.5: an unconsumed grant is answered verbatim.
        let remainder = self.unclaimed_remainder_of(set, appender_id).await?;
        let remainder_extents: u64 = remainder.iter().map(|r| u64::from(r.len)).sum();
        if remainder_extents >= want && want > 0 {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(remainder);
        }
        let record = self.extent_grant_record(appender_id).await?;
        // Bounded by the FREE heap, never by the wire.
        let mut claimed: Vec<u64> =
            Vec::with_capacity(want.min(self.alloc.free_extents()) as usize);
        for _ in 0..want {
            let claim = match class {
                super::alloc_ext_core::AllocClass::User => self.alloc.claim_user(),
                super::alloc_ext_core::AllocClass::Internal => self.alloc.claim_internal(),
            };
            match claim {
                Ok(e) => claimed.push(e),
                Err(KvError::NoSpace { free, reserve }) => {
                    if claimed.is_empty() {
                        return Err(KvError::NoSpace { free, reserve });
                    }
                    break;
                }
                Err(e) => {
                    for c in claimed {
                        self.alloc.release_unpublished(c);
                    }
                    return Err(e);
                }
            }
        }
        claimed.sort_unstable();
        // The page names ≤ GRANT_RUNS_MAX runs of the WHOLE remainder
        // (Issue 9): the carve is coalesced with the caller's current
        // remainder before it is decided, and while the union has more
        // runs than the page carries, the smallest run made only of NEW
        // claims is released (never granted) — a fragmented heap answers
        // fewer extents rather than a remainder the page cannot name.
        let mut new_set: std::collections::BTreeSet<u64> = claimed.iter().copied().collect();
        let mut union: std::collections::BTreeSet<u64> = remainder
            .iter()
            .flat_map(|r| r.start..r.start + u64::from(r.len))
            .collect();
        union.extend(new_set.iter().copied());
        loop {
            let runs =
                super::slot_state::ExtentGrantRecord::from_extents(union.iter().copied()).runs;
            if runs.len() <= super::appender::GRANT_RUNS_MAX {
                break;
            }
            let Some(victim) = runs
                .iter()
                .filter(|r| (r.start..r.start + u64::from(r.len)).all(|e| new_set.contains(&e)))
                .min_by_key(|r| r.len)
                .cloned()
            else {
                break;
            };
            for e in victim.start..victim.start + u64::from(victim.len) {
                union.remove(&e);
                new_set.remove(&e);
                self.alloc.release_unpublished(e);
            }
        }
        claimed.retain(|e| new_set.contains(e));
        if claimed.is_empty() {
            // Nothing carvable fits beside the caller's fragmented
            // remainder: the remainder, verbatim (a return coalesces it).
            return Ok(remainder);
        }
        let runs = super::slot_state::ExtentGrantRecord::from_extents(claimed.iter().copied()).runs;
        let mut recs: Vec<(u8, Record)> = claimed
            .iter()
            .map(|e| super::alloc_ext::alloc_record(*e, 0))
            .collect();
        let merged = super::slot_state::ExtentGrantRecord::from_extents(
            record.extents().chain(claimed.iter().copied()),
        );
        recs.push((
            super::journal::tag_for(super::record::TREE_CONTROL, 0),
            Record::put(
                super::slot_state::extent_grant_key(appender_id),
                0,
                merged.encode()?,
            ),
        ));
        if let Err(e) = self.write_control_entry(recs, ControlAdmit::Try).await {
            for c in claimed {
                self.alloc.release_unpublished(c);
            }
            return Err(e);
        }
        set.extent_grants.fetch_add(1, Ordering::Relaxed);
        set.extent_grant_extents
            .fetch_add(claimed.len() as u64, Ordering::Relaxed);
        if let Some(r) = set.region(appender_id) {
            // The page names the grant (§5.3.3) — one write; the next
            // checkpoint's barrier covers it, and a page lost before that
            // only over-states the claimed set (fsck C13's class). The
            // page names the WHOLE remainder (Issue 9): an excess run
            // moves to the returnable batch before the write.
            {
                let mut g = r.grant();
                g.add_runs(&runs);
                g.trim_to_page_runs();
                let mut page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                page.grant = g.unclaimed_runs();
            }
            self.write_region_page(r).await?;
        } else {
            // A wire joiner's page: its remainder ∪ the carve, the union
            // the loop above fitted to the page.
            let page_grant =
                super::slot_state::ExtentGrantRecord::from_extents(union.iter().copied()).runs;
            self.write_wire_joiner_page_grant(appender_id, &page_grant)
                .await?;
        }
        log::info!(
            "meta volume {}: extent grant to appender {appender_id}: {} extent(s) in {} run(s) \
             (extent_grants)",
            self.path.display(),
            claimed.len(),
            runs.len()
        );
        Ok(runs)
    }

    /// **`ReturnExtents { runs }`** (§5.3.3): clear the returned extents'
    /// bits — a `free(extent, retire 0)` delta each, immediately reusable:
    /// the appender's own tail covered the free, so nothing durable
    /// routes to the old image — and rewrite the appender's grant record
    /// without them, ONE control entry. Idempotent: an extent the record
    /// no longer grants is already returned (`already`). Answers
    /// `(cleared, already)`. The extent list is the CADENCE's — this
    /// process's own returnable batch, bounded by the grant it came from;
    /// the wire form is [`Self::manager_return_runs`].
    pub async fn manager_return_extents(
        &self,
        appender_id: u32,
        extents: &[u64],
    ) -> std::result::Result<(u64, u64), KvError> {
        self.return_extents_inner(appender_id, extents, false).await
    }

    /// **`ReturnExtents { runs }`** as the WIRE carries it (review round
    /// 1, Issue 2 — "bounded codec = bounded execution"): every run is
    /// validated against the VOLUME before anything proportional to a
    /// wire integer happens — `start + len` must not overflow and must
    /// lie inside the volume's extents; a frame that fails is REJECTED
    /// (`manager_verb_rejected`, the buggy/hostile-peer class — kept
    /// apart from `manager_verb_refusals`, whose must-stay-0 meaning is
    /// "a verb's durable witness contradicts the caller"). The runs are
    /// then intersected with the caller's record as INTERVALS — the
    /// materialized extent list is bounded by the record, never by the
    /// frame — and what lies outside the record is `already` (§5.3.5's
    /// idempotency: a replay after a lost reply names extents the record
    /// no longer holds and answers as the first reply did).
    pub async fn manager_return_runs(
        &self,
        appender_id: u32,
        runs: &[super::appender::GrantRun],
    ) -> std::result::Result<(u64, u64), KvError> {
        let set = self.manager_gate(false)?;
        let total = self.alloc.total_extents();
        let record = self.extent_grant_record(appender_id).await?;
        if let Err(r) = super::appender::validate_return_runs(runs, total) {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Rejected(format!(
                "{}: ReturnExtents from appender {appender_id} rejected — run ({}, {}) lies \
                 outside this volume's {total} extents (manager_verb_rejected)",
                self.path.display(),
                r.start,
                r.len
            )));
        }
        // The frame's runs COALESCED first (bounded by the frame's own
        // length), then intersected with the record (bounded by the
        // record) — never a list proportional to runs × record. `named`
        // is counted AFTER the coalesce, so a duplicated run is one
        // extent, not two (Issue 19).
        let coalesced = super::appender::coalesce_runs(runs);
        let named = super::appender::runs_extent_count(&coalesced);
        let extents = super::appender::intersect_coalesced_with_record(&coalesced, &record);
        if extents.is_empty() {
            // Everything named is already outside the record: a replay
            // after a lost reply — or a peer naming what it never held,
            // which the debug line lets it find.
            log::debug!(
                "meta volume {}: ReturnExtents from appender {appender_id} names {named} \
                 extent(s) in {} run(s), none inside its grant record ({} extent(s)) — \
                 answered already (manager_verb_replays)",
                self.path.display(),
                coalesced.len(),
                record.len()
            );
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok((0, named));
        }
        let outside = named.saturating_sub(extents.len() as u64);
        // The inner partition runs against the FRESH record under the
        // verb mutex: what a concurrent return took is `already` too, so
        // `cleared + already ≡ named` on every reply.
        let (cleared, inner_already) = self
            .return_extents_inner(appender_id, &extents, false)
            .await?;
        Ok((cleared, outside.saturating_add(inner_already)))
    }

    async fn return_extents_inner(
        &self,
        appender_id: u32,
        extents: &[u64],
        at_leave: bool,
    ) -> std::result::Result<(u64, u64), KvError> {
        let set = self.manager_gate(at_leave)?;
        let _g = self.manager_verbs.lock().await;
        let record = self.extent_grant_record(appender_id).await?;
        let (granted, already): (Vec<u64>, Vec<u64>) =
            extents.iter().copied().partition(|e| record.contains(*e));
        if granted.is_empty() {
            if !already.is_empty() {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            }
            return Ok((0, already.len() as u64));
        }
        // An in-process region's RAM grant must not disagree with the
        // bitmap (Issue 11): an extent it still holds CLAIMED holds a live
        // image, one PENDING has its free parked on the region's tail (the
        // §4.7 coverage gate — Issue 17) — both refused; unclaimed /
        // returnable are dropped from RAM BEFORE the durable return (an
        // SMO claiming one between the screen and the bit clear would
        // write an image into an extent the manager re-grants), restored
        // to the returnable batch if the return fails — so a later
        // `claim()` can never hand out an extent the manager re-granted.
        let mut dropped_from_ram: Vec<u64> = Vec::new();
        if let Some(r) = set.region(appender_id) {
            let mut g = r.grant();
            let mut held: Vec<(u64, super::appender::GrantHeld)> = Vec::new();
            for e in &granted {
                match g.drop_returned(*e) {
                    Ok(true) => dropped_from_ram.push(*e),
                    Ok(false) => {}
                    Err(why) => held.push((*e, why)),
                }
            }
            if !held.is_empty() {
                g.restore_returnable(std::mem::take(&mut dropped_from_ram));
                drop(g);
                set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                let (e, why) = held[0];
                return Err(KvError::Busy(format!(
                    "{}: ReturnExtents from appender {appender_id} refused — {} extent(s) are \
                     held by its grant, the first {e} {}: {:?} (manager_verb_refusals)",
                    self.path.display(),
                    held.len(),
                    why.as_str(),
                    held.iter().map(|(e, _)| *e).take(4).collect::<Vec<_>>()
                )));
            }
        }
        let remaining = super::slot_state::ExtentGrantRecord::from_extents(
            record.extents().filter(|e| !granted.contains(e)),
        );
        let mut recs: Vec<(u8, Record)> = granted
            .iter()
            .map(|e| super::alloc_ext::free_record(*e, 0, 0))
            .collect();
        let key = super::slot_state::extent_grant_key(appender_id);
        recs.push((
            super::journal::tag_for(super::record::TREE_CONTROL, 0),
            if remaining.is_empty() {
                Record::delete(key, 0)
            } else {
                Record::put(key, 0, remaining.encode()?)
            },
        ));
        if let Err(e) = self.write_control_entry(recs, ControlAdmit::Try).await {
            if let Some(r) = set.region(appender_id) {
                r.grant().restore_returnable(dropped_from_ram);
            }
            return Err(e);
        }
        for e in &granted {
            self.alloc.release_unpublished(*e);
        }
        if set.region(appender_id).is_none() && !at_leave {
            // A wire joiner's page remainder drops the returned extents
            // (its page is the manager's to write in PR 3; §5.3.5's
            // idempotency reads the remainder off it).
            let remainder = self.unclaimed_remainder_of(set, appender_id).await?;
            let kept = super::slot_state::ExtentGrantRecord::from_extents(
                remainder
                    .iter()
                    .flat_map(|r| r.start..r.start + u64::from(r.len))
                    .filter(|e| !granted.contains(e)),
            )
            .runs;
            self.write_wire_joiner_page_grant(appender_id, &kept)
                .await?;
        }
        set.extent_returns.fetch_add(1, Ordering::Relaxed);
        if !already.is_empty() {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
        }
        Ok((granted.len() as u64, already.len() as u64))
    }

    /// The custody transfer OUT of appender `appender_id`'s grant record
    /// (§5.1.4 / §5.8.5 C13, review round 2 Issue 4): the live image
    /// extents of `slots`' trees the record claims leave it — the
    /// rewritten record (a delete when nothing remains) as the tree-0 put
    /// to ride the caller's control entry, and the extents that left (the
    /// RAM `transfer_out` after the write). Nothing for the manager
    /// (appender 0 — its images are untracked) or a record claiming none.
    async fn grant_record_minus_images(
        &self,
        appender_id: u32,
        slots: &[super::record::ForestSlot],
    ) -> std::result::Result<(Option<(u8, Record)>, Vec<u64>), KvError> {
        if appender_id == 0 {
            return Ok((None, Vec::new()));
        }
        let record = self.extent_grant_record(appender_id).await?;
        if record.is_empty() {
            return Ok((None, Vec::new()));
        }
        let mut leaving: Vec<u64> = Vec::new();
        for slot in slots {
            leaving.extend(
                self.slot_tree_image_extents(*slot)
                    .await?
                    .into_iter()
                    .filter(|e| record.contains(*e)),
            );
        }
        leaving.sort_unstable();
        leaving.dedup();
        if leaving.is_empty() {
            return Ok((None, Vec::new()));
        }
        let remaining = super::slot_state::ExtentGrantRecord::from_extents(
            record.extents().filter(|e| !leaving.contains(e)),
        );
        let key = super::slot_state::extent_grant_key(appender_id);
        let tag = super::journal::tag_for(super::record::TREE_CONTROL, 0);
        let rec = if remaining.is_empty() {
            Record::delete(key, 0)
        } else {
            Record::put(key, 0, remaining.encode()?)
        };
        Ok((Some((tag, rec)), leaving))
    }

    /// The image extents `slot`'s tree reaches (empty when no tree exists)
    /// — a slot handover's custody-transfer set (§5.1.4): one paged walk
    /// of the tree's interior population, C13's reachability per tree.
    /// The tree is flushed and gated (a release) or unleased (a grant) at
    /// every call, so no image of it is between its claim and its
    /// publication.
    async fn slot_tree_image_extents(
        &self,
        slot: super::record::ForestSlot,
    ) -> std::result::Result<Vec<u64>, KvError> {
        let Some(tree) = self.forest().and_then(|f| f.tree(slot)) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<u64> = tree
            .reachable_node_addrs()
            .await?
            .into_iter()
            .map(|addr| self.cache.addr_extent(addr))
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Whether appender `id`'s RAM grant holds `extent` in ANY set
    /// (claimed, unclaimed, parked, returnable) — the durable-vs-RAM law's
    /// probe (`record ⊆ RAM sets` after every open; review round 1, Issue
    /// 1). Its first product caller is PR 4's lease gate (a slot lease's
    /// `slot_tree_extents` audit); the contract suite is the consumer
    /// today.
    pub fn grant_holds(&self, appender_id: u32, extent: u64) -> bool {
        self.appenders
            .as_ref()
            .and_then(|a| a.region(appender_id))
            .is_some_and(|r| r.grant().contains(extent))
    }

    /// The image extents every tree of this volume reaches, under the
    /// caller's SMO + mint serialization (fsck C13's reachability set).
    async fn reachable_image_extents(
        &self,
    ) -> std::result::Result<std::collections::BTreeSet<u64>, KvError> {
        let mut reachable = std::collections::BTreeSet::new();
        for tree in self.all_trees() {
            for addr in tree.reachable_node_addrs().await? {
                reachable.insert(self.cache.addr_extent(addr));
            }
        }
        Ok(reachable)
    }

    /// **fsck C13 — orphan image extents** (design-symmetric-metadata
    /// §5.8.5): every extent a declared region's grant holds CLAIMED that
    /// no tree root reaches. The census runs under the SMO mutex and the
    /// forest's mint guard, so no image is between its claim and its
    /// publication (an SMO's successor before the route flip, a lazy
    /// mint's root before the forest names it) — the "live in-window
    /// image" is never a candidate; a retired image parked on its
    /// region's tail is a pending-free, not a claim. The class is the
    /// §5.3.4 unpublished-root-swap window's successor, the page's
    /// truncated `unclaimed_runs` remainder, and a return lost between
    /// its `advance_durable` and the cadence. Empty on a flat volume and
    /// an unpartitioned forest (no grant exists there); the walk pages
    /// every interior node in once and is skipped when no grant holds a
    /// claim.
    pub async fn c13_orphan_image_extents(
        &self,
    ) -> std::result::Result<Vec<OrphanImageExtent>, KvError> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Ok(Vec::new());
        };
        let _smo = self.smo.lock().await;
        let _mint = match self.forest() {
            Some(f) => Some(f.mint_guard().await),
            None => None,
        };
        let claimed: Vec<(u32, Vec<u64>)> = set
            .regions
            .iter()
            .skip(1)
            .map(|r| (r.id, r.grant().claimed_extents()))
            .filter(|(_, c)| !c.is_empty())
            .collect();
        if claimed.is_empty() {
            return Ok(Vec::new());
        }
        let reachable = self.reachable_image_extents().await?;
        Ok(claimed
            .into_iter()
            .flat_map(|(appender, extents)| {
                extents
                    .into_iter()
                    .filter(|e| !reachable.contains(e))
                    .map(move |extent| OrphanImageExtent { appender, extent })
            })
            .collect())
    }

    /// **fsck C13's repair — return the orphan to the bitmap**: the
    /// appender FREES its orphan image exactly as an SMO retires a
    /// predecessor — one `free(extent)` record in ITS ring (checkpoint-
    /// class admission), the extent parked on ITS tail gated on that
    /// record's seq — so the cadence's `ReturnExtents` clears the bit and
    /// rewrites the grant record once the tail passes it, and the ring's
    /// window never shows an `alloc` outside the record (the `Extent`
    /// violation a direct return would have planted for the next mount).
    /// Verify-before-repair under the same serialization as the census:
    /// `Ok(false)` when the extent is no longer a claimed orphan.
    pub async fn c13_return_orphan(
        &self,
        appender: u32,
        extent: u64,
    ) -> std::result::Result<bool, KvError> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Ok(false);
        };
        let Some(region) = set.region(appender).filter(|r| r.id != 0) else {
            return Ok(false);
        };
        let _smo = self.smo.lock().await;
        let _mint = match self.forest() {
            Some(f) => Some(f.mint_guard().await),
            None => None,
        };
        if !region.grant().is_claimed(extent) {
            return Ok(false);
        }
        if self.reachable_image_extents().await?.contains(&extent) {
            return Ok(false);
        }
        let retire_tag = self.retire_seq.load(Ordering::Acquire);
        let recs = vec![super::alloc_ext::free_record(extent, retire_tag, 0)];
        let len = super::journal::entry_len_for(&recs)?;
        let ring = region.ring();
        let adm = ring
            .try_admit(len, super::journal_core::AdmissionClass::Checkpoint)
            .ok_or(KvError::JournalReserveExhausted { needed: len })?;
        let (res, seq_base) = ring.reserve_registered(adm);
        let mut recs = recs;
        recs[0].1.seq = seq_base;
        ring.commit_entry(&res, &recs).await?;
        region.grant().free_pending(extent, res.start);
        log::info!(
            "meta volume {}: fsck C13 returned orphan image extent {extent} of appender \
             {appender} (freed in its ring at seq {}; the cadence returns it to the bitmap)",
            self.path.display(),
            res.start
        );
        Ok(true)
    }

    /// **`JoinAppender { identity, ring_want_bytes }`** (§5.3.1 / §5.3.5,
    /// KD-SYM-7): a page already `Live` under `identity` answers
    /// `already` with what it names; otherwise the lowest `Free` page of
    /// the directory — the chain grown by one extent when its last
    /// extent's pairs are all taken — a ring of whole heap extents (≤
    /// `RING_SEGMENTS_MAX` segments; `ring_want_bytes` clamped to the
    /// volume's floor/ceiling, 0 = the derivation), their allocator
    /// deltas as ONE control entry + barrier, the page written `Live`
    /// into BOTH directory slots (a table change) + barrier, then the
    /// initial extent grant. Refuses past `appenders_capacity` — the
    /// ONE hard resource (§5.11) — naming the volume count as the lever.
    pub async fn manager_join_appender(
        &self,
        identity: super::appender::AppenderIdentity,
        ring_want_bytes: u64,
    ) -> std::result::Result<JoinOutcome, KvError> {
        use super::appender::{
            dir_pair_offsets, dir_pairs_per_extent, read_directory, read_directory_chain,
            AppenderPage, AppenderState, DirHeader,
        };
        let set = self.manager_gate(false)?;
        let node_size = u64::from(self.sb.node_size);
        let chosen = {
            let _g = self.manager_verbs.lock().await;
            let entries = read_directory(&self.path, &self.sb).await?;
            // KD-SYM-7: the durable witness — a Live page under this
            // identity IS the join, whatever RAM remembers. The reply's
            // `grant` is the joiner's UNCLAIMED remainder (its page's
            // `grant`, never the whole record — the record's claimed
            // images are not a remainder; Issue 3), and a join the
            // manager died inside — page Live, no grant minted — is
            // COMPLETED here: the replay mints the grant the interrupted
            // join owed, so `Joined.grant` means the same thing on every
            // reply.
            if let Some(e) = entries.iter().find(|e| {
                e.page
                    .as_ref()
                    .is_some_and(|p| p.state == AppenderState::Live && p.identity == identity)
            }) {
                let page = e.page.clone().expect("matched a page");
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                let owed = page.grant.is_empty()
                    && set.region(page.appender_id).is_none()
                    && self.extent_grant_record(page.appender_id).await?.is_empty();
                let id = page.appender_id;
                let mut out = JoinOutcome {
                    appender_id: id,
                    page_addr: e.dir_offsets[0],
                    ring_segments: page.segments.clone(),
                    grant: page.grant.clone(),
                    already: true,
                };
                if owed {
                    drop(_g);
                    out.grant = self.manager_extent_grant(id, 0).await?;
                    log::info!(
                        "meta volume {}: JoinAppender replay completed appender {id}'s \
                         interrupted join — the grant its first join never minted",
                        self.path.display()
                    );
                }
                return Ok(out);
            }
            let live = entries
                .iter()
                .filter(|e| {
                    e.page
                        .as_ref()
                        .is_some_and(|p| p.state == AppenderState::Live)
                })
                .count() as u64;
            if live >= set.capacity {
                set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::Busy(format!(
                    "{}: {live} appenders are live and the ring budget admits {} \
                     (appenders_capacity = heap/16 ÷ ring) — the lever is the metadata volume \
                     COUNT, never a format-time client count (design-symmetric-metadata §5.11, \
                     R-SYM-6)",
                    self.path.display(),
                    set.capacity
                )));
            }
            // The lowest Free (or blank) page of the chain; none ⇒ grow it.
            let free = entries.iter().skip(1).find(|e| {
                e.page
                    .as_ref()
                    .is_none_or(|p| p.state == AppenderState::Free)
            });
            let mut claimed: Vec<u64> = Vec::new();
            let (id, dir_offsets, prior) = match free {
                Some(e) => (
                    e.appender_id,
                    e.dir_offsets,
                    e.page
                        .clone()
                        .unwrap_or_else(|| AppenderPage::free(e.appender_id, 0)),
                ),
                None => {
                    let chain = read_directory_chain(&self.path, &self.sb).await?;
                    let (last_ext, last_hdr) = chain.last().copied().ok_or_else(|| {
                        KvError::Corrupt(format!(
                            "{}: a forest volume with no appender directory",
                            self.path.display()
                        ))
                    })?;
                    let ext_idx = self.alloc.claim_internal()?;
                    claimed.push(ext_idx);
                    let extent = super::superblock::ExtentRef {
                        start: self.sb.heap.start + ext_idx * node_size,
                        len: node_size,
                    };
                    // The new extent: zeroed pairs, its header LAST, then
                    // linked from the previous last header — each step
                    // barriered, so a torn growth leaves a chain that ends
                    // where it did.
                    crate::uring_fs::write_at(
                        self.path.clone(),
                        extent.start,
                        bytes::Bytes::from(vec![0u8; node_size as usize]),
                    )
                    .await?;
                    let pairs = dir_pairs_per_extent(node_size);
                    super::appender::write_page(
                        &self.path,
                        super::appender::dir_header_offset(&extent),
                        DirHeader {
                            chain_index: last_hdr.chain_index + 1,
                            next: super::superblock::ExtentRef { start: 0, len: 0 },
                            pairs: pairs as u16,
                        }
                        .encode(),
                    )
                    .await?;
                    self.sync_device().await.map_err(KvError::Io)?;
                    super::appender::write_page(
                        &self.path,
                        super::appender::dir_header_offset(&last_ext),
                        DirHeader {
                            next: extent,
                            ..last_hdr
                        }
                        .encode(),
                    )
                    .await?;
                    self.sync_device().await.map_err(KvError::Io)?;
                    let id = entries.len() as u32;
                    log::info!(
                        "meta volume {}: appender directory grew by one extent at {:#x} (chain \
                         index {}, {pairs} pairs) — appender ids {id}..{}",
                        self.path.display(),
                        extent.start,
                        last_hdr.chain_index + 1,
                        id + pairs as u32 - 1
                    );
                    (id, dir_pair_offsets(&extent, 0), AppenderPage::free(id, 0))
                }
            };
            // The ring: whole extents, coalesced into ≤ RING_SEGMENTS_MAX
            // segments (the open-time carve's law).
            let volume_len = self.sb.heap.end();
            let ring_bytes = if ring_want_bytes == 0 {
                super::appender::resolve_sym_ring_bytes(0, volume_len)
            } else {
                ring_want_bytes.clamp(
                    super::appender::SYM_RING_FLOOR_BYTES,
                    super::appender::sym_ring_ceiling_bytes(volume_len),
                )
            };
            let want_extents = ring_bytes.div_ceil(node_size).max(1);
            let mut ring_claimed: Vec<u64> = Vec::with_capacity(want_extents as usize);
            for _ in 0..want_extents {
                match self.alloc.claim_internal() {
                    Ok(e) => ring_claimed.push(e),
                    Err(err) => {
                        for e in claimed.iter().chain(ring_claimed.iter()) {
                            self.alloc.release_unpublished(*e);
                        }
                        return Err(err);
                    }
                }
            }
            ring_claimed.sort_unstable();
            let mut segments: Vec<super::superblock::ExtentRef> = Vec::new();
            for e in &ring_claimed {
                let start = self.sb.heap.start + e * node_size;
                match segments.last_mut() {
                    Some(last) if last.end() == start => last.len += node_size,
                    _ => segments.push(super::superblock::ExtentRef {
                        start,
                        len: node_size,
                    }),
                }
            }
            if segments.len() > super::appender::RING_SEGMENTS_MAX {
                for e in claimed.iter().chain(ring_claimed.iter()) {
                    self.alloc.release_unpublished(*e);
                }
                return Err(KvError::Corrupt(format!(
                    "{}: a {ring_bytes}-byte ring would take {} segments of this heap's free \
                     extents (the page names at most {}) — lower {} or raise --meta-node-kib",
                    self.path.display(),
                    segments.len(),
                    super::appender::RING_SEGMENTS_MAX,
                    super::appender::SYM_RING_KB_ENV
                )));
            }
            claimed.extend(ring_claimed);
            // A predecessor incarnation's ring may have occupied these
            // extents: zeroed before any page names them (`zero_extents`).
            if let Err(e) = super::appender::zero_extents(&self.path, &segments).await {
                for c in claimed {
                    self.alloc.release_unpublished(c);
                }
                return Err(e);
            }
            // The claims' deltas, durable before any page names the ring.
            let recs: Vec<(u8, Record)> = claimed
                .iter()
                .map(|e| super::alloc_ext::alloc_record(*e, 0))
                .collect();
            if let Err(e) = self.write_control_entry(recs, ControlAdmit::Try).await {
                for c in claimed {
                    self.alloc.release_unpublished(c);
                }
                return Err(e);
            }
            // The page, Live under the joiner, into BOTH directory slots.
            let mut page = prior;
            page.appender_id = id;
            page.term += 1;
            page.state = AppenderState::Live;
            page.recovered_by_term = 0;
            page.identity = identity;
            page.is_manager = false;
            page.home_volume = 0;
            page.segments = segments.clone();
            // The joiner's position space continues past this page's
            // watermark and ring 0's head (the carve law — see
            // `open_appender_regions`'s fresh-ring arm).
            let start = page.head_hint.max(self.ring.core().head());
            page.head_hint = start;
            page.ledger_tail_seq = start;
            page.ckpt_seq = 0;
            page.grant.clear();
            page.slots.clear();
            for off in dir_offsets {
                page.generation += 1;
                super::appender::write_page(&self.path, off, page.encode()?).await?;
            }
            self.sync_device().await.map_err(KvError::Io)?;
            set.joins.fetch_add(1, Ordering::Relaxed);
            set.appenders_known
                .store((live + 1).max(set.regions.len() as u64), Ordering::Relaxed);
            log::info!(
                "meta volume {}: JoinAppender — appender {id} (node {:#018x}, mount slot {:#x}) \
                 joined with a {ring_bytes}-byte ring in {} segment(s), term {}",
                self.path.display(),
                identity.node_token,
                identity.mount_slot,
                segments.len(),
                page.term
            );
            (id, dir_offsets[0], segments)
        };
        if TEST_JOIN_HOLD_AFTER_PAGE.load(Ordering::Relaxed) {
            return Err(KvError::Busy(format!(
                "{}: TEST_JOIN_HOLD_AFTER_PAGE — the join failed after its page went Live and \
                 before its initial grant",
                self.path.display()
            )));
        }
        // The initial grant — its own control entry, outside the join's
        // critical section (the verb mutex is not reentrant).
        let (id, page_addr, ring_segments) = chosen;
        // The grant writes the joiner's page with it.
        let grant = self.manager_extent_grant(id, 0).await?;
        // The joiner's identity for the membership carriage (PR 4).
        if let Some(plane) = set.slot_leases() {
            plane.note_identity(id, identity.node_token, identity.mount_slot);
        }
        Ok(JoinOutcome {
            appender_id: id,
            page_addr,
            ring_segments,
            grant,
            already: false,
        })
    }

    /// A WIRE joiner's page names its grant — the manager writes it (an
    /// in-process region's page is written inside the grant itself).
    async fn write_wire_joiner_page_grant(
        &self,
        appender_id: u32,
        grant: &[super::appender::GrantRun],
    ) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        if set.region(appender_id).is_some() {
            return Ok(());
        }
        let entries = super::appender::read_directory(&self.path, &self.sb).await?;
        if let Some(e) = entries.iter().find(|e| e.appender_id == appender_id) {
            if let Some(mut page) = e.page.clone() {
                page.grant = grant.to_vec();
                for off in e.dir_offsets {
                    page.generation += 1;
                    super::appender::write_page(&self.path, off, page.encode()?).await?;
                }
            }
        }
        Ok(())
    }

    /// The appender set (the manager service's gauge handle).
    pub fn appenders_public(&self) -> Option<&Arc<super::appender::AppenderSet>> {
        self.appenders.as_ref()
    }

    /// **The grant cadence of one checkpoint cycle** (§5.3.3): fold every
    /// region's SMO rate, ship the extents its tail released as
    /// `ReturnExtents`, and refill a region at 50 % consumption — the
    /// `alloc_lane` ahead-refill law. In PR 3 the manager is this
    /// process (the region's grant calls are local); the wire form rides
    /// the same executors.
    pub(super) async fn grant_cadence(&self, cycle_ms: u64) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Ok(());
        };
        if !set.joined.load(Ordering::Acquire) || self.read_only || self.non_writer {
            return Ok(());
        }
        for r in set.regions.iter().skip(1) {
            r.fold_smo_rate(cycle_ms);
            let returnable = r.grant().take_returnable();
            if !returnable.is_empty() {
                if let Err(e) = self.manager_return_extents(r.id, &returnable).await {
                    r.grant().restore_returnable(returnable);
                    log::warn!(
                        "meta volume {}: appender {}'s ReturnExtents deferred ({e}); retried next \
                         cycle",
                        self.path.display(),
                        r.id
                    );
                }
            }
            // Due at 50 % consumption AND below the derived size (a
            // remainder at or above it is answered verbatim by §5.3.5's
            // idempotency — asking would only count a replay).
            let due = {
                let g = r.grant();
                g.refill_due() && g.unclaimed() < self.grant_extents_for(set, r.id)
            };
            if due && !super::appender::test_manager_unreachable() {
                if let Err(e) = self.manager_extent_grant(r.id, 0).await {
                    log::warn!(
                        "meta volume {}: appender {}'s ExtentGrant refill deferred ({e})",
                        self.path.display(),
                        r.id
                    );
                }
            }
        }
        Ok(())
    }

    /// **Ring growth on `journal_full_stalls`** (design-symmetric-metadata
    /// §5.3.2; PR 2) — the checkpoint cycle's first step on a partitioned
    /// forest volume. A declared region whose ring stalled since its last
    /// growth decision, is DRAINED (head == reusable_upto, no reservation
    /// open) and has no pass inside its admit→handoff window (the Dekker
    /// pair with `run_batch_group`) gets ONE more segment — extents
    /// claimed internal-class up to the ring's current size, adjacent
    /// ones coalesced — in this order: bitmap pages + barrier (the extents
    /// are durably claimed), the page naming the grown table + barrier
    /// (replay's geometry), THEN the ring swap (nothing lands in the new
    /// geometry before a page describes it). Region 0's fixed ring never
    /// grows: it is the format's `--meta-journal-mb` decision.
    pub(super) async fn grow_stalled_regions(&self) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Ok(());
        };
        if !set.joined.load(Ordering::Acquire) {
            return Ok(());
        }
        let node_size = u64::from(self.sb.node_size);
        for r in set.regions.iter().skip(1) {
            let stalls = r.stalls.load(Ordering::Relaxed);
            if stalls == r.stalls_at_last_grow.load(Ordering::Relaxed) {
                continue;
            }
            let ring = r.ring();
            if ring.segments().len() >= super::appender::RING_SEGMENTS_MAX {
                r.stalls_at_last_grow.store(stalls, Ordering::Relaxed);
                continue;
            }
            r.growing.store(true, Ordering::SeqCst);
            // A pass inside the region's window, or a stage-B window not
            // yet at its terminal outcome (its reservation may be closed
            // while its §4.4 pt 4 compensation is still to reserve on the
            // ring it holds): the ring is not this cycle's to replace.
            if r.passes_inside.load(Ordering::SeqCst) != 0
                || r.windows_inflight.load(Ordering::SeqCst) != 0
            {
                r.end_growth();
                continue;
            }
            let core = ring.core();
            let drained =
                core.head() == core.reusable_upto() && ring.min_inflight_start() == u64::MAX;
            if !drained {
                r.end_growth();
                continue;
            }
            // One growth step = up to the ring's current size in extents,
            // as ONE contiguous segment (the run breaks at the first
            // non-adjacent claim; the remainder is released).
            let want = (ring.ring_bytes() / node_size).max(1);
            let mut claimed: Vec<u64> = Vec::new();
            for _ in 0..want {
                match self.alloc.claim_internal() {
                    Ok(e) => {
                        if let Some(&last) = claimed.last() {
                            if e != last + 1 {
                                self.alloc.release_unpublished(e);
                                break;
                            }
                        }
                        claimed.push(e);
                    }
                    Err(_) => break,
                }
            }
            if claimed.is_empty() {
                r.end_growth();
                r.stalls_at_last_grow.store(stalls, Ordering::Relaxed);
                continue;
            }
            let extent = super::superblock::ExtentRef {
                start: self.sb.heap.start + claimed[0] * node_size,
                len: claimed.len() as u64 * node_size,
            };
            // A predecessor ring may have occupied the new segment's
            // extents: zeroed before the table names it (`zero_extents`).
            if let Err(e) = super::appender::zero_extents(&self.path, &[extent]).await {
                for c in claimed {
                    self.alloc.release_unpublished(c);
                }
                r.end_growth();
                return Err(e);
            }
            let grown = match ring.grown_with(super::journal::RingSegment::from_extent(&extent)) {
                Ok(g) => g,
                Err(e) => {
                    // The drained/bound preconditions were checked above;
                    // a refusal here is a race with a straggling
                    // completion — skip this cycle, keep the stall on
                    // record so the next cycle retries.
                    for c in claimed {
                        self.alloc.release_unpublished(c);
                    }
                    r.end_growth();
                    log::debug!(
                        "checkpoint: appender {}'s ring growth deferred ({e}); retrying next cycle",
                        r.id
                    );
                    continue;
                }
            };
            // The extents' bits, durable before any page names them. A
            // checkpoint-class durable step CONSUMES a checkpoint seq
            // (ledger slots are `seq % 32`, so the gap is harmless): the
            // next cycle's bitmap write then carries a strictly higher
            // generation instead of tying this one and taking DUR-4's
            // loud raise.
            let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
            self.alloc
                .write_dirty_pages(&self.path, self.sb.alloc_bitmap.start, ckpt_seq)
                .await?;
            self.sync_device().await.map_err(KvError::Io)?;
            // The page naming the grown table at the drained head — a
            // TABLE CHANGE, so it lands in the directory pair (both
            // slots) before any position is written under the new map.
            {
                let mut page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                page.segments.push(extent);
                page.head_hint = core.head();
                page.ledger_tail_seq = core.head();
            }
            r.dir_named.store(0, Ordering::Release);
            self.write_region_page(r).await?;
            self.sync_device().await.map_err(KvError::Io)?;
            r.last_tail.store(core.head(), Ordering::Release);
            r.durable_tail.fetch_max(core.head(), Ordering::AcqRel);
            r.ring.store(Arc::new(grown));
            r.ring_grows.fetch_add(1, Ordering::Relaxed);
            r.stalls_at_last_grow.store(stalls, Ordering::Relaxed);
            r.end_growth();
            log::info!(
                "meta volume {}: appender {}'s ring stalled ({stalls} parks) and was drained — \
                 grew by one segment of {} bytes at {:#x} ({} segments, {} bytes; \
                 appender_ring_grows)",
                self.path.display(),
                r.id,
                extent.len,
                extent.start,
                r.ring().segments().len(),
                r.ring().ring_bytes()
            );
        }
        Ok(())
    }

    /// The per-region TAILS of one checkpoint cycle (design-symmetric-
    /// metadata §5.3.4 — one ring, one tail, per appender): region 0's is
    /// the caller's ledger tail; a declared region's is `min(its head, its
    /// oldest open reservation, the dying floors of its slots' leaves,
    /// the live floors of its slots' dirty leaves)` — every position in
    /// ITS ring's logical space. `leaf_floors` are the per-slot dying
    /// floors the cycle drained; `live_floors` the dirty leaves' by slot.
    pub(super) fn appender_region_tails(
        &self,
        leaf_floors: &std::collections::BTreeMap<super::record::ForestSlot, u64>,
        live_floors: &std::collections::BTreeMap<super::record::ForestSlot, u64>,
    ) -> Vec<(u32, u64)> {
        let Some(set) = self.appenders.as_ref().filter(|a| a.is_partitioned()) else {
            return Vec::new();
        };
        set.regions
            .iter()
            .skip(1)
            .map(|r| {
                let ring = r.ring();
                let mut tail = ring.core().head().min(ring.min_inflight_start());
                for (slot, f) in leaf_floors.iter().chain(live_floors.iter()) {
                    if r.leases_slot(*slot) {
                        tail = tail.min(*f);
                    }
                }
                (r.id, tail)
            })
            .collect()
    }

    /// Write region `r`'s RAM page under the DIRECTORY-FIRST law
    /// ([`super::appender::page_write_slots`]): one image per slot the law
    /// names, the generation advanced once per image, the pair's
    /// confirmation mask raised to ALL afterwards. The caller mutates the
    /// page's content first and barriers afterwards; a site that CHANGED
    /// the segment table resets `dir_named` before calling (the fresh ring
    /// at open, growth, the leave), which is what routes that page into
    /// the directory pair instead of a ring-side slot no directory image
    /// can reach yet.
    async fn write_region_page(
        &self,
        r: &super::appender::AppenderRegion,
    ) -> std::result::Result<(), KvError> {
        let images: Vec<(u64, Vec<u8>)> = {
            let mut page = r.page.lock().unwrap_or_else(|e| e.into_inner());
            let mask = r.dir_named.load(Ordering::Acquire);
            let slots = super::appender::page_write_slots(mask, page.generation + 1);
            let mut out = Vec::with_capacity(slots.len());
            for slot in slots {
                page.generation += 1;
                out.push((r.page_offsets[slot], page.encode()?));
            }
            out
        };
        for (off, img) in images {
            super::appender::write_page(&self.path, off, img).await?;
        }
        r.dir_named
            .store(super::appender::DIR_NAMED_ALL, Ordering::Release);
        Ok(())
    }

    /// **The appender PAGE writes of one checkpoint cycle** (§5.3.2 — the
    /// page IS the appender's ledger record; one write per checkpoint):
    /// region 0's mirrors the fixed ledger (its tail, this `ckpt_seq`) and
    /// names the roots of the slot trees it holds — the native slot and
    /// every guest slot no declared region leases, up to the page budget
    /// — never tree 0's root or the stamp (KD-SYM-3); a declared region's
    /// names its own tail and the roots of its leased slots' trees (an
    /// unminted slot has root `(0, 0)`). Each write's tail is pushed on the
    /// region's pending-reclaim ledger with the barrier epoch, exactly
    /// like the fixed ledger's. A no-op on a flat volume and before the
    /// join.
    pub(super) async fn write_appender_pages(
        &self,
        ledger_tail: u64,
        ckpt_seq: u64,
        head: u64,
        region_tails: &[(u32, u64)],
    ) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        if !set.joined.load(Ordering::Acquire) || self.read_only || self.non_writer {
            return Ok(());
        }
        let Some(forest) = self.forest() else {
            return Ok(());
        };
        let trees = forest.slot_trees();
        for r in &set.regions {
            if r.released.load(Ordering::Acquire) {
                continue;
            }
            let tail = if r.id == 0 {
                ledger_tail
            } else {
                region_tails
                    .iter()
                    .find(|(id, _)| *id == r.id)
                    .map_or(0, |(_, t)| *t)
            };
            let mut entries: Vec<super::appender::SlotEntry> = Vec::new();
            if let Some(plane) = self.slot_leases() {
                // The armed plane (PR 4, KD-SYM-3): every page names
                // exactly the slots its region leases, with their live
                // `g`, extent count, root and cursor.
                entries = self.lease_page_entries(set, plane, r, &[]);
            } else if r.id == 0 {
                for (slot, tree) in &trees {
                    if set.region_of_slot(*slot) != 0 {
                        continue;
                    }
                    if entries.len() >= super::appender::SLOT_PAGE_BUDGET {
                        break; // the rest ride tree 0 until PR 4's LRU release
                    }
                    let Ok(page_slot) =
                        super::appender::page_slot_of_forest_slot(*slot, set.native_slot)
                    else {
                        continue;
                    };
                    let root = tree.root();
                    entries.push(super::appender::SlotEntry {
                        slot: page_slot,
                        state: super::appender::SlotEntryState::Live,
                        g: 0,
                        slot_tree_extents: 0,
                        root,
                        cursor: 0,
                    });
                }
            } else {
                for slot in r.leases().iter() {
                    let Ok(page_slot) =
                        super::appender::page_slot_of_forest_slot(*slot, set.native_slot)
                    else {
                        continue;
                    };
                    let root = forest
                        .tree(*slot)
                        .map_or(RootPtr { addr: 0, seq: 0 }, |t| t.root());
                    entries.push(super::appender::SlotEntry {
                        slot: page_slot,
                        state: super::appender::SlotEntryState::Live,
                        g: 0,
                        slot_tree_extents: 0,
                        root,
                        cursor: 0,
                    });
                }
            }
            entries.sort_by_key(|e| e.slot);
            entries.dedup_by_key(|e| e.slot);
            {
                let mut page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                page.ledger_tail_seq = tail;
                page.ckpt_seq = ckpt_seq;
                page.head_hint = if r.id == 0 {
                    head
                } else {
                    r.ring().core().head()
                };
                // The seq-space law's durable word (§5.8.2): the ring's
                // offset in force — recovery reads it back as the floor
                // of the window's own stamps.
                page.seq_offset = r.ring().seq_offset();
                // The segment table names EXTENTS (a declared region's
                // first extent holds its two ring-side pages ahead of the
                // ring proper; growth appends whole extents), so it is
                // maintained at open and growth, never derived from the
                // ring's page ranges. Appender 0's is the fixed extent past
                // its page slots, as format wrote it.
                if r.id == 0 {
                    page.segments = vec![super::appender::appender0_ring_extent(&self.sb.journal)];
                } else {
                    // The grant's UNCLAIMED remainder (§5.3.3): recovery
                    // reads it against tree 0's record for the claimed
                    // set, so the page names the WHOLE of it — a remainder
                    // in more runs than the page carries moves its excess
                    // to the returnable batch first (Issue 9), never a
                    // truncation that recovery would read as claimed.
                    let mut g = r.grant();
                    let moved = g.trim_to_page_runs();
                    if moved > 0 {
                        log::debug!(
                            "meta volume {}: appender {}'s unclaimed remainder exceeded the \
                             page's {} runs — {moved} extent(s) moved to the returnable batch",
                            self.path.display(),
                            r.id,
                            super::appender::GRANT_RUNS_MAX
                        );
                    }
                    page.grant = g.unclaimed_runs();
                }
                page.slots = entries.clone();
            }
            self.write_region_page(r).await?;
            r.last_tail.store(tail, Ordering::Release);
            if r.id != 0 {
                r.pending_reclaim
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((tail, self.barrier_push_epoch()));
            }
            // Under the armed plane the page IS a leased root's durable
            // home (`publish_forest_roots` skips leased slots): a `Live`
            // entry naming a root is that root's publication — its floor
            // stops clamping the next cycle's tail. The next cycle's
            // record advances the tail past the floor only after a barrier
            // that started after this write (the DUR-3 push above), so
            // the page is durable before any record under the root can
            // leave the window. An entry the budget truncated off the
            // page publishes nothing and keeps its floor.
            if self.slot_leases().is_some() {
                for e in entries.iter().filter(|e| {
                    e.state == super::appender::SlotEntryState::Live && e.root.addr != 0
                }) {
                    let slot = super::appender::forest_slot_of_page_slot(e.slot, set.native_slot);
                    if slot != super::record::NATIVE_FOREST_SLOT {
                        forest.note_page_published(slot, e.root);
                    }
                }
            }
        }
        Ok(())
    }

    /// The flush-ceiling audit of one cycle (KD-SYM-10, §5.7.3: "every
    /// dirty leaf … is flushed and barriered within `CHECKPOINT_MAX_AGE_MS`"):
    /// `had_dirty` names the regions whose STAMPED leaves (a slot tree's)
    /// were dirty when the flush pass began, each with its OLDEST leaf's
    /// dirty-since instant (CLOCK_MONOTONIC ns); `now_ns` is the covering
    /// barrier's completion. The audited quantity is the leaf's AGE at the
    /// barrier — record → durable — against the LANDING ceiling of the
    /// cadence in force (`AppenderSet::flush_ceiling_ms`; the trigger
    /// alone, the round-2 build, read a healthy mount as overrunning by
    /// the pass; the pass wall alone, the round-1 build, read 0 for every
    /// violation up to ≈ 2× the bound). Past the ceiling each such region
    /// is one overrun (must-stay-0), logged.
    pub(super) fn note_flush_ceiling(&self, had_dirty: &[(u32, u64)], now_ns: u64) {
        let Some(set) = self.appenders.as_ref() else {
            return;
        };
        let ceiling_ns = set.flush_ceiling_ms * 1_000_000;
        let over: Vec<(u32, u64)> = had_dirty
            .iter()
            .filter(|(_, since)| now_ns.saturating_sub(*since) > ceiling_ns)
            .map(|(r, since)| (*r, now_ns.saturating_sub(*since) / 1_000_000))
            .collect();
        if over.is_empty() {
            return;
        }
        set.flush_ceiling_overruns
            .fetch_add(over.len() as u64, Ordering::Relaxed);
        log::warn!(
            "meta volume {}: flush ceiling OVERRUN — appender region(s) {over:?} (id, oldest \
             dirty leaf's age in ms at the covering barrier) exceeded the {} ms landing \
             ceiling (appender_flush_ceiling_overruns, must stay 0)",
            self.path.display(),
            set.flush_ceiling_ms
        );
    }

    /// The fixed journal extent's RING part: the whole extent on a flat
    /// volume; on a forest volume (bit 17) the extent past appender 0's
    /// four page slots (design-symmetric-metadata §5.3.2).
    pub fn fixed_ring_extent(sb: &SuperblockV3) -> super::superblock::ExtentRef {
        if sb.symmetric_forest_stamped() {
            super::appender::appender0_ring_extent(&sb.journal)
        } else {
            sb.journal
        }
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
        Ok(self.lookup_kind(tree_id, key).await?.map(|b| b.to_vec()))
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
    /// not have). Since the rung-10b flip the DEFAULT format carries
    /// bit 13, so this is `true` on every plain write mount and fresh
    /// mints carry stamps (PR 6a's item-1 verdict; `--single-writer` /
    /// pre-flip volumes stay bare).
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

    /// §4.7 heap-full posture word (`meta_kv_heap_full`, 0/1 per volume):
    /// growth needing a new leaf is refused `ENOSPC` while set. Reads,
    /// deletes and in-place appends keep committing; the volume is never
    /// FAILED for it — the SPACE standstill class, not the wedge class.
    pub fn heap_full(&self) -> bool {
        self.heap_full.load(Ordering::Acquire)
    }

    /// `meta_kv_enospc_refusals`: user commits refused `NoSpace` at heap
    /// admission on this volume.
    pub fn enospc_refusals(&self) -> u64 {
        self.enospc_refusals.load(Ordering::Relaxed)
    }

    /// `meta_kv_heap_full_cycles`: checkpoint cycles whose flush pass
    /// deferred ≥ 1 node because the allocator answered `NoSpace`.
    pub fn heap_full_cycles(&self) -> u64 {
        self.heap_full_cycles.load(Ordering::Relaxed)
    }

    /// `meta_kv_heap_promised`: extents the heap admission promised to
    /// pending SMOs ([`NodeCache::heap_promised`]) — 0 at quiesce.
    pub fn heap_promised(&self) -> u64 {
        self.cache.heap_promised()
    }

    /// `meta_kv_merge_sweeps` (§4.6a (e)): heap-full / backlog merge
    /// sweeps the checkpoint cycle ran on this volume.
    pub fn merge_sweeps(&self) -> u64 {
        self.merge_sweeps.load(Ordering::Relaxed)
    }

    /// `meta_kv_merge_candidates` (§4.6a (h), EXACT as of the last
    /// completed sweep lap or census): underfull leaves standing on this
    /// volume.
    pub fn merge_candidates(&self) -> u64 {
        self.merge_candidates.load(Ordering::Relaxed)
    }

    /// `meta_kv_merge_laps`: whole-volume sweep laps completed (the
    /// exact-candidates publish instants).
    pub fn merge_laps(&self) -> u64 {
        self.merge_laps.load(Ordering::Relaxed)
    }

    /// `meta_kv_merge_sweep_ns`: wall ns spent in sweep calls (sum).
    pub fn merge_sweep_ns(&self) -> u64 {
        self.merge_sweep_ns.load(Ordering::Relaxed)
    }

    /// `meta_kv_merge_sweep_projections`: nodes the sweep projected (count).
    pub fn merge_sweep_projections(&self) -> u64 {
        self.merge_sweep_projections.load(Ordering::Relaxed)
    }

    /// The sweep's per-cycle budget in force (ms) — `merge_sweep_budget_ms`
    /// of the flush interval this volume opened with.
    pub fn merge_sweep_budget_ms(&self) -> u64 {
        self.merge_sweep_budget_ms
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

    /// The per-kind trees of a FLAT volume, tree-id order (inodes,
    /// dentries, xattrs) — the shape the pre-forest suites drive
    /// directly. **Empty on a forest volume**: there is no tree per kind
    /// there, and production code never indexes this — it routes through
    /// [`Self::lookup_kind`] / [`Self::range_kind`] / the structural
    /// [`Self::all_trees`], which serve both layouts.
    pub fn flat_trees(&self) -> Vec<Arc<KvTree>> {
        match &self.trees {
            TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                ..
            } => vec![Arc::clone(inodes), Arc::clone(dentries), Arc::clone(xattrs)],
            TreeSet::Forest { .. } => Vec::new(),
        }
    }

    /// The §4.2 record kinds every volume serves as USER content — the
    /// digest walk's, the slot-migration keyspace's and fsck's tree-walk
    /// domain, in tree-id order.
    pub const USER_KINDS: [u8; 3] = [TREE_INODES, TREE_DENTRIES, TREE_XATTRS];

    /// Every tree this volume must checkpoint, flush, and name a root
    /// for: flat — the three §4.2 user trees plus the durable block-
    /// reference tree and the block-map tree when engaged; forest — tree
    /// 0 plus every slot tree that exists.
    pub fn all_trees(&self) -> Vec<Arc<KvTree>> {
        self.trees.all()
    }

    /// Whether this volume is a slot-tree forest (incompat bit 17).
    pub fn symmetric_forest(&self) -> bool {
        matches!(self.trees, TreeSet::Forest { .. })
    }

    /// The forest router, on a forest volume.
    fn forest(&self) -> Option<&super::forest::SlotTrees> {
        match &self.trees {
            TreeSet::Forest { forest, .. } => Some(forest),
            TreeSet::Flat { .. } => None,
        }
    }

    /// Whether this mount's directory listings must be FILTERED to the
    /// slot trees it holds: a non-writer of a forest (S5 reader, co-writer,
    /// probe, a §4.11-degraded writer) serves the slot trees tree 0 named
    /// at its last poll, while a dentry lives in its PARENT's slot and
    /// names a child in the CHILD's — the one edge that crosses slots —
    /// and the parent's leaf log carries the child's dentry as soon as a
    /// threshold append lands it, checkpoint or not. `false` on every
    /// write mount (it holds every slot it ever wrote) and on a flat
    /// volume (one tree per kind): one bool, so the shipped listing path
    /// pays nothing.
    pub fn filters_unpublished_children(&self) -> bool {
        self.non_writer && self.forest().is_some()
    }

    /// Whether this mount HOLDS the slot tree of `local` — the condition
    /// its `lookup` (dentry + `getattr(child)`) answers for a child, and
    /// therefore the condition under which its `readdir` lists the child's
    /// name (`RoutedMetaBackend::readdir_stream`): the partial view a
    /// non-writer serves is a consistent snapshot, adopted whole at the
    /// poll that names the slot (design-symmetric-metadata §5.3.4). Always
    /// `true` where [`Self::filters_unpublished_children`] is `false`.
    pub fn holds_slot_of(&self, local: Ino) -> bool {
        if !self.filters_unpublished_children() {
            return true;
        }
        let held = self
            .forest()
            .is_some_and(|f| f.tree(super::record::forest_slot_of_ino(local)).is_some());
        if !held {
            super::META_KV_FOREST_READER_UNPUBLISHED_CHILDREN.fetch_add(1, Ordering::Relaxed);
        }
        held
    }

    /// The live root of slot tree `slot` (`None` = the slot has no tree —
    /// "an empty slot owns no extent"; also `None` on a flat volume).
    pub fn slot_tree_root(&self, slot: super::record::ForestSlot) -> Option<RootPtr> {
        self.forest()?.tree(slot).map(|t| t.root())
    }

    /// The root LEVEL of slot tree `slot` (0 = one leaf; `None` as
    /// [`Self::slot_tree_root`]).
    pub async fn slot_tree_root_level(&self, slot: super::record::ForestSlot) -> Option<u8> {
        let tree = self.forest()?.tree(slot)?;
        tree.root_level().await.ok()
    }

    /// Where a `(kind, legacy key)` record LIVES on this volume: the tree
    /// that holds it and the key it is stored under — the kind's tree +
    /// the legacy key on a flat volume, the slot tree the forest key
    /// routes to + the forest key on a forest one. A READ-side locator
    /// (never mints — `None` when the key's slot has no tree), so the
    /// layout-blind harnesses (the fsck corruption seeds' leaf finder, the
    /// leaf-merge suite's height term) resolve leaves and read heights
    /// through the ONE router instead of indexing a per-kind tree that a
    /// forest does not have.
    pub fn record_locator(
        &self,
        kind: u8,
        legacy: &[u8],
    ) -> std::result::Result<Option<(Arc<KvTree>, Vec<u8>)>, KvError> {
        match &self.trees {
            TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                block_refs,
                block_map,
            } => {
                let tree = match kind {
                    TREE_INODES => Some(inodes),
                    TREE_DENTRIES => Some(dentries),
                    TREE_XATTRS => Some(xattrs),
                    super::record::TREE_BLOCK_REFS => block_refs.as_ref(),
                    super::record::TREE_BLOCK_MAP => block_map.get(),
                    _ => None,
                };
                Ok(tree.map(|t| (Arc::clone(t), legacy.to_vec())))
            }
            TreeSet::Forest { forest, .. } => {
                Ok(forest.route_read(kind, legacy)?.map(|r| (r.tree, r.key)))
            }
        }
    }

    /// A RAW page of slot tree `slot` — forest keys as stored, every kind,
    /// nothing decoded or skipped — fsck's forest C1 walk (the one reader
    /// that must SEE a key the codec refuses, to report it). `Ok(empty)`
    /// when the slot has no tree.
    pub async fn slot_tree_range_raw(
        &self,
        slot: super::record::ForestSlot,
        start: &[u8],
        end: &[u8],
        max: usize,
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        match self.forest().and_then(|f| f.tree(slot)) {
            Some(tree) => tree.range(start, end, max).await,
            None => Ok(Vec::new()),
        }
    }

    /// Raw point lookup of a forest key in slot tree `slot` (fsck's C1
    /// re-check of an undecodable key).
    pub async fn slot_tree_lookup_raw(
        &self,
        slot: super::record::ForestSlot,
        key: &[u8],
    ) -> std::result::Result<Option<Bytes>, KvError> {
        match self.forest().and_then(|f| f.tree(slot)) {
            Some(tree) => tree.lookup(key).await,
            None => Ok(None),
        }
    }

    /// Raw `Delete` of a forest key in slot tree `slot` — fsck's C1
    /// rebuild-in-place for a record whose key the codec refuses (the
    /// kind-routed [`Self::delete_kind`] cannot name it).
    pub async fn slot_tree_delete_raw(
        &self,
        slot: super::record::ForestSlot,
        key: &[u8],
    ) -> std::result::Result<(), KvError> {
        match self.forest().and_then(|f| f.tree(slot)) {
            Some(tree) => tree.delete(key).await,
            None => Ok(()),
        }
    }

    /// Every slot tree's `(slot, live root)` in slot order — the forest
    /// suites' root census (empty on a flat volume).
    pub fn forest_roots(&self) -> Vec<(super::record::ForestSlot, RootPtr)> {
        self.forest()
            .map(|f| {
                f.slot_trees()
                    .into_iter()
                    .map(|(s, t)| (s, t.root()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The tail the newest WRITTEN ledger record names (replay starts
    /// there) — what a checkpoint's covering argument is checked against.
    pub fn ledger_tail(&self) -> u64 {
        self.last_ledger_tail.load(Ordering::Acquire)
    }

    /// A forest census for the stats inode and the suites: how many slot
    /// trees exist and how many guest roots tree 0 currently names.
    pub fn forest_census(&self) -> Option<ForestCensus> {
        let f = self.forest()?;
        Some(ForestCensus {
            slot_trees: f.slot_tree_count() as u64,
            control_records: f.published_count() as u64,
            minted: f.minted(),
            root_publishes: f.publishes(),
        })
    }

    /// The flat per-kind tree for `id` — FLAT volumes only (the forest
    /// routes by KEY, never by kind alone). Borrowed: the flat hot path
    /// pays no refcount traffic (the DARK pin is a COST pin too — W-6's
    /// shared-line law).
    fn flat_tree(&self, id: u8) -> &KvTree {
        match &self.trees {
            TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                block_refs,
                block_map,
            } => match id {
                TREE_INODES => inodes,
                TREE_DENTRIES => dentries,
                TREE_XATTRS => xattrs,
                // Spec §6.2 item 1: staged only by the layout-commit paths,
                // and only when the tree is engaged (`block_refs_engaged`
                // gates every staging site) — an absent tree here means a
                // caller staged accounting onto a volume that has none.
                super::record::TREE_BLOCK_REFS => block_refs
                    .as_ref()
                    .expect("block-reference records staged on a volume without incompat bit 9"),
                // PB-class files, PR 1: unreachable un-engaged — the staging
                // seam refuses non-empty map ops loud BEFORE a tx exists
                // (`set_layout_and_size_with_map`), unlike the block_refs
                // silent-skip.
                super::record::TREE_BLOCK_MAP => block_map
                    .get()
                    .expect("block-map records staged on a volume without incompat bit 16"),
                _ => unreachable!("kv commits stage only the §4.2 trees"),
            },
            TreeSet::Forest { .. } => {
                unreachable!("a forest volume routes records by key, never by kind alone")
            }
        }
    }

    /// The tree a STAGED / journaled / replayed record belongs to: its
    /// kind's tree on a flat volume (BORROWED — no refcount traffic on
    /// the shipped hot path); on a forest volume the slot tree its
    /// (forest-form) key names — minted on the slot's first record — or
    /// tree 0 for a [`TREE_CONTROL`] tag. The ONE routing point of the
    /// commit pipelines and replay.
    async fn tree_for_record(
        &self,
        tag_tree_id: u8,
        key: &[u8],
    ) -> std::result::Result<TreeRef<'_>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => Ok(TreeRef::Borrowed(self.flat_tree(tag_tree_id))),
            TreeSet::Forest { forest, .. } => {
                if tag_tree_id == super::record::TREE_CONTROL {
                    return Ok(TreeRef::Borrowed(forest.control()));
                }
                let slot = forest.slot_of_forest_key(key)?;
                forest
                    .slot_or_mint(slot, &self.mint_context_for(slot))
                    .await
                    .map(TreeRef::Owned)
            }
        }
    }

    /// The tree a cache node belongs to (the checkpoint flush pass and the
    /// heap-admission root check): by header id on a flat volume; on a
    /// forest volume tree 0 by id, a slot tree by its owner stamp.
    pub(super) fn tree_of_node(
        &self,
        node: &CachedNode,
    ) -> std::result::Result<TreeRef<'_>, KvError> {
        match &self.trees {
            TreeSet::Flat {
                inodes,
                dentries,
                xattrs,
                block_refs,
                block_map,
            } => {
                let tree: Option<&KvTree> = match node.tree_id() {
                    TREE_INODES => Some(inodes),
                    TREE_DENTRIES => Some(dentries),
                    TREE_XATTRS => Some(xattrs),
                    super::record::TREE_BLOCK_REFS => block_refs.as_deref(),
                    super::record::TREE_BLOCK_MAP => block_map.get().map(|t| &**t),
                    _ => None,
                };
                tree.map(TreeRef::Borrowed).ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "node {:#x} carries tree id {} — no mounted tree",
                        node.addr(),
                        node.tree_id()
                    ))
                })
            }
            TreeSet::Forest { forest, .. } => forest.tree_for_node(node).map(TreeRef::Owned),
        }
    }

    /// The journal payload ONE staged record of `kind` with a `value_len`
    /// value costs on THIS volume — the admission's framing over the key
    /// as STAGED: the legacy key on a flat volume, one kind byte longer on
    /// a forest. Every entry planner (the corpse sweep's destroy chunks,
    /// the reclaim grouping) prices with this, so a planned entry fits by
    /// the arithmetic the commit enforces on either layout.
    pub fn staged_frame_len(&self, kind: u8, value_len: usize) -> u64 {
        let legacy = match kind {
            TREE_INODES => INODE_KEY_LEN,
            TREE_DENTRIES => super::record::DENTRY_KEY_LEN,
            TREE_XATTRS => XATTR_KEY_LEN,
            super::record::TREE_BLOCK_REFS => super::block_refs::BLOCK_REF_KEY_LEN,
            super::record::TREE_BLOCK_MAP => super::block_map::BLOCK_MAP_KEY_LEN,
            _ => 0,
        };
        let framed = match &self.trees {
            TreeSet::Flat { .. } => legacy,
            TreeSet::Forest { .. } => legacy + 1,
        };
        super::journal::record_frame_len(framed, value_len)
    }

    /// The journal payload `n` reference-release `Delete`s stage on THIS
    /// volume (RECLAIM-ATOMIC's release term, framed as staged here).
    pub fn release_records_bytes(&self, n: usize) -> u64 {
        n as u64 * self.staged_frame_len(super::record::TREE_BLOCK_REFS, 0)
    }

    /// The key a `(kind, legacy key)` pair is STAGED and JOURNALED under:
    /// the legacy key verbatim on a flat volume, the §5.2.1 forest key on
    /// a forest volume (the kind byte inserted once here; the tag stays
    /// the kind, so the journal wire is the shipped one).
    fn stage_key(&self, kind: u8, legacy: Vec<u8>) -> std::result::Result<Vec<u8>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => Ok(legacy),
            // Tree 0's records are keyed by their own codec (`slot_state`),
            // never framed — a control-tree committer (PR 3/4's manager
            // verbs) stages them verbatim.
            TreeSet::Forest { .. } if kind == super::record::TREE_CONTROL => Ok(legacy),
            TreeSet::Forest { .. } => super::record::forest_key(kind, &legacy).map_err(|e| {
                super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                e
            }),
        }
    }

    /// A committed batch's records with their keys back in LEGACY form —
    /// what the migration tee's consumer re-reads by. Borrows on a flat
    /// volume (identity); re-frames on a forest one.
    fn legacy_recs<'r>(
        &self,
        recs: &'r [(u8, Record)],
    ) -> std::result::Result<Cow<'r, [(u8, Record)]>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => Ok(Cow::Borrowed(recs)),
            TreeSet::Forest { .. } => {
                let mut out = Vec::with_capacity(recs.len());
                for (tree_id, r) in recs {
                    let mut r = r.clone();
                    if *tree_id != super::record::TREE_CONTROL {
                        r.key = super::record::split_forest_key(&r.key)?.1;
                    }
                    out.push((*tree_id, r));
                }
                Ok(Cow::Owned(out))
            }
        }
    }

    /// What a lazy slot-tree mint needs — the floor is the ring HEAD at
    /// the mint: every record the new tree will hold reserves at or past
    /// it, so the checkpoint tail must not pass it until tree 0 names the
    /// root ([`super::forest::SlotTrees::unpublished_root_floors`]).
    fn mint_context_for(&self, slot: super::record::ForestSlot) -> super::forest::MintContext<'_> {
        // A slot a declared appender leases mints INSIDE that appender's
        // grant, with its ring's head as the floor (§5.3.3).
        let region = self.smo_region_of_slot(slot);
        let floor = region
            .as_ref()
            .map_or_else(|| self.ring.core().head(), |r| r.ring.core().head());
        super::forest::MintContext {
            cache: &self.cache,
            seq: self.seq_ref(),
            alloc: &self.alloc,
            floor,
            // The runtime mint is USER growth; a mount that may not write
            // never reaches one (the write gate refuses upstream) — the
            // policy is the belt to that brace.
            policy: if self.read_only {
                super::forest::MintPolicy::Refuse
            } else {
                super::forest::MintPolicy::User
            },
            region,
        }
    }

    /// The SMO scope of `slot`'s lessee: its ring and grant when a
    /// declared region leases it, `None` for the manager's own slots.
    pub(super) fn smo_region_of_slot(
        &self,
        slot: super::record::ForestSlot,
    ) -> Option<super::tree::SmoRegion> {
        let set = self.appenders.as_ref().filter(|a| a.is_partitioned())?;
        let id = set.region_of_slot(slot);
        if id == 0 {
            return None;
        }
        let r = set.region(id)?;
        Some(super::tree::SmoRegion {
            appender_id: id,
            ring: r.ring(),
            grant: Arc::clone(&r.grant),
        })
    }

    /// The volume-shared record/node seq source, borrowed (see
    /// [`Self::seq_handle`]).
    fn seq_ref(&self) -> &Arc<AtomicU64> {
        match &self.trees {
            TreeSet::Flat { inodes, .. } => inodes.seq_ref(),
            TreeSet::Forest { forest, .. } => forest.control().seq_ref(),
        }
    }

    /// The volume-shared record/node seq source (every tree clones the
    /// same `Arc`; one clone here, never a tree-set walk — this sits on
    /// the per-record commit path of a forest volume).
    fn seq_handle(&self) -> Arc<AtomicU64> {
        match &self.trees {
            TreeSet::Flat { inodes, .. } => inodes.seq_handle(),
            TreeSet::Forest { forest, .. } => forest.control().seq_handle(),
        }
    }

    /// Latch-free point lookup of a `(kind, legacy key)` — the ONE read
    /// primitive both layouts serve.
    pub async fn lookup_kind(
        &self,
        kind: u8,
        legacy: &[u8],
    ) -> std::result::Result<Option<Bytes>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => self.flat_tree(kind).lookup(legacy).await,
            TreeSet::Forest { forest, .. } => forest.lookup(kind, legacy).await,
        }
    }

    /// The durable delta-chain probe of a `(kind, legacy key)`
    /// ([`KvTree::delta_chain_probe`]) on either layout.
    pub async fn delta_chain_probe_kind(
        &self,
        kind: u8,
        legacy: &[u8],
    ) -> std::result::Result<(u32, Option<(u64, u64)>), KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => self.flat_tree(kind).delta_chain_probe(legacy).await,
            TreeSet::Forest { forest, .. } => forest.delta_chain_probe(kind, legacy).await,
        }
    }

    /// Range scan of `kind` over the inclusive LEGACY window `[start,
    /// end]`, at most `max` records, keys in legacy form ([`KvTree::range`]
    /// on a flat volume; the slot-ordered kind-filtered walk on a forest).
    pub async fn range_kind(
        &self,
        kind: u8,
        start: &[u8],
        end: &[u8],
        max: usize,
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => self.flat_tree(kind).range(start, end, max).await,
            TreeSet::Forest { forest, .. } => forest.range(kind, start, end, max).await,
        }
    }

    /// Direct (un-journaled) `Put` of a `(kind, legacy key)` — the
    /// offline repair verbs' primitive ([`KvTree::insert`]).
    pub async fn insert_kind(
        &self,
        kind: u8,
        legacy: &[u8],
        value: impl Into<Bytes>,
    ) -> std::result::Result<(), KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => self.flat_tree(kind).insert(legacy, value).await,
            TreeSet::Forest { forest, .. } => {
                let (slot, key) = forest.route_key(kind, legacy)?;
                let tree = forest
                    .slot_or_mint(slot, &self.mint_context_for(slot))
                    .await?;
                tree.insert(&key, value).await
            }
        }
    }

    /// Direct (un-journaled) `Delete` of a `(kind, legacy key)` — the
    /// offline repair verbs' primitive ([`KvTree::delete`]).
    pub async fn delete_kind(&self, kind: u8, legacy: &[u8]) -> std::result::Result<(), KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => self.flat_tree(kind).delete(legacy).await,
            TreeSet::Forest { forest, .. } => match forest.route_read(kind, legacy)? {
                Some(r) => r.tree.delete(&r.key).await,
                None => Ok(()), // nothing was ever written to that slot
            },
        }
    }

    /// `true` ⇔ durable block-reference accounting is engaged on this
    /// volume (incompat bit 9 present and the mount may write). `false`
    /// means every block-ownership answer is derived state, exactly as it
    /// was before the bit existed.
    pub fn block_refs_engaged(&self) -> bool {
        match &self.trees {
            TreeSet::Flat { block_refs, .. } => block_refs.is_some(),
            TreeSet::Forest { block_refs, .. } => *block_refs,
        }
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
    /// [`Self::block_refs_engaged`]. On a forest volume the by-block
    /// prefix is probed in EVERY slot tree (a reference lives in the
    /// referencing ino's slot — §5.4.2), which the kind-routed range walk
    /// does by construction.
    pub async fn block_ref_scan(
        &self,
        vol_tag: u64,
    ) -> std::result::Result<Vec<super::block_refs::BlockRef>, KvError> {
        if !self.block_refs_engaged() {
            return Ok(Vec::new());
        }
        let (start, end) = super::block_refs::volume_range(vol_tag);
        let mut out = Vec::new();
        for (k, v) in self.block_refs_window(&start, &end).await? {
            // Decode both halves: a malformed accounting record is
            // loud corruption, never a silently skipped reference
            // (an under-count is the exact failure this structure
            // exists to prevent).
            let r = super::block_refs::decode_block_ref_key(&k)?;
            let _ = super::block_refs::decode_block_ref_value(&v)?;
            out.push(r);
        }
        Ok(out)
    }

    /// Every block-reference record in the LEGACY window `[start, end]`
    /// (`volume_range` / `block_range`), keys in legacy form: paged by
    /// legacy cursor on the flat tree; on a forest the union over EVERY
    /// slot tree, each paged with its own cursor
    /// ([`super::forest::SlotTrees::refs_window`]) — a legacy cursor is
    /// not a forest resume point for the block-major refs family.
    async fn block_refs_window(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        match &self.trees {
            TreeSet::Flat { .. } => {
                let tree = self.flat_tree(super::record::TREE_BLOCK_REFS);
                let mut out = Vec::new();
                let mut cursor = start.to_vec();
                loop {
                    let page = tree.range(&cursor, end, 512).await?;
                    let Some((last_key, _)) = page.last() else {
                        break;
                    };
                    cursor = key_successor(last_key);
                    out.extend(page);
                }
                Ok(out)
            }
            TreeSet::Forest { forest, .. } => forest.refs_window(start, end).await,
        }
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
        if !self.block_refs_engaged() {
            return Ok(0);
        }
        let (start, end) = super::block_refs::block_range(vol_tag, block_idx);
        let mut population = 0usize;
        for (k, v) in self.block_refs_window(&start, &end).await? {
            // Decode both halves (the block_ref_scan discipline): a
            // malformed accounting record is loud corruption, never a
            // silently skipped — or silently COUNTED — reference.
            let _ = super::block_refs::decode_block_ref_key(&k)?;
            let _ = super::block_refs::decode_block_ref_value(&v)?;
            population += 1;
        }
        Ok(population)
    }

    /// `true` ⇔ the block-map tree is engaged on this volume (incompat
    /// bit 16 present and the mount may write). `false` means every
    /// layout head stays inline/`indirect:`, exactly as before the bit
    /// existed — and staging a map record refuses loud
    /// ([`Self::set_layout_and_size_with_map`]).
    pub fn block_map_tree_engaged(&self) -> bool {
        match &self.trees {
            TreeSet::Flat { block_map, .. } => block_map.get().is_some(),
            TreeSet::Forest { block_map, .. } => block_map.load(Ordering::Acquire),
        }
    }

    /// PB-class files, PR 1 (design §3 read law, the exact-key half):
    /// resolve `(ino, block_index)` through the block-map tree — a point
    /// lookup, then (PR 6a, design §12/A6) the bounded run-floor probe on
    /// an exact miss. Returns the RECORD's key index alongside the entry:
    /// an exact hit answers `(block_index, entry)`, a covering run
    /// answers `(start_index, run)` and the caller derives the per-index
    /// binding from `block_index − start_index` (the offset/stamp
    /// arithmetic lives with the router's stride census). The read law is
    /// exact-supersedes-covering-run BY CONSTRUCTION here: the exact
    /// lookup runs first, the floor probe only on its miss.
    ///
    /// `Ok(None)` on a volume with no engaged tree — a caller that must
    /// distinguish asks [`Self::block_map_tree_engaged`] (the
    /// `block_ref_scan` posture). The reserved index refuses (A5).
    pub async fn get_block_mapping(
        &self,
        ino: Ino,
        block_index: u32,
    ) -> std::result::Result<Option<(u32, super::block_map::MapEntry)>, KvError> {
        match self.block_map_exact(ino, block_index).await? {
            Some(hit) => Ok(Some(hit)),
            None => self.block_map_floor(ino, block_index).await,
        }
    }

    /// The EXACT half of [`Self::get_block_mapping`] alone: the record
    /// keyed at `(ino, block_index)` or `None` — no floor probe. The
    /// finding-46 WINDOW train's probe: a miss there is either a fresh
    /// index (no record can cover it — RAM is whole-map authority) or an
    /// index inside a run, and BOTH stage the same superseding point Put
    /// under the §2 read law, so the RUN_LEN_MAX-bounded floor scan (up
    /// to ~4 k records on a point-dense map) buys nothing per claim.
    pub async fn block_map_exact(
        &self,
        ino: Ino,
        block_index: u32,
    ) -> std::result::Result<Option<(u32, super::block_map::MapEntry)>, KvError> {
        if !self.block_map_tree_engaged() {
            return Ok(None);
        }
        super::META_KV_BLOCK_MAP_LOOKUP_EXACT.fetch_add(1, Ordering::Relaxed);
        let key = super::block_map::block_map_key(ino, block_index)?;
        match self
            .lookup_kind(super::record::TREE_BLOCK_MAP, &key)
            .await?
        {
            // A malformed mapping is loud corruption, never a silently
            // skipped block (a wrong resolve is the failure the tree
            // exists to prevent).
            Some(v) => Ok(Some((
                block_index,
                super::block_map::decode_block_map_value(&v)?,
            ))),
            None => Ok(None),
        }
    }

    /// The A6 run-floor probe (design §3/§12): the tree has no
    /// floor/predecessor primitive, so the exact-miss arm is ONE bounded
    /// forward range `[ino‖N−(RUN_LEN_MAX−1), ino‖N−1]` take-last with the
    /// owner-ino prefix check (`index_range` bounds are ino-exact by
    /// construction; the decode re-checks), then the coverage check —
    /// `Some` only when the floor record is a run whose span reaches `N`.
    /// `RUN_LEN_MAX` bounds the scan BECAUSE it bounds every emitted run:
    /// a record keyed further back can never cover `N`.
    pub async fn block_map_floor(
        &self,
        ino: Ino,
        block_index: u32,
    ) -> std::result::Result<Option<(u32, super::block_map::MapEntry)>, KvError> {
        if !self.block_map_tree_engaged() {
            return Ok(None);
        }
        if block_index == 0 {
            // No index precedes 0 — a covering run would BE the exact hit.
            return Ok(None);
        }
        super::META_KV_BLOCK_MAP_LOOKUP_FLOOR.fetch_add(1, Ordering::Relaxed);
        let lo_idx = block_index.saturating_sub(super::block_map::RUN_LEN_MAX - 1);
        let lo = super::block_map::block_map_key(ino, lo_idx)?;
        let hi = super::block_map::block_map_key(ino, block_index - 1)?;
        // Take the LAST **covering** record over the bounded window,
        // paged forward (the range primitive is forward-only). Not the
        // last record plain: a superseding point INSIDE a run's span (the
        // §2 read law's own legal shape — a claims adoption) sits between
        // the run and `N`, and taking it would read a covered index as
        // absent. Take-LAST among coverers is what the mid-train
        // crash-window analysis needs too: where two runs transiently
        // overlap (the shrink ordering), the later start is the newer
        // truth.
        let mut covering: Option<(u32, super::block_map::MapEntry)> = None;
        let mut cursor: Vec<u8> = lo.to_vec();
        loop {
            let page = self
                .range_kind(super::record::TREE_BLOCK_MAP, &cursor, &hi, 512)
                .await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            let full = page.len() >= 512;
            let next = key_successor(last_key);
            for (k, v) in &page {
                let (owner, idx) = super::block_map::decode_block_map_key(k)?;
                if owner != ino {
                    return Err(KvError::Corrupt(format!(
                        "block-map floor probe for ino {ino} returned a record owned by \
                         {owner}"
                    )));
                }
                let entry = super::block_map::decode_block_map_value(v)?;
                // Coverage: `idx + run_len > N` (`idx < N` by the range
                // bound; point/string records never cover — run_len 1).
                if u64::from(idx) + u64::from(entry.run_len()) > u64::from(block_index) {
                    covering = Some((idx, entry));
                }
            }
            if !full {
                break;
            }
            cursor = next;
        }
        Ok(covering)
    }

    /// A bounded window of `ino`'s mappings from `from_index` upward, in
    /// index order — the tree.rs `range` primitive over the per-ino
    /// prefix ([`super::block_map::index_range_from`]'s exact bounds, so
    /// one ino's window can never bleed into its neighbour's). PR 3/4/5
    /// consume it (read windows, walkers, the A1/A2 sweeps' scan side).
    ///
    /// `Ok(vec![])` on a volume with no engaged tree, like
    /// [`Self::get_block_mapping`].
    pub async fn block_map_range(
        &self,
        ino: Ino,
        from_index: u32,
        max: usize,
    ) -> std::result::Result<Vec<(u32, super::block_map::MapEntry)>, KvError> {
        if !self.block_map_tree_engaged() {
            return Ok(Vec::new());
        }
        super::META_KV_BLOCK_MAP_LOOKUP_RANGE.fetch_add(1, Ordering::Relaxed);
        let (lo, hi) = super::block_map::index_range_from(ino, from_index);
        let page = self
            .range_kind(super::record::TREE_BLOCK_MAP, &lo, &hi, max)
            .await?;
        super::META_KV_BLOCK_MAP_RANGE_RECORDS.fetch_add(page.len() as u64, Ordering::Relaxed);
        let mut out = Vec::with_capacity(page.len());
        for (k, v) in &page {
            let (owner, index) = super::block_map::decode_block_map_key(k)?;
            if owner != ino {
                // Unreachable through the exact bounds — reaching it
                // means the bounds law broke, which must be loud.
                return Err(KvError::Corrupt(format!(
                    "block-map range for ino {ino} returned a record owned by {owner}"
                )));
            }
            out.push((index, super::block_map::decode_block_map_value(v)?));
        }
        Ok(out)
    }

    /// Is `ino`'s crossing train in flight on this volume? The fsck
    /// **C11** map-plane class's zero-FP registry shield (design A3: an
    /// incomplete pass records no verdict for a registered ino — the
    /// C2/C3 `inflight_exempted` pattern); the PR 2 contract tests
    /// consume it too.
    pub fn crossing_in_flight(&self, ino: Ino) -> bool {
        self.crossing_inflight.contains_sync(&ino)
    }

    /// TEST SEAM (the C11 shield contracts, `tests/kvmap_walker_tests.rs`):
    /// register `ino` in the crossing registry exactly as the train does,
    /// returning the same RAII guard. Production registration happens only
    /// inside the held-4a migration train; the seam exists because the A3
    /// shield's zero-FP contract must be pinned deterministically, not by
    /// racing a live train.
    pub fn test_register_crossing(&self, ino: Ino) -> impl Drop + '_ {
        CrossingGuard::register(&self.crossing_inflight, ino)
    }

    /// Every distinct owner ino with at least one tree-7 map record — the
    /// fsck **C11** orphan census's tree side (design §3 fsck: orphan map
    /// records are invisible to every other walker, because they all reach
    /// tree 7 only THROUGH a live layout head). Skip-scan: one bounded
    /// range probe per owner, then the cursor jumps to the next owner's
    /// range start — O(distinct owners), never O(records), so a PB-class
    /// file contributes one probe. `Ok(vec![])` on a volume with no
    /// engaged tree, like [`Self::block_map_range`].
    pub async fn block_map_owner_scan(&self) -> std::result::Result<Vec<Ino>, KvError> {
        if !self.block_map_tree_engaged() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut cursor = [0u8; super::block_map::BLOCK_MAP_KEY_LEN];
        let end = [0xFFu8; super::block_map::BLOCK_MAP_KEY_LEN];
        loop {
            let page = self
                .range_kind(super::record::TREE_BLOCK_MAP, &cursor, &end, 1)
                .await?;
            let Some((k, _)) = page.first() else {
                break;
            };
            let (owner, _idx) = super::block_map::decode_block_map_key(k)?;
            out.push(owner);
            let Some(next) = owner.checked_add(1) else {
                break;
            };
            // Index 0 of the successor owner is always encodable.
            cursor = super::block_map::block_map_key(next, 0)?;
        }
        Ok(out)
    }

    /// The one-time **bit-16 ratchet + tree-7 mint** (PR 2, the
    /// `layout_deltas_ready` shape): the bit is durable — and barriered —
    /// BEFORE the volume's first map record can be (design §2, the KD-14
    /// bit-before-durable-record ordering), and the tree root is minted
    /// live (the open path's post-stamp first-mount mint, run at runtime;
    /// stamp-then-crash is inert — the next mount mints from the ledger
    /// gap and replay folds any journaled record into the fresh root by
    /// key). `false` = the ratchet could not complete — the caller falls
    /// back to the legacy indirect-blob arm (never block writes on it).
    ///
    /// Reachable un-engaged only through the SHIPPED crossing's owner
    /// executor: local crossings pre-probe [`Self::block_map_tree_engaged`]
    /// (an un-stamped local volume keeps the legacy arm — stamping a
    /// volume is otherwise an explicit act), while a co-writer cannot
    /// read the owner's superblock, so the owner self-arms at its first
    /// served crossing.
    pub async fn block_map_tree_ready(&self) -> bool {
        if self.block_map_tree_engaged() {
            return true;
        }
        if self.read_only {
            return false;
        }
        let _g = self.block_map_ratchet.lock().await;
        if self.block_map_tree_engaged() {
            return true;
        }
        match super::superblock::set_block_map_tree_bit(&self.path).await {
            Ok(_newly_set) => {
                // Barrier the sector-0 write before any map record can
                // become durable (same device — one fdatasync covers it).
                if let Err(e) = self.sync_device().await {
                    log::warn!(
                        "meta volume {}: block-map-tree ratchet barrier failed ({e}); \
                         staying on the indirect-blob path",
                        self.path.display()
                    );
                    return false;
                }
                // Under the forest a block map is a record KIND inside
                // the slot trees (design-symmetric-metadata §5.4.2): no
                // root to mint — the durable bit alone engages it.
                let block_map = match &self.trees {
                    TreeSet::Flat { block_map, .. } => block_map,
                    TreeSet::Forest { block_map, .. } => {
                        block_map.store(true, Ordering::Release);
                        log::info!(
                            "meta volume {}: incompat bit 16 (block-map tree) stamped on a \
                             forest volume — map records route into the slot trees",
                            self.path.display()
                        );
                        return true;
                    }
                };
                let seq = self.seq_handle();
                let mut smo = self.smo.lock().await;
                match KvTree::create(
                    self.cache.clone(),
                    &mut smo,
                    super::record::TREE_BLOCK_MAP,
                    seq,
                )
                .await
                {
                    Ok(tree) => {
                        let _ = block_map.set(Arc::new(tree));
                        log::info!(
                            "meta volume {}: incompat bit 16 (block-map tree) stamped and \
                             the tree-7 root minted (the first crossing's ratchet)",
                            self.path.display()
                        );
                        true
                    }
                    Err(e) => {
                        log::warn!(
                            "meta volume {}: could not mint the block-map tree root ({e}); \
                             staying on the indirect-blob path (the stamped bit is inert \
                             with zero records — the next mount mints)",
                            self.path.display()
                        );
                        false
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "meta volume {}: could not stamp KV_BLOCK_MAP_TREE ({e}); \
                     staying on the indirect-blob path",
                    self.path.display()
                );
                false
            }
        }
    }

    /// The §12 claims×runs dissolve's survivor record: index `start +
    /// delta` of a dissolving run, materialized as the router-true key
    /// STRING (the always-decodable PR-2 form; the next local publish
    /// upgrades and re-coalesces it). An unresolvable arithmetic refuses
    /// the train loud — dropping a peer's live binding is the mass-delete
    /// class the claims law exists to prevent.
    fn run_survivor_entry(
        ino: Ino,
        run: &super::block_map::MapEntry,
        delta: u32,
        entry_key: &(dyn Fn(&super::block_map::MapEntry, u32) -> Option<String> + Send + Sync),
    ) -> Result<super::block_map::MapEntry> {
        match entry_key(run, delta) {
            Some(k) => Ok(super::block_map::MapEntry::String(k.into_bytes())),
            None => Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "claims-scoped map train for ino {ino}: cannot materialize index +{delta} \
                 of a dissolving run — no mounted volume resolves its arithmetic; refusing \
                 rather than dropping a peer's live binding (design §12)"
            ))),
        }
    }

    /// **The crossing/migration train** (PR 2, design §3 + Rev 1.1 #1):
    /// reconcile `ino`'s tree-7 records to exactly `entries` and flip the
    /// layout head — under ONE exclusive 4a I-guard held across the whole
    /// train (the stripes are non-reentrant, so no existing verb can
    /// compose this):
    ///
    /// 1. `write_gate` → [`Self::block_map_tree_ready`] (`Ok(None)` = not
    ///    ready — the caller falls back to the legacy blob arm, never
    ///    blocks the write);
    /// 2. register the ino in the in-flight crossing registry (A3);
    /// 3. the A1 residue sweep, generalized to a DIFF: one paged scan of
    ///    the ino's existing records staged against the desired map —
    ///    Deletes for stale indices, Puts for new/changed bindings (a
    ///    changed binding is ONE Put; same-key supersede). On a healthy
    ///    first crossing the scan is one empty leaf descent and the diff
    ///    is the whole map;
    /// 4. chunked commits (`chunk` ops per tx, each co-owning the guard —
    ///    the M7 `hold_guards` law) for everything but the tail;
    /// 5. the FLIP as the final tx while the guard is still held: layout
    ///    head Put + inode(size) Put + `block_refs` + the tail chunk —
    ///    the `set_layout_and_size_with_map` one-tx shape.
    ///
    /// Crash windows: on a FIRST crossing every pre-flip record is
    /// invisible (the head still names the inline/blob map) and the next
    /// crossing's diff deletes/reuses it. On a RE-train of a live
    /// `kvmap:` head, pre-flip Deletes remove bindings whose blocks the
    /// punch/truncate already freed (deleting early is the safe
    /// direction — a stale record naming a freed block is the A1 class)
    /// and pre-flip Puts expose write-through bytes an unacked publish
    /// wrote (legal either way after a crash; the retried save re-diffs
    /// and converges).
    ///
    /// **Modes (PR 5b, design §11 law b)**: `claims: None` is the
    /// whole-map diff — delete-by-absence, legal ONLY where RAM is
    /// whole-map authority (the local save, Rev 1.3 #2, and the
    /// serve-window episode compose). `claims: Some` is the
    /// CLAIMS-SCOPED train every SHIPPED sticky-head save rides: adopt a
    /// Put only under a take claim, delete only under a
    /// release-without-take claim whose index the shipped map does not
    /// name (the f35 removal law) — a stale whole-map ship can never
    /// erase a peer's fresh bindings. With the rung-19 resolver armed the
    /// claims train also RECOMPUTES its staged accounting as the
    /// tree→composed swap diff (the f36b twin: the shipper's stale frame
    /// may mis-name the displaced binding) and returns the released set
    /// for the caller's post-commit free ladder. `claims.window` (finding
    /// 46) is the LOCAL whole-map-authority publish WINDOW: take claims
    /// only, exact-lookup probes only, no recompute (the caller's frame
    /// is exact and commits verbatim) — the steady-state streaming save's
    /// O(window) form of the same train.
    ///
    /// Every committed train on the multi-writer plane bumps the head's
    /// map GENERATION (§11's belt; solo volumes never mint one — their
    /// heads stay byte-identical), and a claims train carrying a
    /// `base_gen` that does not match the durable head refuses
    /// retried-class before anything stages.
    ///
    /// Errors are never-lossy for the caller: nothing here consumes the
    /// RAM map or the `block_refs` accounting — the routing save's
    /// refill discipline re-owns both.
    #[allow(clippy::too_many_arguments)]
    pub async fn migrate_block_map_train(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        entries: &[(u32, super::block_map::MapEntry)],
        chunk: usize,
        claims: Option<&MapTrainClaims>,
        refs_owner: Ino,
        entry_key: &(dyn Fn(&super::block_map::MapEntry, u32) -> Option<String> + Send + Sync),
        cursor_floor: u32,
        ref_for: &(dyn Fn(&str, u32) -> Option<super::block_refs::BlockRef> + Send + Sync),
    ) -> Result<Option<MapMigrateOutcome>> {
        self.write_gate()?;
        if !self.block_map_tree_ready().await {
            return Ok(None);
        }
        let chunk = chunk.max(1);
        let _crossing = CrossingGuard::register(&self.crossing_inflight, ino);
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);

        // PR 5b: the durable head, read under the held 4a — the gen law's
        // input and the claims train's base identity. `None` = no kvmap
        // head (a crossing/conversion — the establishing train). PR 6b:
        // the same read carries the A2 sweep cursor — a live cursor can
        // only exist in a kvmap head, and kvmap heads never regress
        // (sticky, pinned), so the establishing train structurally never
        // meets one: the re-cross-vs-sweep compose is trivial by law.
        let mut durable_size = 0u64;
        let durable_head: Option<super::block_map::KvmapHead> =
            match self.getxattr(ino, "layout").await? {
                Some(bytes) => crate::layout_wire::decode_layout_any(&bytes)
                    .ok()
                    .and_then(|l| {
                        durable_size = l.size;
                        l.block_map_id
                    })
                    .and_then(|id| super::block_map::parse_kvmap_head(&id).ok()),
                None => None,
            };
        let durable_gen: Option<u64> = durable_head.as_ref().map(|h| h.gen);
        let durable_cursor: Option<u32> = durable_head.as_ref().and_then(|h| h.sweep_cursor);
        let shipped_size = size;
        // PR 6c-i: LOCAL claims trains (the §14 overlay saves) are the
        // size/cursor AUTHORITY exactly like the whole-map train they
        // replace — the f35 size law and the §12b #5 cursor refusal are
        // the SHIPPED trains' (a peer's view may lag; the local save
        // cannot lag itself, it holds the 4a).
        let served_claims = claims.is_some_and(|c| c.served);
        // The f35 size law on the claims arm: a stale shipper's size never
        // regresses a peer's growth (truncation is the setattr plane's;
        // the local arms keep caller authority verbatim).
        let size = if served_claims {
            size.max(durable_size)
        } else {
            size
        };
        if let Some(c) = claims {
            let Some(g) = durable_gen else {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "claims-scoped map train for ino {ino} on a non-kvmap durable head — \
                     the claims law composes onto tree records (design §11 law b); a \
                     crossing rides the whole-map train"
                )));
            };
            if let Some(base) = c.base_gen {
                if base != g {
                    crate::meta_ship::publish::note_map_refused();
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "layout delta base unusable: kvmap map generation — ino {ino}'s \
                         shipped train carries base gen {base} but the durable head is at \
                         gen {g} (design §11's belt): refetch and recompose (map_refused)"
                    )));
                }
            }
            // PR 6b (design §3/A2): a SERVED claims train meeting a live
            // sweep cursor composes only STRICTLY BELOW it and never
            // grows the size past the truncate — a size-raising ship or a
            // claim at a shadowed index would re-expose residue the sweep
            // has not released (silent stale data, the A1 class).
            // Retried-class: the shipper refetches and recomposes; the
            // cursor clears when the sweep (or the authority's own extend
            // barrier) finishes. A LOCAL claims train (PR 6c-i) runs the
            // extend barrier below instead — it IS the authority.
            if let Some(k) = durable_cursor.filter(|_| c.served) {
                let claimed_high = c.take.iter().chain(c.release.iter()).any(|&i| i >= k)
                    || entries.iter().any(|(b, _)| *b >= k);
                if shipped_size > durable_size || claimed_high {
                    crate::meta_ship::publish::note_map_refused();
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "layout delta base unusable: kvmap sweep cursor — ino {ino}'s \
                         head carries a live A2 sweep cursor at {k} and the shipped \
                         train grows size ({shipped_size} > {durable_size}) or claims \
                         indices at/above it: refetch and recompose once the sweep \
                         drains (map_refused)"
                    )));
                }
            }
        }
        // PR 6b: the extend barrier (design §3/A2's write-during-sweep
        // law). With a live cursor K, every record at/above K is residue
        // and every live record sits below K — a whole-map publish whose
        // size now reaches past K (`cursor_floor = ceil(size/bs)`,
        // computed by the router) would otherwise re-expose residue in
        // [K, cursor_floor): reads clamp to size, so those stale records
        // become servable the instant the size commits. The barrier
        // sweeps exactly the re-exposed span — Deletes + reference
        // releases in chunked txs co-owning the held 4a (no cursor
        // advances: a crash re-runs record-true and idempotent) — and the
        // flip head carries the advanced cursor. Freed keys travel up for
        // the caller's post-commit reclaim (RES-1).
        let mut swept_records = 0u64;
        let mut swept_freed: Vec<String> = Vec::new();
        // The barrier ownership law (PR 6c-i): every LOCAL train —
        // whole-map or claims-scoped — barriers (it holds the 4a and owns
        // size); SERVED trains never do (they were refused above).
        let flip_cursor: Option<u32> = match (!served_claims, durable_cursor) {
            (true, Some(k)) => {
                // `bound == k` is the degenerate barrier: no growth, but
                // the left-boundary straddler (a run keyed below K whose
                // span crosses it) still dissolves — a shrink-superseding
                // diff Put would otherwise drop its tail's coverage with
                // the tail's references never released.
                let bound = cursor_floor.max(k);
                let mut pos = k;
                loop {
                    let pg = self
                        .kvmap_sweep_page(ino, pos, Some(bound), chunk, entry_key, ref_for)
                        .await?;
                    swept_records += pg.records;
                    swept_freed.extend(pg.freed);
                    if !pg.ops.is_empty() {
                        let mut tx = KvTx::new();
                        tx.stage_block_map(&pg.ops)?;
                        if self.block_refs_engaged() {
                            tx.stage_block_refs(&pg.rel);
                        }
                        tx.hold_guards(Arc::clone(&guards));
                        self.commit_tx(tx).await?;
                    }
                    match pg.resume {
                        Some(next) => pos = next,
                        None => break,
                    }
                }
                Some(bound)
            }
            (_, cursor) => cursor,
        };
        if !served_claims {
            if let Some(k) = flip_cursor {
                if let Some((b, _)) = entries.iter().find(|(b, _)| *b >= k) {
                    // Unreachable through the routing layer (the RAM map /
                    // overlay excludes shadowed residue and the barrier
                    // covers regrowth) — reaching it means a desired
                    // binding names an index the sweep owns, and
                    // committing it would strand its superseded record's
                    // reference.
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "local train for ino {ino} desires index {b} at/above the \
                         live A2 sweep cursor {k} (design §3): the cursor invariant \
                         broke upstream — refusing rather than stranding references"
                    )));
                }
            }
        }
        // The bump law: a committed train on the MULTI-WRITER plane —
        // a SERVED claims train (shipped by definition), any train on a
        // custody-armed authority, or a head whose gen was ever minted
        // (monotone survives a disarm/remount) — bumps; a solo volume's
        // heads stay byte-identical (dark by default). PR 6c-i (§14 S2
        // pre-fix b): claims-PRESENCE alone no longer mints — a solo
        // mount's LOCAL claims trains (the overlay saves) run per
        // publish, and keying on presence would gen-stamp every solo
        // partial-mode head.
        let committed_gen = match durable_gen {
            None => 0,
            Some(g) => {
                if claims.is_some_and(|c| c.served)
                    || g > 0
                    || crate::data_grant::custody_owner().is_some()
                {
                    g + 1
                } else {
                    0
                }
            }
        };
        // Re-stamp the flip head's generation — and, PR 6b, the A2 sweep
        // cursor: the DURABLE head owns the cursor (the caller's bytes
        // arrive cursor-less — the routing layer never authors one), so a
        // publish during a live sweep must carry it forward or the plan
        // is lost and the residue's records/references strand forever.
        // Gen 0 with no live cursor commits the caller's bytes verbatim
        // (the pre-belt byte-identity pin).
        let layout_restamped: Option<Vec<u8>> = if committed_gen > 0 || flip_cursor.is_some() {
            let mut head: crate::layout_wire::LayoutMetadata = bincode::deserialize(layout)
                .map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "map train for ino {ino}: undecodable flip head ({e}) — refusing \
                         to commit a generation-bearing train whose head cannot carry it"
                    ))
                })?;
            let parsed = head
                .block_map_id
                .as_deref()
                .and_then(|id| super::block_map::parse_kvmap_head(id).ok())
                .ok_or_else(|| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "map train for ino {ino}: the flip head is not a kvmap head — \
                         nothing staged"
                    ))
                })?;
            let flip_gen = if committed_gen > 0 {
                committed_gen
            } else {
                parsed.gen
            };
            if parsed.gen == flip_gen && parsed.sweep_cursor == flip_cursor {
                None
            } else {
                head.block_map_id = Some(
                    super::block_map::KvmapHead {
                        sweep_cursor: flip_cursor,
                        gen: flip_gen,
                    }
                    .encode(),
                );
                Some(bincode::serialize(&head).map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "map train for ino {ino}: flip-head re-encode failed: {e}"
                    ))
                })?)
            }
        } else {
            None
        };
        let layout: &[u8] = layout_restamped.as_deref().unwrap_or(layout);

        // The diff scan (step 3). Desired bindings arrive in RECORD form
        // (PR 3, Rev 1.3 #3: the routed layer encodes POINT for
        // undecorated keys, STRING for decorated/encoder-less arms;
        // PR 6a: local trains arrive run-coalesced — design §12), so
        // the match is record equality: a PR-2 STRING record whose key
        // now encodes POINT reads as a changed binding and upgrades on
        // this publish — a one-time rewrite, never a steady-state churn.
        //
        // Ops are staged as GROUPS the chunk packer keeps tx-atomic where
        // they fit (the A6 run-Put + point-Deletes one-tx coalesce law);
        // a group past the chunk splits IN ORDER (Puts lead), which the
        // exact-supersedes-covering-run read law keeps crash-safe: every
        // pre-flip intermediate state resolves each index to its old or
        // its new binding, never to absence (coverage-shrinking same-key
        // Puts are ordered AFTER the records that re-cover their tail).
        let desired: std::collections::BTreeMap<u32, &super::block_map::MapEntry> =
            entries.iter().map(|(b, e)| (*b, e)).collect();
        let mut groups: Vec<Vec<super::block_map::BlockMapOp>> = Vec::new();
        let mut preexisting = 0u64;
        let mut put_bytes = 0u64;
        // Claims mode: the displaced OLD records per staged transition —
        // the f36b recompute's input. The third element is the index's
        // DELTA into the displaced record (0 for point-class records; a
        // run's per-index arithmetic — design §12).
        let mut displaced: Vec<(u32, super::block_map::MapEntry, u32)> = Vec::new();
        // Claims mode: the ADOPTED bindings — the refs-take side of the
        // f36b recompute. Explicit (never derived from staged Puts): the
        // §12 claims×runs law also stages binding-PRESERVING Puts when a
        // release dissolves a run, and those must mint no reference.
        let mut ref_takes: Vec<(u32, super::block_map::MapEntry)> = Vec::new();
        let stage_put = |groups: &mut Vec<Vec<super::block_map::BlockMapOp>>,
                         put_bytes: &mut u64,
                         idx: u32,
                         entry: super::block_map::MapEntry| {
            *put_bytes += (super::block_map::BLOCK_MAP_KEY_LEN + entry.encoded_len()) as u64;
            groups.push(vec![super::block_map::BlockMapOp::Put {
                owner_ino: ino,
                block_index: idx,
                entry,
            }]);
        };
        if let Some(c) = claims {
            // The §12 claims×runs law: a claims-scoped train never
            // partial-adopts a run silently. Desired records are
            // per-index BY CONSTRUCTION (the routed seam skips run
            // emission on claims trains); a run here is a caller bug and
            // refuses rather than guessing a span-adoption semantics.
            if let Some((idx, _)) = entries.iter().find(|(_, e)| e.run_len() > 1) {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "claims-scoped map train for ino {ino} carries a RUN record at index \
                     {idx} — claims trains are per-index only (design §12); refusing \
                     rather than partial-adopting a span"
                )));
            }
            // PR 6c-i (design §14 S2 pre-fix a): the claims-scoped diff
            // needs the current records at CLAIMED indices only — one
            // exact lookup (+ the bounded A6 floor probe on a miss) per
            // claimed index, never a whole-map materialization: the
            // overlay saves publish through this arm per conveyor pass,
            // so an O(map) scan here would re-pay the very cost the
            // partial store deletes. `preexisting` counts the DISTINCT
            // records the probes observed (claims-bounded by
            // construction — the A1 whole-population census stays the
            // establishing train's). Finding 46: a WINDOW train probes
            // exact-only — with RAM whole-map authority behind it, a
            // miss is a fresh index or an index inside a run, and both
            // stage the same superseding point Put (the §2 read law), so
            // the floor scan's only yields (the arithmetic-equal skip and
            // the recompute's displaced ledger) buy nothing here.
            let mut current: std::collections::BTreeMap<u32, super::block_map::MapEntry> =
                std::collections::BTreeMap::new();
            for &idx in c.take.iter().chain(c.release.iter()) {
                let probe = if c.window {
                    self.block_map_exact(ino, idx).await
                } else {
                    self.get_block_mapping(ino, idx).await
                };
                if let Some((ridx, entry)) = probe
                    .map_err(|e| self.eio(&format!("block-map claim probe for ino {ino}: {e}")))?
                {
                    // Probes insert records at their TRUE keys, so the
                    // exact-vs-covering reads below stay the §2 read law:
                    // `current.get(&idx)` is Some only for a record keyed
                    // at the claimed index (the exact arm ran first
                    // inside the probe), and a covering run lands at its
                    // own start key.
                    if current.insert(ridx, entry).is_none() {
                        preexisting += 1;
                    }
                }
            }
            // The covering run for an index with no exact record (the §2
            // read law's resolve, over the scanned records). Backward
            // over the RUN_LEN_MAX-bounded window for the last record
            // that COVERS — never take-last plain: a superseding point
            // between the run and the index (the read law's own legal
            // shape) would otherwise hide the run.
            let covering_run =
                |current: &std::collections::BTreeMap<u32, super::block_map::MapEntry>,
                 idx: u32|
                 -> Option<(u32, super::block_map::MapEntry)> {
                    let lo = idx.saturating_sub(super::block_map::RUN_LEN_MAX - 1);
                    current
                        .range(lo..=idx)
                        .rev()
                        .find(|(s, e)| {
                            e.run_len() > 1
                                && u64::from(**s) + u64::from(e.run_len()) > u64::from(idx)
                        })
                        .map(|(s, e)| (*s, e.clone()))
                };
            // Per existing run intersected by a claim that cannot ride a
            // superseding point: the release set (absence has no point
            // form) and any changed take at the run's OWN key (a same-key
            // Put would replace the whole span). §12's conservative law:
            // the run DISSOLVES to per-index records in this train; the
            // next full local publish re-coalesces (the publish train is
            // the canonicalizer).
            #[derive(Default)]
            struct RunClaims {
                releases: std::collections::BTreeSet<u32>,
                adopted: std::collections::BTreeMap<u32, super::block_map::MapEntry>,
                start_adopt: Option<super::block_map::MapEntry>,
            }
            let mut run_claims: std::collections::BTreeMap<u32, RunClaims> =
                std::collections::BTreeMap::new();
            for &idx in &c.take {
                // A take with no shipped entry adopts nothing (a
                // contradictory frame; the durable record stands).
                let Some(want) = desired.get(&idx) else {
                    continue;
                };
                match current.get(&idx) {
                    Some(existing) if existing == *want => {}
                    Some(existing) if existing.run_len() > 1 => {
                        // A changed take AT a run's key: same-key Puts
                        // replace the record, so this is a split — the
                        // run dissolves.
                        run_claims.entry(idx).or_default().start_adopt = Some((*want).clone());
                    }
                    Some(existing) => {
                        displaced.push((idx, existing.clone(), 0));
                        ref_takes.push((idx, (*want).clone()));
                        stage_put(&mut groups, &mut put_bytes, idx, (*want).clone());
                    }
                    None => match covering_run(&current, idx) {
                        Some((start, run))
                            if entry_key(&run, idx - start).as_deref()
                                != entry_key(want, 0).as_deref()
                                || entry_key(want, 0).is_none() =>
                        {
                            // A changed take INSIDE a run rides the §2
                            // read law verbatim: one superseding Put,
                            // never a hot-path run split.
                            displaced.push((idx, run, idx - start));
                            ref_takes.push((idx, (*want).clone()));
                            stage_put(&mut groups, &mut put_bytes, idx, (*want).clone());
                        }
                        Some(_) => {} // arithmetic-equal: the run already binds it
                        None => {
                            ref_takes.push((idx, (*want).clone()));
                            stage_put(&mut groups, &mut put_bytes, idx, (*want).clone());
                        }
                    },
                }
            }
            for &idx in &c.release {
                // The f35 removal law: release-without-take, absent from
                // the shipped map — absence alone is the shipper's stale
                // view, never a removal intent.
                if c.take.contains(&idx) || desired.contains_key(&idx) {
                    continue;
                }
                match current.get(&idx) {
                    Some(old) if old.run_len() > 1 => {
                        // Release at a run's own key: the span splits.
                        run_claims.entry(idx).or_default().releases.insert(idx);
                    }
                    Some(old) => {
                        displaced.push((idx, old.clone(), 0));
                        groups.push(vec![super::block_map::BlockMapOp::Delete {
                            owner_ino: ino,
                            block_index: idx,
                        }]);
                    }
                    None => {
                        if let Some((start, _)) = covering_run(&current, idx) {
                            run_claims.entry(start).or_default().releases.insert(idx);
                        }
                    }
                }
            }
            // Fold the superseding adopt Puts already staged into the
            // dissolve bookkeeping: a dissolved run must not re-materialize
            // a survivor under an index the adoption just re-bound.
            for (idx, want) in &ref_takes {
                if let Some((start, _)) = covering_run(&current, *idx) {
                    if let Some(rc) = run_claims.get_mut(&start) {
                        rc.adopted.insert(*idx, want.clone());
                    }
                }
            }
            // Pre-fix (a)'s dissolve face: a dissolving run's survivors
            // must not clobber exact shadow points inside its span (the
            // §2 read law's `contains_key` screen below), and the bounded
            // probes above fetched CLAIMED keys only — page exactly the
            // dissolving spans (≤ RUN_LEN_MAX records each,
            // claims-bounded: every dissolving run was itself claimed).
            let dissolving: Vec<(u32, u32)> = run_claims
                .keys()
                .filter_map(|s| current.get(s).map(|e| (*s, e.run_len())))
                .collect();
            for (start, len) in dissolving {
                let span_end = u64::from(start) + u64::from(len);
                let mut cursor = start;
                loop {
                    let page = self
                        .block_map_range(ino, cursor, chunk)
                        .await
                        .map_err(|e| {
                            self.eio(&format!("block-map dissolve-span scan for ino {ino}: {e}"))
                        })?;
                    let Some(last) = page.last().map(|(i, _)| *i) else {
                        break;
                    };
                    let short = page.len() < chunk;
                    for (idx, entry) in page {
                        if u64::from(idx) < span_end {
                            current.entry(idx).or_insert(entry);
                        }
                    }
                    if short || u64::from(last) + 1 >= span_end {
                        break;
                    }
                    let Some(next) = last.checked_add(1) else {
                        break;
                    };
                    cursor = next;
                }
            }
            // The dissolves, one GROUP per run: survivor/adopted records
            // for every still-bound index (survivors verbatim as STRING
            // — router-true key strings, the always-decodable PR-2 form;
            // the next local publish upgrades and re-coalesces), the
            // run's OWN key staged LAST (the coverage-shrink order law).
            for (start, rc) in run_claims {
                let Some(run) = current.get(&start).cloned() else {
                    continue;
                };
                let len = run.run_len();
                let mut group: Vec<super::block_map::BlockMapOp> = Vec::new();
                let mut start_op: Option<super::block_map::BlockMapOp> = None;
                for delta in 0..len {
                    let idx = start + delta;
                    if rc.releases.contains(&idx) {
                        displaced.push((idx, run.clone(), delta));
                        if idx == start {
                            start_op = Some(super::block_map::BlockMapOp::Delete {
                                owner_ino: ino,
                                block_index: idx,
                            });
                        }
                        continue;
                    }
                    // An EXACT record at this index (a prior superseding
                    // point — the §2 shadow shape) already IS the truth
                    // there: the run never bound it, so the dissolve must
                    // not clobber it with the run's stale arithmetic.
                    if idx != start && current.contains_key(&idx) {
                        continue;
                    }
                    let entry = if idx == start {
                        match &rc.start_adopt {
                            Some(want) => {
                                displaced.push((idx, run.clone(), 0));
                                ref_takes.push((idx, want.clone()));
                                want.clone()
                            }
                            None => Self::run_survivor_entry(ino, &run, delta, entry_key)?,
                        }
                    } else if let Some(want) = rc.adopted.get(&idx) {
                        // The adoption Put is already staged as its own
                        // superseding group; the dissolve replaces the
                        // run record, so the point stands on its own.
                        want.clone()
                    } else {
                        Self::run_survivor_entry(ino, &run, delta, entry_key)?
                    };
                    let op = super::block_map::BlockMapOp::Put {
                        owner_ino: ino,
                        block_index: idx,
                        entry,
                    };
                    if idx == start {
                        put_bytes += (super::block_map::BLOCK_MAP_KEY_LEN
                            + match &op {
                                super::block_map::BlockMapOp::Put { entry, .. } => {
                                    entry.encoded_len()
                                }
                                _ => 0,
                            }) as u64;
                        start_op = Some(op);
                    } else if !rc.adopted.contains_key(&idx) {
                        put_bytes += (super::block_map::BLOCK_MAP_KEY_LEN
                            + match &op {
                                super::block_map::BlockMapOp::Put { entry, .. } => {
                                    entry.encoded_len()
                                }
                                _ => 0,
                            }) as u64;
                        group.push(op);
                    }
                }
                if let Some(op) = start_op {
                    group.push(op);
                }
                groups.push(group);
            }
        } else {
            // Desired runs, for the covered-point coalesce (sorted by
            // construction: `entries` arrive index-ascending).
            let desired_runs: Vec<(u32, u32)> = entries
                .iter()
                .filter(|(_, e)| e.run_len() > 1)
                .map(|(b, e)| (*b, e.run_len()))
                .collect();
            let covered_by_desired_run = |idx: u32| -> Option<u32> {
                let i = desired_runs.partition_point(|&(s, _)| s <= idx);
                let (s, l) = *desired_runs.get(i.checked_sub(1)?)?;
                (u64::from(s) + u64::from(l) > u64::from(idx)).then_some(s)
            };
            let mut matched: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
            // Existing point-class records covered by a desired run: their
            // Deletes ride the covering run-Put's GROUP (the A6 law).
            let mut run_deletes: std::collections::BTreeMap<
                u32,
                Vec<super::block_map::BlockMapOp>,
            > = std::collections::BTreeMap::new();
            // Same-key replacements whose coverage SHRINKS: ordered after
            // every other Put (their displaced tail is re-covered first).
            let mut shrink_keys: std::collections::BTreeSet<u32> =
                std::collections::BTreeSet::new();
            let mut stale_deletes: Vec<super::block_map::BlockMapOp> = Vec::new();
            let mut cursor = 0u32;
            'scan: loop {
                let page = self
                    .block_map_range(ino, cursor, chunk)
                    .await
                    .map_err(|e| self.eio(&format!("block-map diff scan for ino {ino}: {e}")))?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                let next = last.checked_add(1);
                for (idx, existing) in page {
                    // PR 6b: the diff never reaches past a live sweep
                    // cursor — records at/above it are residue the
                    // background sweep owns (deleting them by-absence
                    // here would strand their references, which the diff
                    // has no frame to release).
                    if flip_cursor.is_some_and(|k| idx >= k) {
                        break 'scan;
                    }
                    preexisting += 1;
                    match desired.get(&idx) {
                        Some(want) if existing == **want => {
                            matched.insert(idx);
                        }
                        // Changed binding: the desired-walk below stages the
                        // superseding Put (same key — no Delete). A
                        // replacement that covers LESS than the record it
                        // supersedes is deferred behind the Puts that
                        // re-cover its tail.
                        Some(want) => {
                            if u64::from(idx) + u64::from(want.run_len())
                                < u64::from(idx) + u64::from(existing.run_len())
                            {
                                shrink_keys.insert(idx);
                            }
                        }
                        None => {
                            let del = super::block_map::BlockMapOp::Delete {
                                owner_ino: ino,
                                block_index: idx,
                            };
                            match covered_by_desired_run(idx) {
                                Some(run_start) if existing.run_len() == 1 => {
                                    run_deletes.entry(run_start).or_default().push(del);
                                }
                                // Stale (or a whole superseded run):
                                // deleted AFTER every Put — desired
                                // coverage lands first.
                                _ => stale_deletes.push(del),
                            }
                        }
                    }
                }
                let Some(next) = next else { break };
                cursor = next;
            }
            let mut shrink_groups: Vec<Vec<super::block_map::BlockMapOp>> = Vec::new();
            for (idx, entry) in entries {
                if matched.contains(idx) {
                    continue;
                }
                put_bytes += (super::block_map::BLOCK_MAP_KEY_LEN + entry.encoded_len()) as u64;
                let mut group = vec![super::block_map::BlockMapOp::Put {
                    owner_ino: ino,
                    block_index: *idx,
                    entry: entry.clone(),
                }];
                if entry.run_len() > 1 {
                    group.append(&mut run_deletes.remove(idx).unwrap_or_default());
                }
                if shrink_keys.contains(idx) {
                    shrink_groups.push(group);
                } else {
                    groups.push(group);
                }
            }
            // Covered points under a run that MATCHED verbatim: coverage
            // is already durable, the shadow deletes stand alone.
            for (_, dels) in run_deletes {
                if !dels.is_empty() {
                    groups.push(dels);
                }
            }
            groups.append(&mut shrink_groups);
            if !stale_deletes.is_empty() {
                groups.push(stale_deletes);
            }
        }
        let records: u64 = groups.iter().map(|g| g.len() as u64).sum();

        // The f36b recompute (claims mode, resolver armed): the staged
        // accounting is the tree→composed swap diff — the shipper's frame
        // legitimately lags and may MIS-NAME the displaced binding, so
        // its non-map-blob ops are REPLACED (the rung-19 law on the
        // scoped-Put arm, verbatim); its map-blob ops travel beside. The
        // released set travels up for the post-commit free ladder.
        // Unarmed (no resolver): the caller's frame stands byte-identical
        // (the f36 preservation arm) and the caller keeps its own
        // displaced-free stream.
        let mut recomputed = false;
        let mut released: Vec<super::block_refs::BlockRef> = Vec::new();
        let mut released_keys: Vec<String> = Vec::new();
        // PR 6c-i: the recompute arms — the rung-19 GLOBAL resolver
        // (served/mw trains) or, for a LOCAL claims train, the caller's
        // own `ref_for` (the routing `block_ref_for` closure every mount
        // installs): a solo overlay save's displacement discovery lives
        // WHOLLY here (§14 S2 option ii — the merge stopped capturing
        // tree prev-bindings), so it cannot depend on the mw arm.
        // Finding 46: a WINDOW train never recomputes — its caller's RAM
        // merge captured every displacement (whole-map authority), so the
        // frame is exact and a recompute would only double-count the
        // displaced frees the caller's own stream already owns.
        let resolver_global = super::block_refs::block_ref_resolver();
        let recompute_armed =
            claims.is_some_and(|c| !c.window && (resolver_global.is_some() || c.overlay));
        let staged_refs: Vec<super::block_refs::BlockRefOp> = if recompute_armed {
            recomputed = true;
            let mut out: Vec<super::block_refs::BlockRefOp> = Vec::new();
            let mut resolve =
                |entry: &super::block_map::MapEntry, idx: u32, delta: u32, take: bool| {
                    let Some(key) = entry_key(entry, delta) else {
                        super::META_KV_BLOCK_REFS_UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    let resolved = match &resolver_global {
                        Some(resolver) => resolver(&key, refs_owner, idx),
                        None => ref_for(&key, idx),
                    };
                    match resolved {
                        Some(r) => {
                            if take {
                                out.push(super::block_refs::BlockRefOp::taken(r));
                            } else {
                                out.push(super::block_refs::BlockRefOp::released(r));
                                released.push(r);
                                released_keys.push(key);
                            }
                        }
                        None => {
                            super::META_KV_BLOCK_REFS_UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                };
            for (idx, old, delta) in &displaced {
                resolve(old, *idx, *delta, false);
            }
            // Takes are the EXPLICIT adoption ledger — never the
            // staged Puts, which since §12 also carry
            // binding-preserving dissolve survivors that must
            // mint no reference.
            for (idx, entry) in &ref_takes {
                resolve(entry, *idx, 0, true);
            }
            out.extend(
                block_refs
                    .iter()
                    .filter(|o| o.reference.is_map_blob())
                    .copied(),
            );
            out
        } else {
            block_refs.to_vec()
        };
        // Claims mode carries the WHOLE frame (finding 36b's owner half),
        // so the journal-entry-cap chunking (f38's law) runs HERE, in
        // refs-only transactions co-owning the held 4a; the tail rides
        // the flip. Whole-map callers pre-chunk (their loops are the f38
        // sites). A crash between chunk and flip leaves only report-only
        // fsck C8 residue (space-safe, data-safe).
        const TRAIN_REF_TX_CHUNK: usize = 512;
        let mut refs_tail = staged_refs;
        if claims.is_some() && self.block_refs_engaged() {
            while refs_tail.len() > TRAIN_REF_TX_CHUNK {
                let rest = refs_tail.split_off(TRAIN_REF_TX_CHUNK);
                let chunk_ops = std::mem::replace(&mut refs_tail, rest);
                let mut tx = KvTx::new();
                tx.stage_block_refs(&chunk_ops);
                tx.hold_guards(Arc::clone(&guards));
                self.commit_tx(tx).await?;
            }
        }

        // Steps 4–5: chunked commits, tail rides the flip. Groups pack
        // whole into a tx where they fit (the A6 one-tx coalesce law); a
        // group past the chunk splits in its own order, which the read
        // law keeps crash-safe (Puts lead their Deletes).
        let mut tail: Vec<super::block_map::BlockMapOp> = Vec::new();
        for group in groups {
            if !tail.is_empty() && tail.len() + group.len() > chunk {
                let chunk_ops = std::mem::take(&mut tail);
                let mut tx = KvTx::new();
                tx.stage_block_map(&chunk_ops)?;
                tx.hold_guards(Arc::clone(&guards));
                self.commit_tx(tx).await?;
            }
            tail.extend(group);
            while tail.len() > chunk {
                let rest = tail.split_off(chunk);
                let chunk_ops = std::mem::replace(&mut tail, rest);
                let mut tx = KvTx::new();
                tx.stage_block_map(&chunk_ops)?;
                tx.hold_guards(Arc::clone(&guards));
                self.commit_tx(tx).await?;
            }
        }
        self.set_layout_and_size_with_map_holding(ino, layout, size, &refs_tail, &tail, guards)
            .await?;
        Ok(Some(MapMigrateOutcome {
            records,
            record_bytes: put_bytes,
            preexisting,
            gen: committed_gen,
            recomputed,
            released,
            released_keys,
            sweep_cursor: flip_cursor,
            swept_records,
            swept_freed,
        }))
    }

    /// **The bounded synchronous record sweep** (PR 2, Rev 1.1 #4's
    /// unlink half): delete every tree-7 record of `ino`, chunked one tx
    /// per `chunk` Deletes under one held 4a guard — never silent
    /// residue. The A2 job-fabric sweep (size-flip-first + durable
    /// cursor) supersedes this for truncate at scale in a later PR;
    /// unlink teardown is bounded by the PRESENT record population.
    /// A no-op — not an error — on a volume with no engaged tree.
    pub async fn sweep_block_map(&self, ino: Ino, chunk: usize) -> Result<u64> {
        if !self.block_map_tree_engaged() {
            return Ok(0);
        }
        self.write_gate()?;
        let chunk = chunk.max(1);
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let mut deleted = 0u64;
        loop {
            // Always from index 0: committed Deletes vanish from the
            // fold, so the window shrinks to empty.
            let page = self
                .block_map_range(ino, 0, chunk)
                .await
                .map_err(|e| self.eio(&format!("block-map sweep scan for ino {ino}: {e}")))?;
            if page.is_empty() {
                break;
            }
            let ops: Vec<super::block_map::BlockMapOp> = page
                .into_iter()
                .map(|(idx, _)| super::block_map::BlockMapOp::Delete {
                    owner_ino: ino,
                    block_index: idx,
                })
                .collect();
            deleted += ops.len() as u64;
            let mut tx = KvTx::new();
            tx.stage_block_map(&ops)?;
            tx.hold_guards(Arc::clone(&guards));
            self.commit_tx(tx).await?;
        }
        Ok(deleted)
    }

    /// PR 6b (design §3/A2): the **size-flip-first truncate/unlink
    /// handoff** — commit ONLY the new size plus the durable per-ino
    /// sweep cursor in the head sentinel (`kvmap:1;sweep:K`), one
    /// two-record tx under one 4a guard, O(1) regardless of the removed
    /// set (a 1 PiB truncate is 2²⁸ record Deletes — five orders past the
    /// whole-entry cap — so the DURABLE record/ref/free work defers to
    /// the job-fabric sweep; reads clamp to size, so every shadowed
    /// record is immediately unreadable). The cursor law: a handoff onto
    /// an already-cursored head takes `min(existing, k)` — the swept
    /// region only ever grows downward, so "every record ≥ cursor is
    /// residue" survives repeated truncates. Everything else in the head
    /// (gen included — this is not a train, it stages no records) is
    /// preserved verbatim. Returns the committed head id for the
    /// caller's RAM republish.
    pub async fn kvmap_truncate_handoff(
        &self,
        ino: Ino,
        new_size: u64,
        k: u32,
        block_refs: &[super::block_refs::BlockRefOp],
    ) -> Result<String> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let bytes = self.getxattr(ino, "layout").await?.ok_or_else(|| {
            Self::not_found(format!("kvmap truncate handoff: ino {ino} has no layout"))
        })?;
        let mut layout: crate::layout_wire::LayoutMetadata =
            bincode::deserialize(&bytes).map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "kvmap truncate handoff for ino {ino}: undecodable head ({e}) — \
                     the handoff composes onto a durable kvmap head only"
                ))
            })?;
        let head = layout
            .block_map_id
            .as_deref()
            .and_then(|id| super::block_map::parse_kvmap_head(id).ok())
            .ok_or_else(|| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "kvmap truncate handoff for ino {ino}: the durable head is not \
                     kvmap-class — the synchronous path owns this shape"
                ))
            })?;
        let cursor = head.sweep_cursor.map_or(k, |c| c.min(k));
        layout.block_map_id = Some(
            super::block_map::KvmapHead {
                sweep_cursor: Some(cursor),
                gen: head.gen,
            }
            .encode(),
        );
        layout.size = new_size;
        let head_id = layout
            .block_map_id
            .clone()
            .expect("just assigned the head id");
        let encoded = bincode::serialize(&layout).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "kvmap truncate handoff for ino {ino}: head re-encode failed: {e}"
            ))
        })?;
        self.set_layout_and_size_with_map_holding(ino, &encoded, new_size, block_refs, &[], guards)
            .await?;
        Ok(head_id)
    }

    /// PR 6b: one A2 sweep chunk under one held 4a — the per-chunk
    /// ONE-tx law (design §12): record-true map Deletes + their
    /// BlockRefOp releases + the cursor-advance head Put ride ONE KvTx;
    /// the freed block keys are RETURNED for the caller's post-commit
    /// reclaim enqueue (RES-1 — device commands never issue under the
    /// guard, which drops when this returns).
    ///
    /// The cursor law: the deletable floor is re-derived from the
    /// CURRENT durable size under the held 4a (`floor_of(size)`), so a
    /// write/extend that grew the file back past the cursor — whose
    /// publish barrier already swept and advanced past the re-exposed
    /// span — never has its minted records deleted: the chunk starts at
    /// `max(cursor, floor)`.
    ///
    /// The straddler law (the run face of the 6a dissolve law): a RUN
    /// record keyed below the start whose span crosses it is re-Put
    /// SHORTENED at its own key (len 1 collapses to the point form —
    /// same vol_tag/offset/stamp arithmetic, so the record stays
    /// record-true) and the covered tail above the start releases with
    /// the chunk; a record fully at/above the start deletes whole.
    ///
    /// The TERMINAL chunk (scan exhausted) clears the cursor from the
    /// head in the same tx; the caller owns the corpse destroy
    /// (`nlink == 0`).
    #[allow(clippy::type_complexity)]
    pub async fn kvmap_sweep_chunk(
        &self,
        ino: Ino,
        chunk: usize,
        floor_of: &(dyn Fn(u64) -> u32 + Send + Sync),
        entry_key: &(dyn Fn(&super::block_map::MapEntry, u32) -> Option<String> + Send + Sync),
        ref_for: &(dyn Fn(&str, u32) -> Option<super::block_refs::BlockRef> + Send + Sync),
    ) -> Result<SweepChunkOutcome> {
        if !self.block_map_tree_engaged() {
            return Ok(SweepChunkOutcome::NoCursor);
        }
        self.write_gate()?;
        let chunk = chunk.max(1);
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        // The durable head, re-read under the held 4a — the plan IS the
        // cursor (KD-6); a vanished record / cleared cursor means the
        // work is done (a publish absorbed it, or a duplicate job ran).
        let Some(bytes) = self.getxattr(ino, "layout").await? else {
            return Ok(SweepChunkOutcome::NoCursor);
        };
        let Ok(mut layout) = bincode::deserialize::<crate::layout_wire::LayoutMetadata>(&bytes)
        else {
            return Ok(SweepChunkOutcome::NoCursor); // JSON/legacy: never kvmap
        };
        let Some(head) = layout
            .block_map_id
            .as_deref()
            .and_then(|id| super::block_map::parse_kvmap_head(id).ok())
        else {
            return Ok(SweepChunkOutcome::NoCursor);
        };
        let Some(cursor) = head.sweep_cursor else {
            return Ok(SweepChunkOutcome::NoCursor);
        };
        let start = cursor.max(floor_of(layout.size));
        let page = self
            .kvmap_sweep_page(ino, start, None, chunk, entry_key, ref_for)
            .await?;
        let new_cursor = page.resume;
        layout.block_map_id = Some(
            super::block_map::KvmapHead {
                sweep_cursor: new_cursor,
                gen: head.gen,
            }
            .encode(),
        );
        let encoded = bincode::serialize(&layout).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "kvmap sweep chunk for ino {ino}: head re-encode failed: {e}"
            ))
        })?;
        self.set_layout_and_size_with_map_holding(
            ino,
            &encoded,
            layout.size,
            &page.rel,
            &page.ops,
            guards,
        )
        .await?;
        Ok(match new_cursor {
            Some(_) => SweepChunkOutcome::Progress {
                records: page.records,
                freed: page.freed,
            },
            None => SweepChunkOutcome::Terminal {
                records: page.records,
                freed: page.freed,
            },
        })
    }

    /// The shared A2 sweep-page builder (the chunk method and the train's
    /// extend barrier): stage record-true Deletes for `ino`'s records in
    /// `[start, upto)` (unbounded when `upto` is `None`), one bounded
    /// page's worth (`chunk` counts COVERED INDICES, so a run's per-index
    /// releases can never blow the f38 journal-entry cap), plus the
    /// straddler dissolve at the left boundary. Never-lossy: an
    /// unresolvable entry (foreign/retired volume tag) REFUSES the chunk
    /// loud — deleting a record whose reference cannot release, or whose
    /// block cannot free, would strand the block forever (fsck C2/C8).
    async fn kvmap_sweep_page(
        &self,
        ino: Ino,
        start: u32,
        upto: Option<u32>,
        chunk: usize,
        entry_key: &(dyn Fn(&super::block_map::MapEntry, u32) -> Option<String> + Send + Sync),
        ref_for: &(dyn Fn(&str, u32) -> Option<super::block_refs::BlockRef> + Send + Sync),
    ) -> Result<SweepPageOps> {
        let refs_engaged = self.block_refs_engaged();
        let mut out = SweepPageOps::default();
        let mut budget = chunk;
        // Release the deltas `[from, to)` of one record — every release
        // pairs a freed key, so the chunk budget bounds BOTH the tx's
        // journal footprint (the whole-entry cap: a 4096-index run's
        // one-shot release measured ~178 KiB against the 128 KiB cap)
        // and the caller's post-commit reclaim batch.
        let resolve_span = |out: &mut SweepPageOps,
                            entry: &super::block_map::MapEntry,
                            idx: u32,
                            from: u32,
                            to: u32|
         -> Result<()> {
            for delta in from..to {
                let Some(key) = entry_key(entry, delta) else {
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "kvmap sweep for ino {ino}: cannot resolve index {} of record \
                         {entry:?} — no mounted volume resolves its arithmetic; refusing \
                         rather than stranding its block (never-lossy)",
                        idx + delta
                    )));
                };
                if refs_engaged {
                    let Some(r) = ref_for(&key, idx + delta) else {
                        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                            "kvmap sweep for ino {ino}: no durable-reference resolution \
                             for '{key}' at index {} — refusing rather than leaking the \
                             record's reference (fsck C8's class)",
                            idx + delta
                        )));
                    };
                    out.rel.push(super::block_refs::BlockRefOp::released(r));
                }
                out.freed.push(key);
            }
            Ok(())
        };
        // Consume one record's removable span `[keep, run_len)` from the
        // RIGHT under the budget — the run face of the §12a dissolve law:
        // each tx re-Puts the run SHORTENED by exactly the span whose
        // references it releases, so every committed intermediate state
        // is coverage-exact (never released-but-covered, never
        // covered-but-released). `true` = the record fully consumed
        // (deleted, or shortened to its live `keep` head); `false` =
        // budget ran out mid-record — the chunk stops and resumes at a
        // cursor whose scan re-finds this record's remainder.
        let consume = |out: &mut SweepPageOps,
                       budget: &mut usize,
                       idx: u32,
                       entry: &super::block_map::MapEntry,
                       keep: u32|
         -> Result<bool> {
            let len = entry.run_len();
            let span = len - keep;
            let take = (span as usize).min((*budget).max(1)) as u32;
            resolve_span(out, entry, idx, len - take, len)?;
            *budget = budget.saturating_sub(take as usize);
            out.records += 1;
            if take == span && keep == 0 {
                out.ops.push(super::block_map::BlockMapOp::Delete {
                    owner_ino: ino,
                    block_index: idx,
                });
            } else {
                out.ops.push(super::block_map::BlockMapOp::Put {
                    owner_ino: ino,
                    block_index: idx,
                    entry: shortened_run_entry(entry, len - take),
                });
            }
            Ok(take == span)
        };
        // The left-boundary straddler: a run keyed below `start` covering
        // it (the floor probe answers COVERING records only). Its keep
        // head `[s, start)` is LIVE; the tail consumes right-to-left
        // across as many chunk txs as the budget dictates — resuming at
        // `start`, whose floor probe re-finds the shorter run until the
        // tail is gone.
        if start > 0 {
            if let Some((s, entry)) = self
                .block_map_floor(ino, start)
                .await
                .map_err(|e| self.eio(&format!("kvmap sweep floor probe for ino {ino}: {e}")))?
            {
                if !consume(&mut out, &mut budget, s, &entry, start - s)? {
                    out.resume = Some(start);
                    return Ok(out);
                }
            }
        }
        let page = self
            .block_map_range(ino, start, chunk)
            .await
            .map_err(|e| self.eio(&format!("kvmap sweep scan for ino {ino}: {e}")))?;
        let scan_exhausted = page.len() < chunk;
        let mut cut_short = false;
        let mut bound_reached = false;
        for (idx, entry) in page {
            if upto.is_some_and(|u| idx >= u) {
                // Bounded (barrier) mode: records at/above the bound stay
                // the background sweep's.
                bound_reached = true;
                break;
            }
            if budget == 0 {
                // Budget spent: the first UNPROCESSED record's own key is
                // the resume cursor (never a processed record's
                // `idx + run_len` — a superseding point INSIDE a deleted
                // run's span is its own record and must not be skipped).
                out.resume = Some(idx);
                cut_short = true;
                break;
            }
            if !consume(&mut out, &mut budget, idx, &entry, 0)? {
                // Budget ran out mid-run: the record survives shortened —
                // resume AT its key (it still carries the remainder).
                out.resume = Some(idx);
                cut_short = true;
                break;
            }
        }
        if !cut_short && !bound_reached && !scan_exhausted {
            // A full page, fully processed (and the bound — if any — not
            // reached): more records may follow the last processed key.
            // A bound that WAS reached left `resume` `None`, which is the
            // exhaustion verdict.
            out.resume = out
                .ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    super::block_map::BlockMapOp::Delete { block_index, .. } => {
                        block_index.checked_add(1)
                    }
                    super::block_map::BlockMapOp::Put { .. } => None,
                })
                .or(Some(start));
        }
        Ok(out)
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
        kind: u8,
        start_key: &[u8],
        end_key: &[u8],
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        // A chain is ≤ 256 records by construction.
        self.range_kind(kind, start_key, end_key, 256).await
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
        for (_k, v) in self.chain_scan(TREE_DENTRIES, &start, &end).await? {
            let d = DentryValue::decode(&v)?;
            if d.name == name.as_bytes() {
                return Ok(Some(d));
            }
        }
        Ok(None)
    }

    async fn read_inode_value(&self, ino: Ino) -> std::result::Result<Option<InodeValue>, KvError> {
        match self.lookup_kind(TREE_INODES, &inode_key(ino)).await? {
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
            let page = self
                .range_kind(TREE_DENTRIES, &cursor, &end, SCAN_PAGE)
                .await?;
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
            let page = self.range_kind(TREE_DENTRIES, &cursor, &end, want).await?;
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
        for (_k, v) in self.chain_scan(TREE_XATTRS, &start, &end).await? {
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
            let page = self
                .range_kind(TREE_XATTRS, &cursor, &end, SCAN_PAGE)
                .await?;
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
        let mut region0_tail: Option<u64> = None;
        for (tail, _) in drained {
            self.alloc.advance_durable(tail);
            self.ring.advance_reusable_upto(tail);
            region0_tail = Some(tail);
        }
        match self.appenders.as_ref() {
            None => {
                if let Some(tail) = region0_tail {
                    self.cache.set_durable_tail(tail);
                }
            }
            Some(set) => {
                // Per region: the same epoch split on ITS pending list,
                // advancing ITS ring; the cache's elision tail is the MIN
                // over the regions (a tombstone is elidable only below its
                // own ring's tail, and the min is safe for every ring).
                if let Some(tail) = region0_tail {
                    if let Some(r0) = set.regions.first() {
                        r0.durable_tail.fetch_max(tail, Ordering::AcqRel);
                    }
                }
                for r in set.regions.iter().skip(1) {
                    let drained: Vec<(u64, u64)> = {
                        let mut g = r.pending_reclaim.lock().unwrap_or_else(|e| e.into_inner());
                        let split = g.partition_point(|&(_, epoch)| epoch < covered);
                        g.drain(..split).collect()
                    };
                    for (tail, _) in drained {
                        r.ring().advance_reusable_upto(tail);
                        r.durable_tail.fetch_max(tail, Ordering::AcqRel);
                        // The region's own §4.7 coverage gate: frees its
                        // ring's tail covers become RETURNABLE — the
                        // `ReturnExtents` batch the next cycle ships.
                        r.grant().advance_durable(tail);
                    }
                }
                // Elision tails are PER RING (positions are not comparable
                // across rings): the cache-wide word is ring 0's, as on a
                // flat volume; each leased slot's is its lessee's.
                if let Some(tail) = region0_tail {
                    self.cache.set_durable_tail(tail);
                }
                Self::publish_slot_durable_tails(set, &self.cache);
            }
        }
    }

    /// Publish every declared region's durable tail as the elision tail of
    /// the slots it leases (`NodeCache::set_slot_durable_tail` — monotone
    /// per slot). A no-op on an unpartitioned set.
    fn publish_slot_durable_tails(set: &super::appender::AppenderSet, cache: &NodeCache) {
        for r in set.regions.iter().skip(1) {
            let tail = r.durable_tail.load(Ordering::Acquire);
            for slot in r.leases().iter() {
                cache.set_slot_durable_tail(*slot, tail);
            }
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

    /// [`Self::checkpoint_past`] for an appender region's ring: cycle until
    /// THAT region's page tail reaches `pos` (positions are per ring).
    /// Region 0's is the fixed ledger's tail — [`Self::checkpoint_past`].
    pub async fn checkpoint_past_region(
        &self,
        region: u32,
        pos: u64,
    ) -> std::result::Result<(), KvError> {
        let Some(r) = self
            .appenders
            .as_ref()
            .and_then(|a| a.region(region))
            .filter(|r| r.id != 0)
            .cloned()
        else {
            return self.checkpoint_past(pos).await;
        };
        let mut smo = self.smo.lock().await;
        for _ in 0..8 {
            self.checkpoint_cycle(&mut smo, true).await?;
            if r.last_tail.load(Ordering::Acquire) >= pos {
                return Ok(());
            }
        }
        Err(KvError::Corrupt(format!(
            "appender {region}'s checkpoint tail failed to clear the journal hole ending at \
             {pos} after 8 cycles (tail stuck at {}) — replay would walk into the hole",
            r.last_tail.load(Ordering::Acquire)
        )))
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
        // A mount with NO local metadata authority over this volume
        // (reader / co-writer / peer-owned) writes nothing at teardown
        // either. Without this arm the `else` branch below — taken exactly
        // when no checkpoint task exists, which is precisely these three
        // postures — would run `checkpoint_now()` and WRITE to a volume
        // whose whole contract is that it does not: the S5 reader's
        // *"this mount cannot and will not write"*, S9's co-writer, and
        // per-volume claim admission's own law that a `Peer`-mode
        // backend's shutdown is a NO-OP because it took nothing (§5.4's
        // rollback ladder). §4.11's unknown-ro degradation keeps its
        // shipped path: it is a WRITE mount holding Layer A, and its
        // replay residue is its own to make durable.
        if matches!(
            self.ro_cause,
            ReadOnlyCause::ReaderMount
                | ReadOnlyCause::CoWriterMount
                | ReadOnlyCause::PeerOwnedVolume
        ) {
            self.shutting_down.store(true, Ordering::Release);
            self.ring.wake_parked();
            self.ckpt_wake.notify_one();
            return Ok(());
        }
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
        // The shutdown signal is a PERMIT (`notify_one`), never an epoch
        // (`notify_waiters`): the checkpoint task reads `shutting_down`
        // only after its park wakes, and it is often NOT parked when this
        // runs — it is inside a §4.6 pt 1 maintenance pass (the reopen's
        // post-replay fold on a forest volume is one big mixed-leaf split
        // still running at the test's `shutdown`). `notify_waiters` wakes
        // the waiters registered at that instant and stores nothing, so a
        // busy task came back to a fresh `notified()` and slept to its
        // cadence deadline before it saw the flag — one full flush
        // interval per unmount that raced a pass (review round 4, Issue
        // 26; `SQUEEZEFS_META_FLUSH_INTERVAL_MS=60000` made it a 60 s
        // stall). A permit is consumed by the task's NEXT `notified()`,
        // whenever that is.
        self.ckpt_wake.notify_one();
        // The trace edge a harness orders on: past this line the flag is
        // stored and the permit sent — whatever the checkpoint task is
        // doing now, its next park is not where it learns about the
        // shutdown.
        self.trace_guard_event("shutdown_signalled");
        // PR M6: the drain task observes the flag on its wake and exits;
        // joining it keeps the no-leaked-tasks teardown contract. The same
        // permit law as the checkpoint task's signal above: the drain task
        // may be inside `drain_pending_times_now` when this runs, and an
        // epoch signal it did not witness would leave it asleep to its own
        // tick with this join waiting on it.
        self.times_drain_wake.notify_one();
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
        // The appender region release (design-symmetric-metadata §5.1.3,
        // PR 2): every page of ours goes Free after the final checkpoint
        // named every root in tree 0 (a no-op on a flat volume).
        if let Err(e) = self.leave_appender_regions().await {
            log::warn!(
                "meta volume {}: clean unmount could not release its appender pages: {e} (the \
                 pages stay Live — the next mount of this identity recovers its own residue)",
                self.path.display()
            );
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

    /// This volume's §4.7 extent allocator (the `journal_ring()` precedent:
    /// the space-standstill contracts drain and refill the heap through it
    /// as a foreign claimant).
    pub fn allocator(&self) -> &Arc<ExtentAllocator> {
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
        let trees = self.all_trees();
        self.node_cache().for_each_node(|n| {
            if n.level() != 0 || n.state().is_superseded() {
                return;
            }
            let snap = n.snapshot();
            let (total, distinct) = snap.indexed_record_census();
            out.leaves += 1;
            out.records_indexed += total;
            out.records_live += distinct;
            if total > distinct {
                out.candidates.push((n.tree_id(), n.addr()));
            }
            // §4.6a (e): the underfull face — the sweep's own predicate
            // (`KvTree::is_merge_candidate`, one source of truth).
            if trees
                .iter()
                .find(|t| t.tree_id() == n.tree_id())
                .is_some_and(|t| t.is_merge_candidate(n))
            {
                out.merge_candidates.push((n.tree_id(), n.addr()));
            }
        });
        self.merge_candidates_tail
            .store(self.cache.durable_tail(), Ordering::Relaxed);
        self.merge_candidates
            .store(out.merge_candidates.len() as u64, Ordering::Relaxed);
        out
    }

    /// ONE bounded, cursor-resumed sweep call over the volume's trees
    /// (§4.6a (e) finalized) — the heap-full sweep's and the D4 arm's
    /// shared body, under the caller's SMO mutex. Trees are walked in
    /// order under one `deadline`; a tree whose lap completed is marked
    /// done for the VOLUME lap and skipped until every tree is done, so a
    /// tree the budget keeps cutting is never starved by the ones before
    /// it re-walking. When the last tree completes: the exact candidate
    /// count over the volume publishes (`meta_kv_merge_candidates`),
    /// `merge_laps` increments, the lap resets. `merge_sweeps` counts
    /// calls; the `meta_kv_merge_sweep_{ns,projections}` pair accumulates
    /// every call.
    pub(super) async fn run_merge_sweep(
        &self,
        smo: &mut SmoContext,
        forced_retirement: bool,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<VolumeMergeSweep, KvError> {
        let trees = self.all_trees();
        let mut report = VolumeMergeSweep::default();
        let mut lap = self.merge_lap.lock().expect("merge lap").clone();
        lap.done.resize(trees.len(), false);
        let mut all_done = true;
        for (i, tree) in trees.iter().enumerate() {
            if lap.done[i] {
                continue;
            }
            let sweep = match tree.merge_underfull(smo, forced_retirement, deadline).await {
                Ok(s) => s,
                Err(e) => {
                    // A loud class (corruption / I/O): keep the lap state
                    // as it stands and surface it.
                    *self.merge_lap.lock().expect("merge lap") = lap;
                    return Err(e);
                }
            };
            self.merge_sweep_ns
                .fetch_add(sweep.elapsed.as_nanos() as u64, Ordering::Relaxed);
            self.merge_sweep_projections
                .fetch_add(sweep.projections, Ordering::Relaxed);
            report.merges += sweep.outcome.merges;
            report.interior_merges += sweep.outcome.interior_merges;
            report.root_collapses += sweep.outcome.root_collapses;
            report.space_refused |= sweep.space_refused;
            if sweep.lap_complete {
                lap.done[i] = true;
                lap.candidates += sweep.candidates;
            } else {
                // The budget is spent or a merge was refused: the next
                // call resumes this tree from its parked cursor.
                report.refusal = sweep.refusal;
                all_done = false;
                break;
            }
        }
        if all_done && lap.done.iter().all(|d| *d) {
            report.lap_complete = true;
            report.candidates = lap.candidates;
            // The tail the count was taken under: a later tail advance
            // elides more tombstones and can only ADD candidates, so the
            // gauge is exact for THIS tail and a lower bound until the
            // next lap (`merge_candidates_audit` pins the law).
            self.merge_candidates_tail
                .store(self.cache.durable_tail(), Ordering::Relaxed);
            self.merge_candidates
                .store(lap.candidates, Ordering::Relaxed);
            self.merge_laps.fetch_add(1, Ordering::Relaxed);
            lap = VolumeLap::default();
        }
        *self.merge_lap.lock().expect("merge lap") = lap;
        self.merge_sweeps.fetch_add(1, Ordering::Relaxed);
        Ok(report)
    }

    /// The exact-candidates law, auditable: under the SMO mutex (so no
    /// sweep, compaction or collapse interleaves), the published gauge
    /// with the durable tail its lap (or census) counted under, a fresh
    /// census UNDER THAT SAME TAIL (`KvTree::merge_candidate_census_at` —
    /// the sweep's own predicate), and a census under the tail in force
    /// now. The law: `gauge == census_at_gauge_tail` whenever no user
    /// commit changed the tree since the publish; and `census_now ≥
    /// census_at_gauge_tail` always — a tail advance can only elide more
    /// tombstones and ADD candidates, which the next lap publishes.
    pub async fn merge_candidates_audit(&self) -> MergeCandidatesAudit {
        let _smo = self.smo.lock().await;
        let gauge_tail = self.merge_candidates_tail.load(Ordering::Relaxed);
        let census_tail = self.cache.durable_tail();
        let trees = self.all_trees();
        MergeCandidatesAudit {
            gauge: self.merge_candidates.load(Ordering::Relaxed),
            gauge_tail,
            census_at_gauge_tail: trees
                .iter()
                .map(|t| t.merge_candidate_census_at(gauge_tail))
                .sum(),
            census_now: trees.iter().map(|t| t.merge_candidate_census()).sum(),
            census_tail,
        }
    }

    /// §4.6a (e), the D4 arm's MERGE half: one bounded sweep call under
    /// the FIFO-valve posture, serialized with the checkpoint task through
    /// the per-volume SMO mutex — the `defrag_compact_nodes` discipline
    /// verbatim for its refusals: journal-reserve / pending-free refusals
    /// run a checkpoint cycle and retry from the parked cursor (bounded);
    /// a compaction-floor refusal ends the merging of this lap and stands
    /// the backlog (the cycles return the merged extents; the count phase
    /// still runs, so the published candidates stay exact). `deadline`
    /// bounds the call (`None` = one whole volume lap) — the job fabric
    /// passes its throttle chunk and `duty_park`s between calls.
    pub async fn defrag_merge_sweep(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<VolumeMergeSweep, KvError> {
        let mut smo = self.smo.lock().await;
        let mut total = VolumeMergeSweep::default();
        for attempt in 0..=4 {
            let report = self.run_merge_sweep(&mut smo, false, deadline).await?;
            total.merges += report.merges;
            total.interior_merges += report.interior_merges;
            total.root_collapses += report.root_collapses;
            total.space_refused |= report.space_refused;
            total.candidates = report.candidates;
            total.lap_complete = report.lap_complete;
            total.refusal = report.refusal;
            if report.space_refused {
                log::info!(
                    "defrag-meta merge pass on {:?} stopped merging at the compaction \
                     floor; the checkpoint cycles return the merged extents",
                    self.path
                );
                self.merge_backlog.store(true, Ordering::Release);
            }
            match report.refusal {
                Some(r) if attempt < 4 => {
                    // The nudge's ladder: one forced cycle refills the ring
                    // reserve / drains the FIFO, then resume from the
                    // parked cursor.
                    log::debug!(
                        "defrag-meta merge pass on {:?}: {r:?} — one checkpoint cycle, then \
                         the lap resumes",
                        self.path
                    );
                    self.checkpoint_cycle(&mut smo, true).await?;
                }
                _ => break,
            }
        }
        Ok(total)
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
        let mut compacted = 0u64;
        for &(tree_id, addr) in targets {
            // The census names nodes by header id + address; on a forest
            // volume the header id is 0 for every slot tree, so the
            // owning tree is the one the cached node's stamp names.
            let Some(tree) = self.tree_for_census_target(tree_id, addr) else {
                continue; // unknown tree id / unmapped node: stale census entry
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
    /// §4.6a (e): `(tree_id, node_addr)` of every UNDERFULL non-root leaf
    /// (fold ≤ ¼ capacity, covered tombstones credited as elided) — the
    /// `mergeable_leaves` face, computed with the sweep's own predicate.
    pub merge_candidates: Vec<(u8, u64)>,
}

/// A volume's merge lap across its trees (`KvMetaBackend::run_merge_sweep`):
/// per tree (in `all_trees` order) whether its lap completed since the last
/// publish, and the exact candidates the completed trees reported.
#[derive(Debug, Default, Clone)]
pub(super) struct VolumeLap {
    pub(super) done: Vec<bool>,
    pub(super) candidates: u64,
}

/// [`KvMetaBackend::merge_candidates_audit`]'s answer: the gauge and a
/// fresh census, each with the durable tail it was counted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeCandidatesAudit {
    /// `meta_kv_merge_candidates` as published by the last lap or census.
    pub gauge: u64,
    /// The durable tail that publish counted under.
    pub gauge_tail: u64,
    /// The sweep predicate over every resident leaf under `gauge_tail`.
    pub census_at_gauge_tail: u64,
    /// The sweep predicate over every resident leaf under `census_tail`.
    pub census_now: u64,
    /// The durable tail in force now.
    pub census_tail: u64,
}

/// What one bounded volume sweep call did (§4.6a (e), finalized).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VolumeMergeSweep {
    /// Sibling merges this call ran (`meta_kv_node_merges`).
    pub merges: u64,
    /// The level-≥ 1 subset (`meta_kv_interior_merges`).
    pub interior_merges: u64,
    /// Root collapses (`meta_kv_root_collapses`).
    pub root_collapses: u64,
    /// The EXACT underfull-leaf count over the volume — valid when
    /// `lap_complete` (the `meta_kv_merge_candidates` publish).
    pub candidates: u64,
    /// Every tree's lap completed in this call (the publish instant).
    pub lap_complete: bool,
    /// A compaction-floor refusal stopped merging in some tree's lap.
    pub space_refused: bool,
    /// A merge refused ring reserve / FIFO room stopped the call with its
    /// merges so far reported and the cursor parked (`KvTree::merge_
    /// underfull`): the caller's checkpoint cycle remedies it and the
    /// next call resumes.
    pub refusal: Option<crate::meta_backend::kv::tree::SweepRefusal>,
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

/// The bounded window [`KvMetaBackend::await_transient_flock_release`]
/// polls a transient flock holder for before falling back to the loud
/// refusal. Generous vs. the ms-grade shapes it absorbs (one checkpoint /
/// conveyor pass pin, a udev change-event re-probe, a released `LOCK_SH`
/// classification probe); a genuinely live holder pays it once before the
/// refusal.
const TRANSIENT_FLOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

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

/// The durable per-volume identity every per-volume admission mode is
/// keyed on (KD-5: *never a path, an ordinal, or a set position*).
///
/// Derived from the volume's **superblock uuid** — the sole durable
/// per-volume identity a mount can read before any tree is routed
/// (AGENTS.md §Filesystem generation identity), and the one
/// `MetaSetDiscovery` already carries in canonical order. Rendered in the
/// house `vol-{16 hex}` style so the operator surface is one shape.
///
/// The `meta_volumes` FormatConfig record also carries a `vol-{hex}` id,
/// but it is a documented MIRROR: absent on sets the lifecycle verbs never
/// touched and synthesized as `meta-pos-N` by `config_ops`, so keying
/// admission on it would key it on a position after all.
pub fn durable_volume_id_of(sb_uuid: &[u8; 16]) -> String {
    format!("vol-{:016x}", xxhash_rust::xxh3::xxh3_64(sb_uuid))
}

/// How a manager control entry takes its ring-0 admission
/// (`write_control_entry`): a WIRE verb / the cadence tries once and its
/// refusal is the caller's retry (the peer's resend, the next cycle); the
/// commit DOOR's first-touch acquire parks like the user commit it runs
/// inside (review round 2, Issue 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAdmit {
    Try,
    Park,
}

/// One slot the manager granted (design-symmetric-metadata §5.1.2 / §6.3,
/// PR 4): its lease generation and the words its tree carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotGrant {
    pub slot: super::record::ForestSlot,
    pub g: u32,
    pub words: crate::slot_lease_core::SlotWords,
    /// The caller already held it (KD-SYM-7's replay).
    pub already: bool,
}

/// `AcquireSlot`'s answer (§5.1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireSlotReply {
    Granted(SlotGrant),
    Already(SlotGrant),
    /// Another appender holds it — ship to it.
    Refused {
        holder: u32,
        g: u32,
    },
}

/// What `JoinAppender` answered (design-symmetric-metadata §6.3
/// `Joined`): the appender's id, its page's directory slot A, its ring's
/// segment table, its grant, and whether the page was ALREADY `Live`
/// under the caller's identity (KD-SYM-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOutcome {
    pub appender_id: u32,
    pub page_addr: u64,
    pub ring_segments: Vec<super::superblock::ExtentRef>,
    pub grant: Vec<super::appender::GrantRun>,
    pub already: bool,
}

/// One fsck C13 candidate (design-symmetric-metadata §5.8.5): a heap
/// extent `appender`'s grant holds claimed that no tree root reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OrphanImageExtent {
    pub appender: u32,
    pub extent: u64,
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

    /// Layer A transient-holder absorption: poll the flock for a bounded
    /// window ([`TRANSIENT_FLOCK_WAIT`]) when the holder is provably or
    /// plausibly transient; refuse instantly otherwise. The three waitable
    /// arms, each pinned in `tests/mount_writer_guard_tests.rs`:
    ///
    /// 1. **Same-process teardown pin** (2026-07-26;
    ///    `test_reopen_waits_out_same_process_teardown_pin`): the claim
    ///    names THIS process (pid + boot) — a backend whose last external
    ///    `Arc` was dropped but whose struct is still pinned by a mid-pass
    ///    background task, or a genuinely live same-process double mount.
    ///    A dying holder frees it within one pass (ms-grade); a live one
    ///    never does and the caller falls back to the loud refusal.
    /// 2. **Dead same-host holder under a transient probe** (rung-9
    ///    finding #5, documented at its arm below).
    /// 3. **Anonymous holder — no claim at all** (the mw_fleet `--owners`
    ///    finding, 2026-08-23; documented at its arm below): udev's
    ///    change-event flock and released-by-contract probes, which no
    ///    squeezefs record can ever name.
    ///
    /// Returns the acquired guard fd, or `None` (refuse) on a live/foreign
    /// claimed holder or bound expiry.
    async fn await_transient_flock_release(
        path: &Path,
        holder: &Option<(WriterClaim, u64)>,
    ) -> Option<std::fs::File> {
        const POLL: std::time::Duration = std::time::Duration::from_millis(5);

        let waitable = match holder {
            Some((claim, _)) => {
                let same_process = claim.pid == std::process::id() && claim.boot == read_boot_id();
                // Rung-9 finding #5 (the S8-b E2 leg, live): a successor
                // mounting over a PROVABLY-DEAD same-host holder can meet a
                // *transient* `LOCK_SH` at its one-shot NB acquire — a
                // reader's / co-writer's released-immediately mount probe
                // (5 rejoining co-writers hammer them while the authority
                // is dark). The dead holder cannot own the flock and SH
                // probes release by contract, so this shape gets the SAME
                // bounded wait-out as the same-process teardown race. A
                // LIVE same-boot holder and every foreign-boot claim still
                // refuse instantly (unchanged posture).
                let dead_same_host = claim.boot == read_boot_id() && pid_provably_dead(claim.pid);
                same_process || dead_same_host
            }
            // The mw_fleet --owners finding (2026-08-23): a held flock with
            // NO on-volume claim is an ANONYMOUS holder — udevd's
            // BLOCK_DEVICE_LOCKING change-event probe (the kernel
            // synthesizes a `change` uevent on every write-close of the
            // node, and udevd flocks it while re-probing), a reader's /
            // co-writer's released-by-contract `LOCK_SH` probe, or a
            // concurrent mount still inside its pre-claim window. The
            // first two release in milliseconds; the third commits its
            // claim (the volume's first post-replay mutation) and keeps
            // holding — the bound expiry refuses it, and the caller
            // re-probes so the refusal names it. A live squeezefs writer
            // is never claim-less past its pre-claim window, so instant
            // refusal here attributed udev's lock to a squeezefs process
            // that did not exist.
            None => true,
        };
        if !waitable {
            return None; // live/foreign claimed holder: refuse instantly
        }
        let deadline = std::time::Instant::now() + TRANSIENT_FLOCK_WAIT;
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

    /// The Layer-B2 standing of this volume's claim, in the per-volume
    /// admission ladder's vocabulary — **the gate's own classification**,
    /// not a second spelling of it.
    ///
    /// `crate::partial_authority`'s module doc states why this exists:
    /// the dead-pid proof, the boot-id scope and the `CLIENT_STALE_TTL_SECS`
    /// window are the D0 gate's law and must have exactly one
    /// implementation, so the gather asks the gate rather than re-deriving
    /// it. Read through a probe open (never blocked, never writes); no
    /// witness is passed, so the `PeerAuthority` arm stays structurally
    /// unreachable here (R12).
    pub async fn claim_standing(&self) -> crate::partial_authority::ClaimStanding {
        use crate::partial_authority::ClaimStanding;
        let raw = self.getxattr(1, WRITER_CLAIM_XATTR).await.ok().flatten();
        match self.classify_claim(raw, unix_now_secs(), None) {
            ClaimEvidence::Reclaimable => ClaimStanding::Reclaimable,
            ClaimEvidence::FreshForeign(_) | ClaimEvidence::PeerAuthority(_) => {
                ClaimStanding::Fresh
            }
            ClaimEvidence::StaleForeign(_) => ClaimStanding::Stale,
        }
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
            let acquire = self.rsv_acquire(&rsv, key).await;
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
                    let wero = self.meta_wero;
                    rsv_call(&rsv, move |c| {
                        if wero {
                            c.preempt_registrants_only(key, victim)
                        } else {
                            c.preempt(key, victim)
                        }
                    })
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
        // The membership plane records eras on this volume in two more
        // places than the ladder record: the claim set (whose stores
        // stamp the storing PROCESS's era — a full writer's arm imports
        // the max across every volume it appends to) and a crashed
        // owner's rendezvous record. The barrier below must publish a
        // term above ALL of them — §6.7's arm law refuses an owner whose
        // era does not exceed every recorded predecessor's, and its
        // remedy message names this gate ("Arm after the D0 gate's claim
        // barrier, which is what publishes the new term"). Same-volume
        // records only (sweep row 17(c): a peer volume's era belongs to
        // its owner). The mw_fleet --owners finding, 2026-08-23.
        let recorded = crate::membership::max_recorded_era(self).await;
        let prior = prior_claim_term.max(stored).max(recorded);
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

    /// The metadata namespace's acquire in the rtype this mount holds
    /// (`meta_wero`): WERO for an armed forest's manager, the shipped
    /// Write Exclusive otherwise.
    async fn rsv_acquire(
        &self,
        rsv: &Arc<dyn crate::meta_backend::reservation::ReservationClient>,
        key: u64,
    ) -> std::io::Result<()> {
        let wero = self.meta_wero;
        rsv_call(rsv, move |c| {
            if wero {
                c.acquire_write_exclusive_registrants_only(key)
            } else {
                c.acquire_write_exclusive(key)
            }
        })
        .await
    }

    /// This mount's reservation key on the metadata namespace (0 on a
    /// non-PR substrate) — the holder key a registrant's report names.
    /// First product caller: PR 4's joiner (it verifies the standing WERO
    /// hold is the manager's before registering under it); the fence
    /// contracts read it today.
    pub fn writer_guard_pr_key(&self) -> u64 {
        self.pr_key
    }

    /// Whether the symmetric PLANE is armed on this volume (KD-SYM-13's
    /// subject): a declared appender partition — or, from PR 4, a join or
    /// a lease. A solo forest mount is NOT the arm.
    pub fn symmetric_arm_engaged(&self) -> bool {
        self.appenders.as_ref().is_some_and(|a| a.is_partitioned())
    }

    /// KD-SYM-13: decide the metadata namespace's fence posture for this
    /// open. Returns whether the manager holds WERO. Refuses loud (a) an
    /// armed plane on a non-PR substrate and (b) `SQUEEZEFS_META_PR_WERO=0`
    /// on a PR-capable one, unless `SQUEEZEFS_SYM_ALLOW_NON_PR=1` — which
    /// is announced at every mount that uses it, never a default. An
    /// unarmed mount (flat, or a solo forest) keeps the shipped posture
    /// verbatim and reads none of the knobs.
    fn decide_meta_fence_posture(&self) -> std::result::Result<bool, KvError> {
        if !self.symmetric_arm_engaged() {
            return Ok(false);
        }
        let allow_non_pr = crate::env_knobs::bool_knob("SQUEEZEFS_SYM_ALLOW_NON_PR", false);
        let wero_wanted = crate::env_knobs::bool_knob("SQUEEZEFS_META_PR_WERO", true);
        let pr_capable = self.reservations.is_some();
        if !pr_capable {
            if !allow_non_pr {
                return Err(KvError::Busy(format!(
                    "{}: the symmetric metadata plane is armed (a declared appender partition) \
                     but this namespace advertises no NVMe Persistent Reservations (RESCAP=0 or \
                     not an NVMe namespace) — fencing between appenders would be \
                     detection-grade only, and detection-grade is not loss-free (a zombie's \
                     frame past the recorded tail is an ACKED-loss class, \
                     design-symmetric-metadata §5.8.2). KD-SYM-13 refuses to arm here; set \
                     SQUEEZEFS_SYM_ALLOW_NON_PR=1 to opt in LOUDLY (lab use), or use a \
                     PR-capable namespace (the kernel nvmet target)",
                    self.path.display()
                )));
            }
            log::warn!(
                "meta volume {}: SYMMETRIC PLANE ARMED ON A NON-PR SUBSTRATE under \
                 SQUEEZEFS_SYM_ALLOW_NON_PR=1 (KD-SYM-13) — fencing between appenders is \
                 DETECTION-GRADE ONLY, which is not loss-free (design-symmetric-metadata \
                 §5.8.2): a zombie appender's frame past the recorded tail is an acked-loss \
                 class the device would have refused. Lab posture, never a default",
                self.path.display()
            );
            return Ok(false);
        }
        if !wero_wanted {
            if !allow_non_pr {
                return Err(KvError::Busy(format!(
                    "{}: SQUEEZEFS_META_PR_WERO=0 asks for the detection-grade Write Exclusive \
                     posture on a PR-CAPABLE namespace with the symmetric plane armed — the \
                     device could fence every appender (WERO + registrants, \
                     design-symmetric-metadata §5.8.1) and detection-grade is not loss-free \
                     (§5.8.2). Refused unless SQUEEZEFS_SYM_ALLOW_NON_PR=1 (KD-SYM-13)",
                    self.path.display()
                )));
            }
            log::warn!(
                "meta volume {}: SQUEEZEFS_META_PR_WERO=0 on a PR-capable namespace under \
                 SQUEEZEFS_SYM_ALLOW_NON_PR=1 — the manager holds the shipped Write Exclusive \
                 and the other appenders are fenced DETECTION-GRADE ONLY (KD-SYM-13, lab \
                 posture)",
                self.path.display()
            );
            return Ok(false);
        }
        Ok(true)
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
            let wero = self.meta_wero;
            match rsv_call(&rsv, move |c| {
                if wero {
                    c.release_registrants_only(key)
                } else {
                    c.release(key)
                }
            })
            .await
            {
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
    /// post-replay mutation *by construction*, design §5.0 B2) — and, at
    /// the other end, `shutdown_signalled` once `shutdown()` has stored
    /// the flag and sent the checkpoint task its permit (the edge a
    /// liveness harness orders a seam release on).
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
                            self.rsv_acquire(&rsv, key).await
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
        let mut inner = Self::open_inner(path, OpenPosture::Writer).await?;
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
///
/// Public as an OPAQUE handle only (D-1c): the stage halves
/// ([`KvMetaBackend::stage_layout_and_size`] and friends) return one and
/// [`KvMetaBackend::commit_tx_group`] consumes a set — nothing outside
/// this module stages a record into it.
pub struct KvTx {
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
        Self::empty()
    }

    /// A transaction with nothing staged — commits as an inline `Ok(())`
    /// (no pass, no entry). The one constructor outside this module (the
    /// group-commit contracts' degenerate member).
    #[track_caller]
    pub fn empty() -> Self {
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

    /// PB-class files, PR 1 (docs/design-kvmap-block-map-tree.md §3):
    /// stage the transaction's block-map operations — one `Put` per
    /// mapping bound, one `Delete` per mapping removed, into the **same**
    /// tx as the layout record and the inode record. One tx = one
    /// checksummed journal entry (§4.10), so a mapping can never
    /// disagree with the head/size that justifies it, not even across a
    /// torn write; and the publish stays ONE commit (the
    /// write-commit-economy collapse is not re-split). The reserved
    /// index `u32::MAX` (design A5) refuses here as a Result — a staged
    /// tx never carries it.
    fn stage_block_map(
        &mut self,
        ops: &[super::block_map::BlockMapOp],
    ) -> std::result::Result<(), KvError> {
        let mut puts = 0u64;
        let mut deletes = 0u64;
        for op in ops {
            let key = op.key()?.to_vec();
            match op {
                super::block_map::BlockMapOp::Put { entry, .. } => {
                    self.stage_put(super::record::TREE_BLOCK_MAP, key, entry.encode());
                    puts += 1;
                }
                super::block_map::BlockMapOp::Delete { .. } => {
                    self.stage_delete(super::record::TREE_BLOCK_MAP, key);
                    deletes += 1;
                }
            }
        }
        if puts > 0 {
            super::META_KV_BLOCK_MAP_PUTS.fetch_add(puts, std::sync::atomic::Ordering::Relaxed);
        }
        // PR 6a: the run-emission engagement gauge counts staged RUN/RUN2
        // Puts apart (design §5 `block_map_tree_run_puts`).
        let run_puts = ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    super::block_map::BlockMapOp::Put { entry, .. } if entry.run_len() > 1
                )
            })
            .count() as u64;
        if run_puts > 0 {
            super::META_KV_BLOCK_MAP_RUN_PUTS
                .fetch_add(run_puts, std::sync::atomic::Ordering::Relaxed);
        }
        if deletes > 0 {
            super::META_KV_BLOCK_MAP_DELETES
                .fetch_add(deletes, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
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
    /// The submitter's result channel: `(use_delta, staged_version,
    /// recompute verdict)` — the staged link's version (0 on a
    /// full-Put/unversioned commit), which is what lets a co-writer chain
    /// without a refetch (the design-mw-layout-versions §6 residual this
    /// rung pays), plus the finding-36 owner-recompute verdict (see
    /// [`RecomputedReleases`]).
    done: squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<MergeOutcome>>,
}

/// Finding 36 — a chained/composed merge's owner-recompute verdict,
/// returned beside `(use_delta, staged_version)`. `None` = the caller's
/// accounting frame stood (the solo/un-recomputed shape: its own displaced
/// frees stay authoritative). `Some(released)` = the staged accounting was
/// RECOMPUTED against this authority's own head (rung 19/20), and
/// `released` names every DATA-block reference the committed transition
/// dropped (map-blob custody stays on the displaced-blob post-commit free).
/// The S9 publish serve owns these device frees strictly after commit Ok,
/// and the reply's `recomputed` flag stands the co-writer's caller-frame
/// free stream down — a private view that may name blocks whose free
/// already ran (the finding-36 refusal/leak pair).
pub type RecomputedReleases = Option<Vec<super::block_refs::BlockRef>>;

/// One chained merge's result: `(use_delta, staged_version, recompute
/// verdict)`.
pub type MergeOutcome = (bool, u64, RecomputedReleases);

/// One recompute's output (rung 19/20): the ops that REPLACE the caller's
/// frame in the transaction, plus the frame's RAM-only lifetimes (finding
/// 15 — see `recompute_refs_against_map`), which are staged NOWHERE (no
/// durable record ever existed) and travel only into the recompute's
/// post-commit free set.
struct RecomputedFrame {
    ops: Vec<super::block_refs::BlockRefOp>,
    ram_only_releases: Vec<super::block_refs::BlockRef>,
}

/// One crossing/migration train's accounting
/// ([`KvMetaBackend::migrate_block_map_train`]): what the ledger counters
/// (`map_migrate_records` / `publish_map_record_bytes`) and the A1
/// resumed verdict (`preexisting > 0` on a FIRST crossing = a crashed
/// prior train's residue was reconciled) are fed from. Since PR 5b it
/// also carries the committed head generation (design §11's belt — the
/// caller stamps it as its next base), the f36b recompute verdict, and a
/// LOCAL claims-scoped train's recompute-released blocks (served arms
/// travel it back empty — the owner frees its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapMigrateOutcome {
    /// Map operations staged (Puts + Deletes, flip-tx tail included).
    pub records: u64,
    /// Key + value bytes of the staged Puts.
    pub record_bytes: u64,
    /// Tree-7 records that existed BEFORE the train ran.
    pub preexisting: u64,
    /// The committed head generation (0 = un-minted — solo posture).
    pub gen: u64,
    /// The staged accounting was RECOMPUTED (a claims-scoped train with
    /// the rung-19 resolver armed) — the caller's frame-derived displaced
    /// frees stand down for this publish.
    pub recomputed: bool,
    /// A LOCAL claims-scoped train's recompute-released data blocks — the
    /// save's post-guard venue runs them through the shipped-free ladder
    /// (RES-1: never freed under the caller's 3.5 stripe).
    pub released: Vec<super::block_refs::BlockRef>,
    /// PR 6c-i: the released records' router-true KEYS (index-paired with
    /// the recompute's release resolves) — the SOLO overlay save's free
    /// tail (tier purge + deferred key free) consumes these; the mw arms
    /// keep the `BlockRef` ladder above.
    pub released_keys: Vec<String>,
    /// PR 6b: the committed head's A2 sweep cursor (`None` = no live
    /// sweep) — the caller's RAM head id republish carries it, so the
    /// CachedMetadata-head-vs-durable skew rule keeps holding mid-sweep.
    pub sweep_cursor: Option<u32>,
    /// PR 6b: residue records the train's extend barrier deleted
    /// (design §3's write-during-sweep law).
    pub swept_records: u64,
    /// PR 6b: the barrier's freed block keys — the caller's post-commit
    /// reclaim enqueue (RES-1: never freed under the train's guards).
    pub swept_freed: Vec<String>,
}

/// PR 6b (design §3/A2): the verdict of one background sweep chunk.
#[derive(Debug)]
pub enum SweepChunkOutcome {
    /// The head carries no live sweep cursor (never planted, already
    /// terminal, or a publish's extend barrier absorbed the span) — the
    /// job completes with nothing to do.
    NoCursor,
    /// One chunk tx committed (Deletes + releases + cursor advance);
    /// records remain above the advanced cursor. `freed` is the caller's
    /// post-commit reclaim enqueue (RES-1: the 4a guard is gone).
    Progress { records: u64, freed: Vec<String> },
    /// The terminal chunk committed — the cursor CLEARED in the same tx
    /// as the final Deletes. A corpse owner (`nlink == 0`) is the
    /// caller's to destroy.
    Terminal { records: u64, freed: Vec<String> },
}

/// One [`KvMetaBackend::kvmap_sweep_page`] result: the staged ops for one
/// chunk tx plus the resume cursor (`None` = the span is exhausted).
#[derive(Debug, Default)]
struct SweepPageOps {
    ops: Vec<block_map::BlockMapOp>,
    rel: Vec<block_refs::BlockRefOp>,
    freed: Vec<String>,
    records: u64,
    resume: Option<u32>,
}

/// PR 6b straddler dissolve (the run face of the §12a dissolve law): the
/// surviving head `[key, key + keep)` of a run whose tail the sweep
/// removes — same start offset/stamp arithmetic, shorter span; `keep == 1`
/// collapses to the point form (a 1-run is not an encodable run). Only
/// meaningful for run-class entries with `keep < run_len` — the callers'
/// covering-floor probe guarantees both.
fn shortened_run_entry(entry: &block_map::MapEntry, keep: u32) -> block_map::MapEntry {
    use block_map::MapEntry;
    match entry {
        MapEntry::Run {
            vol_tag,
            start_offset,
            ..
        } => {
            if keep == 1 {
                MapEntry::Point {
                    vol_tag: *vol_tag,
                    offset: *start_offset,
                }
            } else {
                MapEntry::Run {
                    vol_tag: *vol_tag,
                    start_offset: *start_offset,
                    len: keep,
                }
            }
        }
        MapEntry::RunStamped {
            vol_tag,
            start_offset,
            start_incarnation,
            ..
        } => {
            if keep == 1 {
                MapEntry::PointStamped {
                    vol_tag: *vol_tag,
                    offset: *start_offset,
                    incarnation: *start_incarnation,
                }
            } else {
                MapEntry::RunStamped {
                    vol_tag: *vol_tag,
                    start_offset: *start_offset,
                    len: keep,
                    start_incarnation: *start_incarnation,
                }
            }
        }
        // Point-class records never cover past their own key; the floor
        // probe cannot nominate one.
        other => other.clone(),
    }
}

/// PR 5b (design §11 law b) — the CLAIMS-SCOPED mode input for
/// [`KvMetaBackend::migrate_block_map_train`]: adopt under a take claim,
/// delete under a release-without-take claim, never delete-by-absence
/// (the whole-map diff stays local-authority-only, Rev 1.3 #2). The
/// caller owns custody scoping (span ∩ claims ∖ demoted) and the f28
/// live-binding filter on the adopt candidates.
#[derive(Debug, Clone, Default)]
pub struct MapTrainClaims {
    /// The shipper's carried base generation, checked against the durable
    /// head under the held 4a (§11's belt) — `None` skips the check (the
    /// authority-local claims arm composes against the head it serializes
    /// on).
    pub base_gen: Option<u64>,
    /// take-claim indices.
    pub take: std::collections::BTreeSet<u32>,
    /// release-claim indices.
    pub release: std::collections::BTreeSet<u32>,
    /// PR 6c-i (design §14 S2 pre-fix b): `true` ⇔ this train executes a
    /// SHIPPED verb on behalf of a peer (`serve_map_train`). The gen-bump
    /// criterion keys on it — a SOLO mount's LOCAL claims trains (the 6c
    /// overlay saves) mint no generation (the solo-dark byte-identity
    /// law); only served and custody-armed trains bump the belt. It also
    /// selects the cursor law's arm: served trains never barrier (a live
    /// cursor refuses retried-class), local claims trains run the extend
    /// barrier like the whole-map train they replace.
    pub served: bool,
    /// PR 6c-i (design §14 S2 option ii): `true` ⇔ this is a LOCAL
    /// OVERLAY save — displacement discovery rides the train's f36b
    /// recompute through the CALLER's `ref_for` even where the rung-19
    /// global resolver is unarmed (solo mounts). Only the routing save
    /// sets it (its `ref_for` is the live `block_ref_for` closure);
    /// other local claims trains keep the 5b posture — recompute only
    /// under the global resolver, caller frame preserved otherwise.
    pub overlay: bool,
    /// Finding 46 (design §3 Publish / §14 S2): `true` ⇔ this is a LOCAL
    /// publish WINDOW on a whole-map-authority kvmap ino — `take` = the
    /// window's indices, `release` empty, `entries` = the RAM map's
    /// bindings at exactly those indices. The train probes exact-only
    /// (no floor scan) and NEVER recomputes: the caller's RAM merge
    /// captured every displacement, so its frame commits verbatim (the
    /// f36 preservation arm) and its displaced-free stream stays its own.
    /// Delete-by-absence stays OFF — the non-publish saves (truncate,
    /// punch, fsync-persist) and the sweep own deletes. Only the routing
    /// save sets it.
    pub window: bool,
}

/// RAII registration in the in-flight crossing registry (design A3) —
/// deregisters on EVERY exit, error paths included, so a failed train
/// can never leave an ino permanently "in flight".
struct CrossingGuard<'a> {
    map: &'a scc::HashMap<Ino, ()>,
    ino: Ino,
}

impl<'a> CrossingGuard<'a> {
    fn register(map: &'a scc::HashMap<Ino, ()>, ino: Ino) -> Self {
        let _ = map.insert_sync(ino, ());
        Self { map, ino }
    }
}

impl Drop for CrossingGuard<'_> {
    fn drop(&mut self) {
        let _ = self.map.remove_sync(&self.ino);
    }
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
    /// The appender region whose ring this tx journals into (0 on a flat
    /// volume and for every manager-owned record — `region_of_records`).
    region: u32,
    /// Exact journal entry length ([`entry_len_for`]) — the Σ-admission
    /// and the drain byte cap read it.
    len: u64,
    /// Enqueue instant (`meta_txpass_phase_ns` tx_queue_wait — the
    /// rewrite-publish-drain decomposition, 2026-08-01).
    enqueued_at: std::time::Instant,
    /// op-trace (audit A2): the originating op's trace id (0 =
    /// untraced) — the pass stamps its stages for every traced member.
    trace_id: u64,
    /// D4.a: the `KvTx` construction site (counted on success).
    site: &'static std::panic::Location<'static>,
    /// Issue 13 (§5.5 revision 2): the tx's DLM I/D guards, held by THIS
    /// entry until the tx's terminal outcome — dropped at fan-out, once
    /// the outcome is computed and BEFORE it is sent (D-3; post-rollback
    /// on failure). Never read, only owned: the RAII hold IS the same-key
    /// exclusion.
    _guards: Arc<[DlmGuard]>,
    /// PR 4 (review round 2, Issue 6): the tx's door tokens — one per
    /// slot its records name — owned by THIS entry until the terminal
    /// outcome, like the guards: a slot's release drains them before its
    /// flush, so no admitted tx can land after the flush's snapshot.
    /// `None` on an unarmed mount.
    _door: Option<super::slot_lease::DoorPass>,
    /// Fan-out channel. A dead receiver (dropped committer future) is
    /// harmless — semantically identical to timeout-fires-after-commit.
    done: squeezefs_ipc::sqz_channel::oneshot::Sender<std::result::Result<(), KvError>>,
}

/// One conveyor **window** (D-2, module docs "The two-stage commit
/// conveyor"): a batch past its RAM apply, its entries SUBMITTED to the
/// journal ring, handed from the apply pass to the per-volume durability
/// lane, which awaits the write, the completed prefix and (strict) the
/// barrier, then fans the members' terminal outcomes out — in handoff
/// order, which is drain order, which is journal-seq order.
struct ConveyorWindow {
    /// The ring this window's reservation lives in (the region's ring at
    /// the pass; region 0's is the fixed ring) and the region's id.
    ring: Arc<JournalRing>,
    region: u32,
    /// The batch members (queue order), each still co-owning its DLM
    /// guards — released only at the terminal outcome the lane computes.
    entries: Vec<QueuedTx>,
    /// Members rolled OUT of the window at apply (`(index, error)`); their
    /// sub-ranges are unwritten holes.
    failed: Vec<(usize, KvError)>,
    /// The batch's contiguous registered reservation; the lane completes
    /// it once the write's outcome is known (`res_open` tracks that).
    res: Reservation,
    res_open: bool,
    /// First-touch pre-images for the §4.4 pt 4 seq-conditional rollback
    /// the lane runs if the write fails.
    undo: Vec<UndoKey>,
    /// The in-flight ring write: `Ok(None)` when nothing was submitted
    /// (every member failed at apply), `Err` when the encode refused
    /// before any byte was submitted (a failed write with nothing landed).
    write: Option<std::result::Result<Option<super::journal::EntriesWriteInFlight>, KvError>>,
    /// The apply pass's start (`window_total`) and the handoff instant
    /// (`window_lane_wait`).
    t_pass: std::time::Instant,
    t_handoff: std::time::Instant,
    /// The write's submission instant, captured when the lane takes the
    /// in-flight handle (`pass_journal_write`'s start; `None` = nothing
    /// was submitted).
    submitted_at: Option<std::time::Instant>,
    /// op-trace (audit A2): the batch's traced members.
    traced: crate::op_trace::TracedBatch,
}

/// What one apply pass produces (`run_batch`): the pre-reserve terminal
/// outcomes (a batch that failed as a unit) or the window for the lane —
/// never both non-empty.
struct BatchProduct {
    outcomes: Vec<(QueuedTx, std::result::Result<(), KvError>)>,
    /// One window per appender region the batch touched (exactly one on
    /// a flat volume / the manager alone).
    windows: Vec<ConveyorWindow>,
}

impl ConveyorWindow {
    /// Non-blocking: has this window's write landed (or is there nothing
    /// to wait for)? The lane's grouping probe.
    fn write_landed(&mut self) -> bool {
        match self.write.as_mut() {
            Some(Ok(Some(inflight))) => inflight.poll_done(),
            _ => true,
        }
    }
}

/// The §5.5 panic guard for the APPLY stage: pipeline state that must
/// never be dropped on the floor, armed for the whole batch pass. Every
/// NORMAL path (success and failure alike) empties it; [`Drop`] therefore
/// fires with content only when the pass unwinds (panic) or the detached
/// task is torn down mid-await (runtime shutdown) — and then performs the
/// all-sync §5.5 cleanup: release the un-transferred `Admission`,
/// `complete()` any registered reservation as **abandoned** (the §4.4
/// pt 4 unwritten-range mechanism — replay's checksum walk drops it), fail
/// the batch's oneshots with EIO, fail out anything still queued behind
/// the dead leader, release leadership, and escalate loud. A batch can
/// fail; `completed_upto` can never wedge.
struct PassSentinel<'a> {
    be: &'a Arc<KvMetaBackend>,
    /// The ring this batch reserves in and the region it belongs to.
    ring: Arc<JournalRing>,
    region: u32,
    /// The pass start (`pass_total`, and the window's `window_total`).
    t_pass: std::time::Instant,
    /// Batch members not yet at their terminal outcome.
    entries: Vec<QueuedTx>,
    /// Members WITH their terminal outcome computed, awaiting fan-out
    /// (the pass task sends these after releasing the backend ref) — the
    /// pre-reserve batch failures.
    outcomes: Vec<(QueuedTx, std::result::Result<(), KvError>)>,
    /// Σ admission, held from admit until transfer to the reservation.
    admission: Option<super::journal_core::Admission>,
    /// The registered batch reservation, held until it moves into the
    /// window at handoff.
    reservation: Option<Reservation>,
    /// Records are applied to RAM and neither journaled nor rolled back
    /// (the span where RAM would silently diverge from replay): a panic
    /// here additionally fail-stops the volume — reads are
    /// RAM-authoritative and writeback would make the divergence durable;
    /// with `failed` latched the checkpoint task idles and the divergence
    /// dies at remount.
    applied_unrolled: bool,
    /// The window built at handoff, taken by the pass task the instant
    /// the pipeline returns (no await in between).
    window: Option<ConveyorWindow>,
    /// op-trace (audit A2): the batch's traced members — every pass-level
    /// phase record stamps its stage for each of them.
    traced: crate::op_trace::TracedBatch,
}

impl Drop for PassSentinel<'_> {
    fn drop(&mut self) {
        if self.entries.is_empty()
            && self.outcomes.is_empty()
            && self.admission.is_none()
            && self.reservation.is_none()
            && self.window.is_none()
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
            self.ring.complete(&res);
        }
        if let Some(adm) = self.admission.take() {
            self.ring.core().release(adm);
        }
        let mut batch_n = self.entries.len();
        for q in self.entries.drain(..) {
            let _ = q.done.send(Err(KvError::Io(self.be.eio(
                "commit conveyor pass panicked — batch failed loud (§5.5 panic guard)",
            ))));
        }
        // A window built but not handed off: its write is in flight and
        // its outcome unknown — abandon it exactly like the lane's
        // sentinel would (RAM diverges from replay ⇒ fail-stop).
        let mut applied_unrolled = self.applied_unrolled;
        if let Some(w) = self.window.take() {
            batch_n += w.entries.len();
            applied_unrolled = true;
            self.be
                .abandon_window(w, "conveyor pass panicked at the durability handoff");
        }
        if applied_unrolled && !self.be.failed.swap(true, Ordering::AcqRel) {
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

/// The §5.5 panic guard for the DURABILITY stage (D-2): the windows the
/// lane took together and has not yet answered. Every normal path empties
/// it; [`Drop`] with content = the lane unwound (panic) or was torn down
/// mid-await, and then: deliver computed outcomes, abandon every window
/// still in progress (complete its reservation as abandoned, fail its
/// members EIO), fail out every window still queued behind the dead lane,
/// release lane leadership, **fail-stop the volume** (every abandoned
/// window's RAM apply is un-journaled-or-unknown — the divergence dies at
/// remount), and escalate loud.
struct LaneSentinel<'a> {
    be: &'a Arc<KvMetaBackend>,
    /// Windows in progress, in journal order.
    windows: Vec<ConveyorWindow>,
    /// Members WITH their terminal outcome computed, awaiting fan-out.
    outcomes: Vec<(QueuedTx, std::result::Result<(), KvError>)>,
}

impl Drop for LaneSentinel<'_> {
    fn drop(&mut self) {
        if self.windows.is_empty() && self.outcomes.is_empty() {
            return;
        }
        for (q, outcome) in self.outcomes.drain(..) {
            let _ = q.done.send(outcome);
        }
        super::META_CONVEYOR_PASS_PANICS.fetch_add(1, Ordering::Relaxed);
        let mut members = 0usize;
        for w in self.windows.drain(..) {
            members += w.entries.len();
            self.be
                .abandon_window(w, "conveyor durability lane panicked — window failed loud");
        }
        let mut stranded = 0usize;
        loop {
            for w in self.be.durability_lane.drain(usize::MAX, u64::MAX) {
                stranded += w.entries.len();
                self.be.abandon_window(
                    w,
                    "conveyor durability lane panicked before this window was reached",
                );
            }
            if !self.be.durability_lane.unlead_and_recheck() {
                break;
            }
        }
        if !self.be.failed.swap(true, Ordering::AcqRel) {
            log::error!(
                "meta volume {}: durability lane panicked with applied windows in flight — \
                 volume marked FAILED (RAM may diverge from replay; mutations return EIO \
                 until remount)",
                self.be.path.display()
            );
        }
        self.be.note_journal_failure();
        log::error!(
            "meta volume {}: durability lane panic contained — {members} member(s) in \
             progress and {stranded} queued failed EIO, reservations abandoned \
             (meta_conveyor_pass_panics)",
            self.be.path.display()
        );
    }
}

/// The posture an `open_inner` runs its bootstrap replay under — set by
/// the DOOR, not read from a latch that is only set after the replay
/// (review round 3, Issue 18: `open_read_only` / `open_co_writer` /
/// `open_probe` / the peer-owned open flip `read_only` on the returned
/// value, so the replay itself did not know it was a non-writer's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenPosture {
    /// The write mount (and the guarded offline verbs that hold its
    /// flock): replay may mint the slot trees the window names —
    /// recovery-class extents.
    Writer,
    /// A mount that may not write: replay mints nothing; records of a
    /// slot tree 0 does not name yet are skipped (S5 bounded staleness —
    /// served at the poll after the writer publishes).
    NonWriter,
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
    /// The forest arm of the mount path (design-symmetric-metadata §5.2,
    /// §5.3.4 — one ring, N slot trees): open tree 0 and the native slot
    /// tree from the ledger, replay tree 0's own records FIRST (its
    /// `slot_state` records are the routing every guest slot tree is
    /// opened by), open every guest slot tree tree 0 names, then run the
    /// two-phase replay of the window's slot-tree records — interior
    /// records routed by their separator key's slot, content records by
    /// their forest key, a slot tree minted on first touch (a slot whose
    /// tree was minted after the last checkpoint has no root yet — its
    /// records fold into a fresh root by key, the bit-9/16 discipline).
    /// Returns the forest plus the §4.8 replayed-ino maxima.
    #[allow(clippy::too_many_arguments)]
    async fn open_forest_and_replay(
        path: &Path,
        sb: &SuperblockV3,
        ledger: &LedgerRecord,
        cache: &Arc<NodeCache>,
        seq: &Arc<AtomicU64>,
        alloc: &Arc<ExtentAllocator>,
        recovery: &super::journal::JournalRecovery,
        posture: OpenPosture,
    ) -> std::result::Result<(TreeSet, u64, std::collections::HashMap<u16, u64>), KvError> {
        use super::record::{
            split_forest_key, ForestSlot, KIND_INTERIOR, NATIVE_FOREST_SLOT, TREE_CONTROL,
        };
        use super::slot_state::{decode_slot_state_key, slot_state_key_range, SlotState};

        let root_of = |tree_id: u8| -> std::result::Result<RootPtr, KvError> {
            ledger
                .tree_roots
                .iter()
                .find(|r| r.tree_id == tree_id)
                .map(|r| RootPtr {
                    addr: r.node_addr,
                    seq: r.node_seq,
                })
                .ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "{}: forest volume's ledger record seq {} names no root for tree {tree_id} \
                         (tree 0 = {TREE_CONTROL}, the native slot tree = {KIND_INTERIOR})",
                        path.display(),
                        ledger.seq
                    ))
                })
        };
        let control_root = root_of(TREE_CONTROL)?;
        let native_root = root_of(KIND_INTERIOR)?;
        let control = Arc::new(
            KvTree::open(
                Arc::clone(cache),
                TREE_CONTROL,
                control_root,
                Arc::clone(seq),
            )
            .await?,
        );
        seq.fetch_max(control_root.seq, Ordering::AcqRel);
        let native = Arc::new(
            KvTree::open_slot_tree(
                Arc::clone(cache),
                NATIVE_FOREST_SLOT,
                native_root,
                Arc::clone(seq),
            )
            .await?,
        );
        seq.fetch_max(native_root.seq, Ordering::AcqRel);

        // ---- Tree 0 first: its window records (level DESC, seq) — the
        // guest roots the rest of the replay is routed through.
        let mut control_interior: Vec<(u8, u64, &Record)> = Vec::new();
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if tree_id == TREE_CONTROL && level > 0 {
                    control_interior.push((level, entry.seq, rec));
                }
            }
        }
        control_interior.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.seq.cmp(&b.2.seq)));
        for (level, entry_start, rec) in control_interior {
            if rec.kind == RecordKind::Put {
                if let Ok((_addr, child_seq)) = decode_interior_value(&rec.value) {
                    seq.fetch_max(child_seq, Ordering::AcqRel);
                }
            }
            control
                .apply_replayed_interior(
                    &rec.key,
                    level,
                    rec.seq,
                    rec.kind,
                    Bytes::copy_from_slice(&rec.value),
                    entry_start,
                )
                .await?;
        }
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if tree_id != TREE_CONTROL || level > 0 {
                    continue;
                }
                control
                    .apply_replayed(
                        &rec.key,
                        rec.seq,
                        rec.kind,
                        Bytes::copy_from_slice(&rec.value),
                        entry.seq,
                    )
                    .await?;
            }
        }

        // ---- Every guest slot tree tree 0 names. A leased slot's root is
        // read off its lessee's page (the directory, one read).
        let directory = super::appender::read_directory(path, sb).await?;
        let native_routing_slot = ledger
            .membership_stamp
            .as_ref()
            .and_then(|s| s.native_slot)
            .unwrap_or(0);
        let mut guests: Vec<(ForestSlot, Arc<KvTree>)> = Vec::new();
        let (mut cursor, end) = slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                let slot = decode_slot_state_key(k)?;
                let root = match SlotState::decode(v)? {
                    SlotState::Unleased { root, .. } => root,
                    SlotState::Leased {
                        appender_id, root, ..
                    } => {
                        // A LEASED slot's live root is the lessee's page
                        // entry (§5.2.2); the record's grant-time root is
                        // the floor a page not yet written leaves.
                        Self::leased_root_from_directory(
                            &directory,
                            native_routing_slot,
                            appender_id,
                            slot,
                            root,
                        )
                    }
                };
                if slot == NATIVE_FOREST_SLOT {
                    // The native slot's record is the LEASE plane's
                    // (KD-SYM-2 — the manager leases it; the clean leave
                    // releases it); its root is the ledger's, never
                    // resolved from tree 0.
                    continue;
                }
                if root.addr == 0 {
                    continue; // granted, never minted: no tree to open yet
                }
                let tree =
                    KvTree::open_slot_tree(Arc::clone(cache), slot, root, Arc::clone(seq)).await?;
                seq.fetch_max(root.seq, Ordering::AcqRel);
                guests.push((slot, Arc::new(tree)));
            }
            if page.len() < 512 {
                break;
            }
        }
        let forest = super::forest::SlotTrees::new(control, native, guests);
        // A tree minted AT REPLAY holds records from the window's start:
        // its root floor is the window's tail (replay begins there), so the
        // first checkpoint cannot pass it before tree 0 names the root. The
        // writer mints in the RECOVERY class (the reserve is fair game —
        // a heap-full volume must mount); a non-writer never mints: the
        // records of a slot tree 0 does not name are SKIPPED here and
        // served after the writer publishes (S5 bounded staleness), never
        // routed into a RAM-only tree the epoch drop pass would tear from
        // under the reader.
        let replay_mint = super::forest::MintContext {
            cache,
            seq,
            alloc,
            floor: ledger.journal_tail_seq,
            policy: match posture {
                OpenPosture::Writer => super::forest::MintPolicy::Recovery,
                OpenPosture::NonWriter => super::forest::MintPolicy::Refuse,
            },
            // A recovery mint draws the bitmap, never a grant: the region
            // set is not yet open here, and a recovered root extent the
            // manager owns is an image like any other.
            region: None,
        };
        let mut skipped_slots: std::collections::BTreeMap<ForestSlot, u64> =
            std::collections::BTreeMap::new();
        // The routing of a window record on this posture: its slot tree,
        // minted if the writer's; `None` = a non-writer's record of an
        // unpublished slot, skipped and counted.
        let route = |slot: ForestSlot,
                     skipped: &mut std::collections::BTreeMap<ForestSlot, u64>|
         -> Option<ForestSlot> {
            if posture == OpenPosture::NonWriter && forest.tree(slot).is_none() {
                *skipped.entry(slot).or_insert(0) += 1;
                super::META_KV_FOREST_READER_WINDOW_SKIPS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(slot)
        };
        // The writer's replay re-mints every slot tree the window holds
        // records for and tree 0 does not name — one recovery-class extent
        // EACH, while the originals sit orphaned in the durable bitmap
        // until the hygiene sweep (design-symmetric-metadata §5.3.4; note
        // §7). A reserve that cannot cover them refuses the MOUNT, so the
        // refusal names the count: what the window needs, what it got,
        // what the heap holds.
        let unpublished: std::collections::BTreeSet<ForestSlot> = recovery
            .entries
            .iter()
            .flat_map(|e| e.records.iter())
            .filter_map(|(tag, rec)| {
                let (tree_id, level) = untag(*tag);
                let slot = if tree_id == KIND_INTERIOR && level > 0 {
                    super::forest::split_interior_journal_key(&rec.key)
                        .ok()
                        .map(|(slot, _)| slot)
                } else if level == 0 && super::record::is_slot_tree_kind(tree_id) {
                    super::record::forest_key_slot(&rec.key).ok()
                } else {
                    None
                };
                slot.filter(|s| forest.tree(*s).is_none())
            })
            .collect();
        // The space class stays the space class (ENOSPC through the crate
        // error, never `Corrupt` — whose "corrupt KV encoding" text once
        // sent a field hunt after phantom device corruption).
        let mint_refused = |e: KvError| -> KvError {
            match e {
                KvError::NoSpace { free, reserve } if posture == OpenPosture::Writer => {
                    KvError::Io(crate::error::SqueezefsError::no_space(format!(
                        "{}: replaying the journal window needs one root extent for EACH of \
                         the {} slot tree(s) tree 0 does not name yet (slots {:?}; their \
                         originals are orphaned in the bitmap until the hygiene sweep), and \
                         the heap cannot cover them — {free} free extent(s) against the \
                         {reserve}-extent compaction reserve. `squeezefs fsck` reclaims the \
                         orphans; design-symmetric-metadata §5.3.4",
                        path.display(),
                        unpublished.len(),
                        unpublished.iter().take(8).collect::<Vec<_>>()
                    )))
                }
                other => other,
            }
        };

        // ---- The slot trees' window: phase 1 interior records by
        // (level DESC, seq), each routed by its separator's slot.
        let mut interior: Vec<(u8, u64, &Record)> = Vec::new();
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if tree_id == KIND_INTERIOR && level > 0 {
                    interior.push((level, entry.seq, rec));
                }
            }
        }
        interior.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.seq.cmp(&b.2.seq)));
        for (level, entry_start, rec) in interior {
            if rec.kind == RecordKind::Put {
                if let Ok((_addr, child_seq)) = decode_interior_value(&rec.value) {
                    seq.fetch_max(child_seq, Ordering::AcqRel);
                }
            }
            // A slot tree's interior record names its slot on the journal
            // key (the separator alone cannot — the top one is
            // `KEY_SPACE_MAX` in every tree); strip it for the apply.
            let (slot, separator) = super::forest::split_interior_journal_key(&rec.key)?;
            let Some(slot) = route(slot, &mut skipped_slots) else {
                continue;
            };
            let tree = forest
                .slot_or_mint(slot, &replay_mint)
                .await
                .map_err(mint_refused)?;
            tree.apply_replayed_interior(
                separator,
                level,
                rec.seq,
                rec.kind,
                Bytes::copy_from_slice(&rec.value),
                entry_start,
            )
            .await?;
        }

        // ---- Phase 2: content records by forest key, plus the §4.8
        // recovery fold (DUR-8c — every ino the window MENTIONS).
        let mut max_replayed_ino: u64 = 0;
        let mut max_replayed_guest: std::collections::HashMap<u16, u64> =
            std::collections::HashMap::new();
        for entry in &recovery.entries {
            for (tag, rec) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if level > 0 || !super::record::is_slot_tree_kind(tree_id) {
                    continue; // phase 1 applied it / tree 0 / allocator records
                }
                let (kind, legacy) = split_forest_key(&rec.key).map_err(|e| {
                    super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                    e
                })?;
                if kind != tree_id {
                    super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                    return Err(KvError::Corrupt(format!(
                        "{}: journaled record tagged kind {tree_id} carries a key of kind {kind}",
                        path.display()
                    )));
                }
                let mentioned: Option<u64> = match kind {
                    TREE_INODES => decode_inode_key(&legacy).ok(),
                    TREE_DENTRIES => super::record::DentryValue::decode(&rec.value)
                        .ok()
                        .map(|d| d.child_ino),
                    TREE_XATTRS => super::record::decode_xattr_key(&legacy)
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
                let slot = super::record::forest_key_slot(&rec.key).map_err(|e| {
                    super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                    e
                })?;
                if route(slot, &mut skipped_slots).is_none() {
                    continue;
                }
                let (_slot, tree) = forest
                    .route_forest_key_or_mint(&rec.key, &replay_mint)
                    .await
                    .map_err(mint_refused)?;
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

        if !skipped_slots.is_empty() {
            let records: u64 = skipped_slots.values().sum();
            log::info!(
                "meta volume {}: non-writer open skipped {records} window record(s) of {} slot \
                 tree(s) tree 0 does not name yet (slots {:?}) — served after the writer's next \
                 publication, at this mount's next poll (S5 bounded staleness)",
                path.display(),
                skipped_slots.len(),
                skipped_slots.keys().take(8).collect::<Vec<_>>()
            );
        }

        let block_refs =
            sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS != 0
                && sb.unknown_ro() == 0;
        let block_map = AtomicBool::new(sb.block_map_tree_stamped() && sb.unknown_ro() == 0);
        Ok((
            TreeSet::Forest {
                forest,
                block_refs,
                block_map,
            },
            max_replayed_ino,
            max_replayed_guest,
        ))
    }

    /// This process's appender identity scope `(node_token, mount_slot)`
    /// (KD-MW-2): the engaged writer scope when a mount armed one, else
    /// the node token + this process's mount slot; a box without any
    /// node-identity material falls back to a boot-scoped token so two
    /// mounts of one boot still match their own residue.
    fn appender_identity_scope(boot_id: &str) -> (u64, u32) {
        if let Some(scope) = crate::writer_scope::engaged_scope() {
            return (scope.node, scope.slot);
        }
        let node = crate::writer_scope::resolve_node_identity()
            .map(|n| n.token)
            .unwrap_or_else(|_| {
                xxhash_rust::xxh3::xxh3_64(format!("sqz-appender\u{0}{boot_id}").as_bytes())
            });
        (node, crate::writer_scope::mount_slot())
    }

    /// **The appender regions of a forest volume at open** (design-
    /// symmetric-metadata §5.3.2 identity binding, §5.3.4, §5.9 — PR 2):
    /// read the directory, bind this node's identity, and — on a WRITER
    /// open — recover its own residue and stand up every region the
    /// declared partition names.
    ///
    /// Region 0 (the manager's, KD-SYM-3) rides the fixed ring the caller
    /// already replayed from the ledger; a `Live` page of our own NODE
    /// means the predecessor died un-recovered and that replay WAS its
    /// recovery (`appender_self_recoveries`) — "own" is the node token,
    /// because the writer holds the D0 flock here and a same-host holder
    /// at ANY mount point is therefore dead (`AppenderIdentity::
    /// owned_by_node`; the kill-9 successor remounting at another mount
    /// point is the shipped shape). A `Live` page of a FOREIGN node
    /// refuses a writer open loud — recovering another node's ring is
    /// PR 10's driver, `squeezefs appender clear` the remedy — and a
    /// non-writer (reader / probe) lists it and mounts what tree 0 names.
    /// A declared region ≥ 1 with an own `Live` page has its ring
    /// replayed from the page's segments and tail (content only — the
    /// manager owns every ring's structure in PR 2); `Free` / `Recovered`
    /// pages get a fresh ring from the heap (`SQUEEZEFS_SYM_RING_KB` or
    /// the derivation), never replayed. The three per-ring violation
    /// classes are evaluated over EVERY ring's window and refuse loud.
    #[allow(clippy::too_many_arguments)]
    async fn open_appender_regions(
        path: &Path,
        sb: &SuperblockV3,
        ledger: &LedgerRecord,
        ring0: &Arc<JournalRing>,
        forest: &super::forest::SlotTrees,
        cache: &Arc<NodeCache>,
        seq: &Arc<AtomicU64>,
        alloc: &Arc<ExtentAllocator>,
        recovery0: &super::journal::JournalRecovery,
        posture: OpenPosture,
        boot_id: &str,
    ) -> std::result::Result<super::appender::AppenderSet, KvError> {
        use super::appender::{
            appender0_page_offsets, declared_partition, dir_pairs_per_extent,
            first_segment_ring_part, page_slot_offsets, read_directory, resolve_sym_ring_bytes,
            AppenderIdentity, AppenderPage, AppenderRegion, AppenderSet, AppenderState,
        };
        use super::journal::RingSegment;

        let entries = read_directory(path, sb).await?;
        let scope = Self::appender_identity_scope(boot_id);
        let native_slot = ledger
            .membership_stamp
            .as_ref()
            .and_then(|s| s.native_slot)
            .unwrap_or(0);
        let live_pages_at_mount = entries
            .iter()
            .filter(|e| {
                e.page
                    .as_ref()
                    .is_some_and(|p| p.state == AppenderState::Live)
            })
            .count() as u64;
        let is_writer = posture == OpenPosture::Writer;
        let partition = if is_writer {
            declared_partition()?
        } else {
            Default::default()
        };
        // The armed plane (PR 4): the seam's declared slots are a WISH-LIST
        // the arm reconciles against tree 0 — a region's lease set starts
        // empty and fills from the real acquire path.
        let armed = is_writer && super::slot_lease::symmetric_meta_requested();
        // The volume length the ring derivation clamps against: the heap's
        // end (the superblock stores no volume length; the redundant
        // superblock copy sits in the one sector past it).
        let volume_len = sb.heap.end();
        let ring_bytes = resolve_sym_ring_bytes(0, volume_len);
        let capacity = super::appender::appenders_capacity(sb.heap.len, ring_bytes);

        // A writer reaching this point holds the D0 flock (step (1) of
        // `open`): a same-NODE Live page is a dead predecessor's residue
        // whatever mount slot it carried — see `owned_by_node`.
        let mine = |p: &AppenderPage| p.identity.owned_by_node(scope.0);
        // A page no PR-2 writer may join over: a FOREIGN node's `Live`
        // page (recovering another node's ring is PR 10's driver), or a
        // `Recovering` page of any identity (a recoverer mid-replay,
        // §5.9 — the design's joiner PARKS; nothing writes the state in
        // PR 2, so this is the tripwire a PR-10 recoverer would trip).
        // The refusal is DEFERRED to the JOIN (step 7 of `open`, after
        // the D0 claim gate has classified the volume's holder): a LIVE
        // foreign writer on a shared non-PR LUN then gets the gate's own
        // "another process holds the writer claim" refusal, and the
        // non-joining Writer opens — `claim clear`, the guarded offline
        // verbs — are not blocked by a page they never write.
        // PR 3 narrows the refusal to what a manager may not mount OVER:
        // a `Recovering` page of any identity, and a foreign `Live` page
        // whose id the DECLARED partition claims (the seam would steal a
        // joined appender's page). A foreign `Live` page 0 is a manager
        // the D0 ladder took the claim from — dead by D0's proof, its ring
        // the fixed ring this open replayed — and the successor adopts it
        // with the role (§5.9); every OTHER foreign `Live` page is a
        // JOINED appender (`JoinAppender`, §5.3.5) — the directory's
        // normal state on a multi-appender volume, listed on
        // `appender_live_pages_at_mount`, recovered by PR 10's driver when
        // its death is proven, never a reason the manager cannot remount.
        let join_refusal = if is_writer {
            entries
                .iter()
                .find_map(|e| {
                    e.page.as_ref().filter(|p| {
                        (p.state == AppenderState::Live
                            && !mine(p)
                            && partition.contains_key(&p.appender_id))
                            || p.state == AppenderState::Recovering
                    })
                })
                .map(|p| {
                    format!(
                        "{}: appender page {} is {} under {} identity (node {:#018x}, mount slot \
                         {:#x}, term {}) — recovering another node's ring is the dead-appender \
                         recovery driver (design-symmetric-metadata PR 10, not yet available; \
                         it owns the operator remedy). Refusing to join over it",
                        path.display(),
                        p.appender_id,
                        p.state.as_str(),
                        if mine(p) { "our own" } else { "a foreign" },
                        p.identity.node_token,
                        p.identity.mount_slot,
                        p.term,
                    )
                })
        } else {
            None
        };
        // A blocked writer stands up region 0 only: it claims no ring and
        // writes no page before the join refuses.
        let partition = if join_refusal.is_some() {
            Default::default()
        } else {
            partition
        };

        // The manager-lease posture off page 0 as found: `Held` is the
        // JOIN's to declare; a foreign `Live` page 0 is a peer's; a Free
        // (or blank) page 0 is vacant. A same-node Live page 0 is our
        // dead predecessor's — vacant until we join.
        let manager_lease = match entries.first().and_then(|e| e.page.as_ref()) {
            Some(p) if p.state == AppenderState::Live && !mine(p) => {
                super::appender::ManagerLease::Peer {
                    node_token: p.identity.node_token,
                }
            }
            _ => super::appender::ManagerLease::Vacant,
        };
        // Ring 0's seq offset (§5.8.2): appender 0's page raises what the
        // fixed ring's window attested at step 4a (`max`, never lower).
        if let Some(p) = entries.first().and_then(|e| e.page.as_ref()) {
            ring0.recover_seq_offset(p.seq_offset);
        }
        let set = AppenderSet {
            regions: Vec::new(),
            identity: AppenderIdentity {
                node_token: scope.0,
                mount_slot: scope.1,
                writer_id: 0,
            },
            native_slot,
            capacity,
            live_pages_at_mount,
            joins: AtomicU64::new(0),
            leaves: AtomicU64::new(0),
            self_recoveries: AtomicU64::new(0),
            flush_ceiling_overruns: AtomicU64::new(0),
            pressure_cycles: AtomicU64::new(0),
            joined: AtomicBool::new(false),
            join_refusal,
            flush_ceiling_ms: super::appender::appender_flush_ceiling_ms(
                crate::meta_backend::resolve_flush_interval_ms(),
            ),
            // The ladder's and the replay's measured terms land at the
            // JOIN (`join_appender_regions`), where the open's wall is
            // known; until then the bound is the TTL alone.
            failover_bound_ms: AtomicU64::new(super::appender::manager_failover_bound_ms(
                crate::fuse_client::CLIENT_STALE_TTL_SECS,
                0,
                0,
            )),
            manager_lease: std::sync::Mutex::new(manager_lease),
            wero_meta: AtomicBool::new(false),
            extent_grants: AtomicU64::new(0),
            extent_grant_extents: AtomicU64::new(0),
            extent_returns: AtomicU64::new(0),
            vol0_unreachable: AtomicU64::new(0),
            verbs: Default::default(),
            cadence_last_ns: AtomicU64::new(0),
            appenders_known: AtomicU64::new(0),
            leases: None,
        };
        let mut regions: Vec<Arc<AppenderRegion>> = Vec::new();

        // ---- Region 0: the fixed ring, already replayed by the caller.
        let page0 = entries
            .first()
            .and_then(|e| e.page.clone())
            .unwrap_or_else(|| {
                // A stamped volume whose page 0 never verified (every copy
                // torn): rebuild the Free page the format wrote — the ring
                // is the fixed extent by construction.
                let mut p = AppenderPage::free(0, 0);
                p.segments = vec![super::appender::appender0_ring_extent(&sb.journal)];
                p
            });
        let self_recovered0 = is_writer && page0.state == AppenderState::Live && mine(&page0);
        if self_recovered0 {
            set.self_recoveries.fetch_add(1, Ordering::Relaxed);
            log::info!(
                "meta volume {}: appender 0's page is LIVE under our own node (mount slot \
                 {:#x}, term {}) — our predecessor died un-recovered (the D0 flock we hold is \
                 the proof); its ring window ({} entries) was replayed as our own residue \
                 (appender_self_recoveries)",
                path.display(),
                page0.identity.mount_slot,
                page0.term,
                recovery0.entries.len()
            );
        }
        let dir_named0 = entries.first().map_or(0, |e| {
            super::appender::dir_named_mask(&e.dir_pages, &page0.segments)
        });
        regions.push(Arc::new(AppenderRegion {
            id: 0,
            page_offsets: appender0_page_offsets(&sb.journal),
            page: std::sync::Mutex::new(page0),
            ring: arc_swap::ArcSwap::from(Arc::clone(ring0)),
            leases: arc_swap::ArcSwap::from_pointee(Default::default()),
            leases_writer: std::sync::Mutex::new(()),
            last_tail: AtomicU64::new(ledger.journal_tail_seq),
            durable_tail: AtomicU64::new(ledger.journal_tail_seq),
            stalls: AtomicU64::new(0),
            stalls_at_last_grow: AtomicU64::new(0),
            ring_grows: AtomicU64::new(0),
            pending_reclaim: std::sync::Mutex::new(Vec::new()),
            passes_inside: std::sync::atomic::AtomicUsize::new(0),
            windows_inflight: std::sync::atomic::AtomicUsize::new(0),
            growing: AtomicBool::new(false),
            growth_done: squeezefs_ipc::sqz_notify::Notify::new(),
            dir_named: std::sync::atomic::AtomicU8::new(dir_named0),
            self_recovered: self_recovered0,
            grant: Default::default(),
            smo_ewma_milli: AtomicU64::new(0),
            smos_this_cycle: AtomicU64::new(0),
            dependency_stalls: AtomicU64::new(0),
            released: AtomicBool::new(false),
        }));

        // ---- Declared regions (the PR-2 seam standing in for PR 4's
        // lease gate): each needs a page in the directory and a ring.
        let mut rings: Vec<(u32, &super::journal::JournalRecovery)> = vec![(0, recovery0)];
        let mut recovered: Vec<(u32, super::journal::JournalRecovery)> = Vec::new();
        let node_size = u64::from(sb.node_size);
        for (&id, slots) in &partition {
            let entry = entries
                .iter()
                .find(|e| e.appender_id == id)
                .ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "{}: declared appender {id} has no page in the directory (the chain holds \
                     {} ids per extent at this node size; growing the chain is the manager's \
                     JoinAppender — PR 3)",
                        path.display(),
                        dir_pairs_per_extent(node_size)
                    ))
                })?;
            let mut page = entry
                .page
                .clone()
                .unwrap_or_else(|| AppenderPage::free(id, 0));
            let own_live = page.state == AppenderState::Live && mine(&page);
            let reserve = checkpoint_reserve_bytes(ring_bytes);
            let (ring, self_recovered) = if own_live && !page.segments.is_empty() {
                // Own residue: replay THIS ring from ITS page's tail.
                let mut segs: Vec<RingSegment> = Vec::with_capacity(page.segments.len());
                for (i, ext) in page.segments.iter().enumerate() {
                    let ext = if i == 0 {
                        first_segment_ring_part(ext)
                    } else {
                        *ext
                    };
                    segs.push(RingSegment::from_extent(&ext));
                }
                let (ring, rec) = JournalRing::recover_segments(
                    path,
                    segs,
                    checkpoint_reserve_bytes(page.ring_bytes()),
                    page.ledger_tail_seq,
                )
                .await?;
                // The seq-space law (§5.8.2): the page's offset, raised by
                // what the window's own stamps attest.
                ring.recover_seq_offset(
                    page.seq_offset
                        .max(super::journal::seq_offset_of_window(&rec.entries)),
                );
                log::info!(
                    "meta volume {}: appender {id}'s page is LIVE under our own node (mount slot \
                     {:#x}, term {}) — replaying its ring ({} entries past tail {}) as our own \
                     residue",
                    path.display(),
                    page.identity.mount_slot,
                    page.term,
                    rec.entries.len(),
                    page.ledger_tail_seq
                );
                set.self_recoveries.fetch_add(1, Ordering::Relaxed);
                recovered.push((id, rec));
                (ring, true)
            } else {
                // A fresh ring from the heap (`Free`, or `Recovered` — never
                // replayed, §5.8.3): whole extents claimed internal-class,
                // adjacent ones coalesced into segments; the bits land in
                // the join's checkpoint before the page names them.
                let want_extents = ring_bytes.div_ceil(node_size).max(1);
                let mut claimed: Vec<u64> = Vec::with_capacity(want_extents as usize);
                for _ in 0..want_extents {
                    match alloc.claim_internal() {
                        Ok(e) => claimed.push(e),
                        Err(err) => {
                            for e in claimed {
                                alloc.release_unpublished(e);
                            }
                            return Err(err);
                        }
                    }
                }
                claimed.sort_unstable();
                let mut extents: Vec<super::superblock::ExtentRef> = Vec::new();
                for e in claimed.iter() {
                    let start = sb.heap.start + e * node_size;
                    match extents.last_mut() {
                        Some(last) if last.end() == start => last.len += node_size,
                        _ => extents.push(super::superblock::ExtentRef {
                            start,
                            len: node_size,
                        }),
                    }
                }
                if extents.len() > super::appender::RING_SEGMENTS_MAX {
                    for e in claimed {
                        alloc.release_unpublished(e);
                    }
                    return Err(KvError::Corrupt(format!(
                        "{}: appender {id}'s {ring_bytes}-byte ring would take {} segments of \
                         this heap's free extents (the page names at most {}) — lower \
                         {} or raise --meta-node-kib",
                        path.display(),
                        extents.len(),
                        super::appender::RING_SEGMENTS_MAX,
                        super::appender::SYM_RING_KB_ENV
                    )));
                }
                // A predecessor incarnation's ring may have occupied these
                // very extents: zeroed before the page names them, or its
                // entries replay as this ring's (`zero_extents`).
                if let Err(e) = super::appender::zero_extents(path, &extents).await {
                    for e in claimed {
                        alloc.release_unpublished(e);
                    }
                    return Err(e);
                }
                page.segments = extents;
                // The ring's position space CONTINUES: past the region's
                // own watermark (the `Free` page's head — its predecessor
                // incarnation's final head) and past ring 0's head, so
                // every record this incarnation writes carries a seq above
                // every durable record of the slots it leases (per-key
                // LWW is by raw seq; a ring restarting at 0 replayed an
                // acked release as resurrected). The cross-ring case — a
                // lease moving between rings — is PR 4's globally
                // monotone seq space.
                let start = page.head_hint.max(ring0.core().head());
                page.head_hint = start;
                page.ledger_tail_seq = start;
                // The predecessor incarnation's seq offset carries over
                // with its position space (§5.8.2): the new ring stamps
                // above every record the predecessor ever stamped.
                let seq_offset = page.seq_offset;
                let segs: Vec<RingSegment> = page
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(i, ext)| {
                        RingSegment::from_extent(&if i == 0 {
                            first_segment_ring_part(ext)
                        } else {
                            *ext
                        })
                    })
                    .collect();
                let fresh = JournalRing::new_segments_at(path, segs, reserve, start);
                fresh.recover_seq_offset(seq_offset);
                (fresh, false)
            };
            let first = page.segments.first().copied().unwrap_or(sb.journal);
            let page_offsets = page_slot_offsets(sb, id, entry.dir_offsets, &first);
            // Which directory images name the ring this mount USES: for
            // a recovered ring the pair's images naming its table; for a
            // fresh ring none — the join's page write fills both slots.
            let dir_named = super::appender::dir_named_mask(&entry.dir_pages, &page.segments);
            // The region's grant (§5.3.3), recovered from tree 0 at EVERY
            // open: the record is the whole of it, the page names the
            // unclaimed remainder — a `Free` page (the clean leave
            // returned the remainder) names none, so everything the record
            // still names is a live image and lands CLAIMED, which is the
            // truth. Before review round 1's Issue 1 a non-recovered open
            // built an EMPTY grant against a record naming the region's
            // live images, and every later retirement of a pre-remount
            // image was dropped by `free_pending`'s claimed guard: never
            // parked, never returned, invisible to C13 and to the closure
            // gauge — the silent leak on the ordinary clean lifecycle. The
            // ring window's alloc/free records refine both below.
            if !self_recovered {
                page.grant.clear();
            }
            let record = match forest
                .control()
                .lookup(&super::slot_state::extent_grant_key(id))
                .await?
            {
                Some(v) => super::slot_state::ExtentGrantRecord::decode(&v)?,
                None => Default::default(),
            };
            let grant = super::appender::RegionGrant::recover(record.extents(), &page.grant);
            regions.push(Arc::new(AppenderRegion {
                id,
                page_offsets,
                last_tail: AtomicU64::new(page.ledger_tail_seq),
                durable_tail: AtomicU64::new(page.ledger_tail_seq),
                page: std::sync::Mutex::new(page),
                ring: arc_swap::ArcSwap::from(Arc::new(ring)),
                leases: arc_swap::ArcSwap::from_pointee(if armed {
                    Default::default()
                } else {
                    slots.clone()
                }),
                leases_writer: std::sync::Mutex::new(()),
                stalls: AtomicU64::new(0),
                stalls_at_last_grow: AtomicU64::new(0),
                ring_grows: AtomicU64::new(0),
                pending_reclaim: std::sync::Mutex::new(Vec::new()),
                passes_inside: std::sync::atomic::AtomicUsize::new(0),
                windows_inflight: std::sync::atomic::AtomicUsize::new(0),
                growing: AtomicBool::new(false),
                growth_done: squeezefs_ipc::sqz_notify::Notify::new(),
                dir_named: std::sync::atomic::AtomicU8::new(dir_named),
                self_recovered,
                grant: Arc::new(std::sync::Mutex::new(grant)),
                smo_ewma_milli: AtomicU64::new(0),
                smos_this_cycle: AtomicU64::new(0),
                dependency_stalls: AtomicU64::new(0),
                released: AtomicBool::new(false),
            }));
        }
        // The slot-lease plane (PR 4): a writer that asked for it. `M`
        // derives from the census at join — the directory's Live pages
        // (foreign appenders) plus this mount.
        let leases = armed.then(|| {
            let writers_known = live_pages_at_mount.max(1);
            let m = super::slot_lease::resolve_mint_slots(
                u64::from(crate::meta_backend::DERIVED_ROUTING_WIDTH),
                writers_known,
            );
            let plane = super::slot_lease::SlotLeasePlane::new(
                cache.lease_gate(),
                Arc::clone(forest.extent_ledger()),
                m,
                partition.clone(),
                set.native_slot,
                u128::from_le_bytes(sb.uuid),
            );
            // The pages' slot entries as loaded — the arm's settle reads
            // them after the join's checkpoint has rewritten the pages.
            {
                let mut loaded = plane
                    .loaded_page_entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for r in &regions {
                    let page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                    if page.state == super::appender::AppenderState::Live {
                        loaded.insert(r.id, page.slots.clone());
                    }
                }
            }
            // Every Live page's identity (the membership carriage's
            // member → appender resolution); this mount's regions carry
            // its own.
            for e in &entries {
                if let Some(p) = e.page.as_ref() {
                    if p.state == super::appender::AppenderState::Live {
                        plane.note_identity(
                            e.appender_id,
                            p.identity.node_token,
                            p.identity.mount_slot,
                        );
                    }
                }
            }
            for r in &regions {
                plane.note_identity(r.id, set.identity.node_token, set.identity.mount_slot);
            }
            Arc::new(plane)
        });
        let set = AppenderSet {
            regions,
            leases,
            ..set
        };

        // ---- The per-ring violation classes over EVERY window (§5.3.4),
        // BEFORE any recovered content is applied: loud, counted, refused.
        for (id, rec) in &recovered {
            rings.push((*id, rec));
        }
        // The lease map the detector reads: the seam's declared partition
        // on an unarmed forest; under the armed plane tree 0's `Leased`
        // records — a slot that moved rings by a handover is judged by
        // its CURRENT lessee (the departing holder's window was flushed
        // clear of it, KD-SYM-4).
        let leases = if set.leases.is_some() {
            Self::read_tree0_lease_map(forest.control()).await?
        } else {
            set.lease_map()
        };
        let grants = Self::read_extent_grant_records(forest.control()).await?;
        let granted = |appender: u32, extent: u64| -> bool {
            grants
                .iter()
                .any(|(id, rec)| *id == appender && rec.contains(extent))
        };
        let owned: Vec<(u32, super::journal::JournalRecovery)> = rings
            .iter()
            .map(|(id, r)| {
                (
                    *id,
                    super::journal::JournalRecovery {
                        entries: r.entries.clone(),
                        head_pos: r.head_pos,
                        dropped_torn: r.dropped_torn,
                        foreign_pages: r.foreign_pages,
                    },
                )
            })
            .collect();
        let violations = super::journal::detect_appender_violations(&owned, &leases, &granted);
        if !violations.is_empty() {
            let (mut key, mut lease, mut extent) = (0u64, 0u64, 0u64);
            for v in &violations {
                match v {
                    super::journal::AppenderViolation::Key { .. } => key += 1,
                    super::journal::AppenderViolation::Lease { .. } => lease += 1,
                    super::journal::AppenderViolation::Extent { .. } => extent += 1,
                }
            }
            super::META_KV_REPLAY_KEY_VIOLATIONS.fetch_add(key, Ordering::Relaxed);
            super::META_KV_REPLAY_LEASE_VIOLATIONS.fetch_add(lease, Ordering::Relaxed);
            super::META_KV_REPLAY_EXTENT_VIOLATIONS.fetch_add(extent, Ordering::Relaxed);
            let shown: Vec<String> = violations.iter().take(4).map(|v| v.to_string()).collect();
            return Err(KvError::Corrupt(format!(
                "{}: appender partition violated by {} record(s) in the replay windows \
                 ({key} key, {lease} lease, {extent} extent) — no merge order is correct: {}{}",
                path.display(),
                violations.len(),
                shown.join("; "),
                if violations.len() > shown.len() {
                    format!(" (+{} more)", violations.len() - shown.len())
                } else {
                    String::new()
                }
            )));
        }

        // ---- The pages' root vectors (§5.2.2 — a leased slot's root
        // lives on its lessee's page, written every cycle AFTER tree 0's
        // publication; §5.2.4): for every own page, a slot entry whose
        // root is NEWER than the tree's (node seqs mint monotonically, so
        // a later root image carries a higher seq) is adopted, and a slot
        // tree 0 does not name yet is OPENED from the page. A declared
        // region's content rides ITS ring, whose tail the page advanced
        // past the flushed records — the page's root is that content's
        // only durable home.
        if is_writer {
            for r in &set.regions {
                let entries: Vec<super::appender::SlotEntry> = {
                    let page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                    if page.state != AppenderState::Live || !mine(&page) {
                        continue;
                    }
                    page.slots.clone()
                };
                for e in entries {
                    if e.root.addr == 0 {
                        continue;
                    }
                    let slot = super::appender::forest_slot_of_page_slot(e.slot, native_slot);
                    if slot == super::record::NATIVE_FOREST_SLOT {
                        continue; // the ledger's, mirrored
                    }
                    match forest.tree(slot) {
                        Some(t) => {
                            if e.root.seq > t.root().seq {
                                t.adopt_root(e.root)?;
                            }
                        }
                        None => {
                            let tree = KvTree::open_slot_tree(
                                Arc::clone(cache),
                                slot,
                                e.root,
                                Arc::clone(seq),
                            )
                            .await?;
                            seq.fetch_max(e.root.seq, Ordering::AcqRel);
                            forest.adopt_guest(slot, Arc::new(tree));
                        }
                    }
                }
            }
        }

        // ---- Apply the recovered content rings into the forest (phase 2
        // of §5.3.4 per ring — content by forest key; order-independent
        // across rings because they are key-disjoint in-window, which the
        // check above just proved).
        let mint = super::forest::MintContext {
            cache,
            seq,
            alloc,
            floor: ledger.journal_tail_seq,
            policy: super::forest::MintPolicy::Recovery,
            region: None,
        };
        for (id, rec) in &recovered {
            // Phase 1: the region's own SMO pointer records by (level
            // DESC, seq) — the lessee's interior flips journal in ITS
            // ring since PR 3 (§5.2.3) — and its allocator deltas, which
            // are GRANT-internal (the bitmap bits were set by the grant's
            // carve in ring 0): an in-window `alloc` is a claim the page
            // predates, a `free` a retirement parked on this ring's tail.
            let mut interior: Vec<(u8, u64, &Record)> = Vec::new();
            for entry in &rec.entries {
                for (tag, r) in &entry.records {
                    let (tree_id, level) = untag(*tag);
                    if tree_id == super::record::TREE_ALLOC_RESERVED {
                        if let Some(region) = set.region(*id) {
                            match super::alloc_ext::decode_alloc_record(r)? {
                                super::alloc_ext::AllocDelta::Allocated { extent } => {
                                    region.grant().claim_exact(extent);
                                }
                                super::alloc_ext::AllocDelta::Freed { extent, .. } => {
                                    region.grant().free_exact(extent, r.seq);
                                }
                            }
                        }
                    } else if tree_id == super::record::KIND_INTERIOR && level > 0 {
                        interior.push((level, entry.seq, r));
                    }
                }
            }
            interior.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.seq.cmp(&b.2.seq)));
            for (level, entry_start, r) in interior {
                if r.kind == RecordKind::Put {
                    if let Ok((_addr, child_seq)) = decode_interior_value(&r.value) {
                        seq.fetch_max(child_seq, Ordering::AcqRel);
                    }
                }
                let (slot, separator) = super::forest::split_interior_journal_key(&r.key)?;
                let tree = forest.slot_or_mint(slot, &mint).await?;
                tree.apply_replayed_interior(
                    separator,
                    level,
                    r.seq,
                    r.kind,
                    Bytes::copy_from_slice(&r.value),
                    entry_start,
                )
                .await?;
            }
            // Phase 2: content by forest key.
            for entry in &rec.entries {
                for (tag, r) in &entry.records {
                    let (tree_id, level) = untag(*tag);
                    if level > 0 || !super::record::is_slot_tree_kind(tree_id) {
                        continue;
                    }
                    let (_slot, tree) = forest.route_forest_key_or_mint(&r.key, &mint).await?;
                    tree.apply_replayed(
                        &r.key,
                        r.seq,
                        r.kind,
                        Bytes::copy_from_slice(&r.value),
                        entry.seq,
                    )
                    .await?;
                }
            }
        }
        // The elision tails are PER RING: a record's seq is a position in
        // ITS ring, so a leaf's tombstones are elidable only below its own
        // region's durable tail — the cache-wide word stays ring 0's (the
        // ledger's tail, as on a flat volume) and every leased slot's tail
        // is the lessee's (`durable_tail_for_slot`).
        cache.set_durable_tail(ledger.journal_tail_seq);
        Self::publish_slot_durable_tails(&set, cache);
        Ok(set)
    }

    /// The live root of LEASED slot `slot`: its lessee's page entry when
    /// the page names it (newest root seq wins), else the grant-time root
    /// tree 0 recorded (`recorded`).
    fn leased_root_from_directory(
        directory: &[super::appender::AppenderEntry],
        native_routing_slot: u16,
        appender_id: u32,
        slot: super::record::ForestSlot,
        recorded: RootPtr,
    ) -> RootPtr {
        let from_page = directory
            .iter()
            .find(|e| e.appender_id == appender_id)
            .and_then(|e| e.page.as_ref())
            .and_then(|p| {
                // Page entries name ROUTING slots, converted against the
                // volume's native slot.
                p.slots
                    .iter()
                    .find(|s| {
                        super::appender::forest_slot_of_page_slot(s.slot, native_routing_slot)
                            == slot
                            && s.root.addr != 0
                    })
                    .map(|s| s.root)
            });
        match from_page {
            Some(r) if r.seq >= recorded.seq => r,
            _ => recorded,
        }
    }

    /// The trees whose roots the FIXED LEDGER names: every tree on a flat
    /// volume; on a forest volume tree 0 and the native slot tree only —
    /// guest slot roots ride tree 0 (`slot_state` records,
    /// [`Self::publish_forest_roots`]), never the 4 KiB ledger slot.
    pub(super) fn ledger_root_trees(&self) -> Vec<Arc<KvTree>> {
        match &self.trees {
            TreeSet::Flat { .. } => self.all_trees(),
            TreeSet::Forest { forest, .. } => {
                vec![Arc::clone(forest.control()), Arc::clone(forest.native())]
            }
        }
    }

    /// **Tree-0 root publication** (design-symmetric-metadata §5.2.2 /
    /// §5.2.4): write a `slot_state` record for every guest slot tree
    /// whose live root moved since its last publication, as ONE
    /// checkpoint-class journal entry applied to tree 0 in RAM — BEFORE
    /// the checkpoint's flush pass, so the same cycle flushes tree 0's
    /// leaf and its ledger record's tree-0 root covers the publication.
    ///
    /// The covering argument, and its two halves: (1) until the record
    /// is applied, every guest root that moved is UNPUBLISHED and its
    /// `root_floor` (the mint's head / the swap's reservation) clamps the
    /// cycle's tail through [`super::forest::SlotTrees::
    /// unpublished_root_floors`] — a deferred publication (reserve
    /// exhausted) therefore leaves the records under those roots in the
    /// window, exactly like an SMO the flush pass skipped; (2) once
    /// applied, the record is a content record of tree 0 with the entry's
    /// own floor: replayed if un-flushed, covered by the ledger's tree-0
    /// root once flushed. The images the record names are made durable
    /// BEFORE the entry is written (a fresh mint's root is written by
    /// `create_slot_tree` with no barrier of its own; an SMO's root
    /// already is) — §4.10's "a replayed pointer must never route to a
    /// torn image", one barrier per cycle-with-moved-roots.
    ///
    /// A no-op on a flat volume and on a forest whose guest roots are all
    /// current. Reserve exhaustion is returned as
    /// [`KvError::JournalReserveExhausted`] for the caller's drain-and-
    /// retry; a failed entry write is the journal-failure class
    /// (`note_journal_failure`), and the roots stay unpublished.
    pub(super) async fn publish_forest_roots(&self) -> std::result::Result<(), KvError> {
        let Some(forest) = self.forest() else {
            return Ok(());
        };
        // Under the ARMED plane a LEASED slot's root rides its lessee's
        // page (KD-SYM-3 — the page is written every checkpoint and
        // `write_appender_pages` records the publication), never tree 0:
        // the `Leased` record keeps the grant-time words, and a
        // publication here would overwrite the lease itself. A slot
        // mid-handover (`releasing`) is the release record's.
        //
        // **The page-budget overflow law** (review round 2 — found by the
        // Issue 5 storm pin): a region holding MORE slots than its page
        // names (`SLOT_PAGE_BUDGET`; the LRU release brings it back only
        // when a slot goes idle) has roots the page CANNOT publish, and an
        // unpublished root is a floor — before this arm a burst of first-
        // touch acquires past the budget pinned the ledger tail for good
        // and the clause-b audit FAIL-STOPPED the volume after 8 barriered
        // cycles. Such a root rides tree 0 after all: its `Leased` record
        // is REWRITTEN with the current root (lessee, `g`, cursor, extent
        // count, seq floor kept — the lease itself is untouched), which
        // is exactly the record the next open's `leased_root_from_
        // directory` falls back to when the page names no entry. In-
        // process regions only (a wire appender's page is its own
        // mount's; its holdings are bounded at its join — PR 12).
        let plane = self.slot_leases();
        let overflow: std::collections::BTreeSet<super::record::ForestSlot> =
            match (plane, self.appenders.as_ref()) {
                (Some(_), Some(set)) => set
                    .regions
                    .iter()
                    .filter(|r| !r.released.load(Ordering::Acquire))
                    .flat_map(|r| Self::region_page_overflow(set, r))
                    .collect(),
                _ => Default::default(),
            };
        let pending: Vec<(super::record::ForestSlot, RootPtr)> = forest
            .roots_to_publish()
            .into_iter()
            .filter(|(slot, _)| {
                plane.is_none_or(|p| {
                    (!p.gate.is_leased(*slot) && !p.gate.is_releasing(*slot))
                        || (overflow.contains(slot) && !p.gate.is_releasing(*slot))
                })
            })
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let tag = super::journal::tag_for(super::record::TREE_CONTROL, 0);
        let mut recs: Vec<(u8, Record)> = Vec::with_capacity(pending.len());
        for (slot, root) in &pending {
            if overflow.contains(slot) {
                let Some(l) = plane
                    .and_then(|p| p.table.get(*slot))
                    .filter(|l| l.state != crate::slot_lease_core::LeaseState::Unleased)
                else {
                    continue;
                };
                let page_addr = self.page_addr_of(l.holder).await?;
                let words = plane.map_or(l.words, |p| self.slot_words_now(p, *slot));
                let value = super::slot_state::SlotState::Leased {
                    appender_id: l.holder,
                    g: l.g,
                    page_addr,
                    root: *root,
                    cursor: words.cursor,
                    slot_tree_extents: words.extents,
                    seq_floor: l.words.seq_floor,
                }
                .encode()?;
                recs.push((
                    tag,
                    Record::put(super::slot_state::slot_state_key(*slot), 0, value),
                ));
                continue;
            }
            // The UNARMED forest (PR 1–3): one appender, one cursor — the
            // native watermark rides the ledger's `next_ino`. An armed
            // plane's UNLEASED slot keeps its release record's `g`,
            // cursor (§5.1.8 — the floor never regresses), extent count
            // and `last_written`; only the root and the tails move.
            let lease = plane
                .and_then(|p| p.table.get(*slot))
                .filter(|l| l.state == crate::slot_lease_core::LeaseState::Unleased);
            let state = match lease {
                Some(l) => {
                    let tails = match forest.tree(*slot) {
                        Some(t) => self.leaf_tails(&t).await?,
                        None => Vec::new(),
                    };
                    super::slot_state::SlotState::Unleased {
                        root: *root,
                        cursor: l.words.cursor,
                        g: l.g,
                        slot_tree_extents: l.words.extents,
                        last_written: l.last_written,
                        seq_floor: l.words.seq_floor,
                        tails,
                    }
                }
                None => super::slot_state::SlotState::Unleased {
                    root: *root,
                    cursor: 0,
                    g: 0,
                    slot_tree_extents: 0,
                    last_written: 0,
                    seq_floor: 0,
                    tails: Vec::new(),
                },
            };
            let value = state.encode()?;
            recs.push((
                tag,
                Record::put(super::slot_state::slot_state_key(*slot), 0, value),
            ));
        }
        let len = entry_len_for(&recs)?;
        // Test seam: a deferred publication (the reserve-exhausted arm)
        // on demand — `tests/sym_forest_tests.rs` pins that the deferral
        // keeps every unpublished root's records in the window.
        if TEST_FOREST_PUBLISH_DEFER
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| if n == u32::MAX { n } else { n - 1 })
            })
            .is_ok()
        {
            return Err(KvError::JournalReserveExhausted { needed: len });
        }
        let Some(adm) = self.ring.try_admit(len, AdmissionClass::Checkpoint) else {
            return Err(KvError::JournalReserveExhausted { needed: len });
        };
        // The named images first (see the doc): one barrier covers every
        // fresh root of this cycle.
        self.sync_device().await.map_err(KvError::Io)?;
        let (res, seq_base) = self.ring.reserve_registered(adm);
        for (i, (_, r)) in recs.iter_mut().enumerate() {
            r.seq = seq_base + i as u64;
        }
        let control = Arc::clone(forest.control());
        for (_, r) in &recs {
            control
                .apply_replayed(
                    &r.key,
                    r.seq,
                    r.kind,
                    Bytes::copy_from_slice(&r.value),
                    res.start,
                )
                .await?;
        }
        if let Err(e) = self.ring.commit_entry(&res, &recs).await {
            // The journal-failure class: the RAM apply stands (tree 0's
            // leaf carries the records into the flush pass), the roots
            // stay UNPUBLISHED so their floors keep clamping the tail,
            // and the volume escalates like every other failed journal
            // write.
            log::error!(
                "meta volume {}: forest root publication entry write failed ({e}) — the \
                 roots stay unpublished (their floors keep clamping the checkpoint tail)",
                self.path.display()
            );
            self.note_journal_failure();
            return Err(e);
        }
        for (slot, root) in pending {
            forest.note_published(slot, root);
            // The RAM table's unleased words follow the record (a later
            // grant hands the requester the root the record names).
            if let Some(p) = plane {
                if let Some(mut l) = p.table.get(slot) {
                    if l.state == crate::slot_lease_core::LeaseState::Unleased {
                        l.words.root = (root.addr, root.seq);
                        p.table.load(slot, l);
                    }
                }
            }
        }
        Ok(())
    }

    /// A coherent READER's forest resync at an epoch step (spec §6.8 item
    /// 2 on a forest volume): tree 0's root was just adopted from the
    /// ledger, so its `slot_state` population is the writer's checkpoint
    /// — adopt every guest root it names onto the guest tree the reader
    /// holds, and OPEN every guest the reader has never seen (a slot the
    /// writer minted after the reader mounted). A no-op on a flat volume.
    /// The reader mints nothing and publishes nothing.
    pub(super) async fn forest_reader_resync(&self) -> std::result::Result<(), KvError> {
        let Some(forest) = self.forest() else {
            return Ok(());
        };
        let control = Arc::clone(forest.control());
        // The directory is read ONCE per pass, at the first leased record
        // (review round 2, Issue 17 — it was read once per `Leased`
        // record per epoch step).
        let mut directory: Option<Vec<super::appender::AppenderEntry>> = None;
        let native = self.appenders.as_ref().map_or(0, |a| a.native_slot);
        let (mut cursor, end) = super::slot_state::slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                let slot = super::slot_state::decode_slot_state_key(k)?;
                let root = match super::slot_state::SlotState::decode(v)? {
                    super::slot_state::SlotState::Unleased { root, .. } => root,
                    super::slot_state::SlotState::Leased {
                        appender_id, root, ..
                    } => {
                        if directory.is_none() {
                            directory =
                                Some(super::appender::read_directory(&self.path, &self.sb).await?);
                        }
                        let directory = directory.as_deref().unwrap_or(&[]);
                        Self::leased_root_from_directory(directory, native, appender_id, slot, root)
                    }
                };
                if root.addr == 0 || slot == super::record::NATIVE_FOREST_SLOT {
                    continue;
                }
                match forest.tree(slot) {
                    Some(t) => {
                        if t.root() != root {
                            t.adopt_root(root)?;
                        }
                    }
                    None => {
                        let tree = KvTree::open_slot_tree(
                            Arc::clone(&self.cache),
                            slot,
                            root,
                            self.seq_handle(),
                        )
                        .await?;
                        forest.adopt_guest(slot, Arc::new(tree));
                    }
                }
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(())
    }

    /// The unpublished guest roots' floors per slot (empty on a flat
    /// volume) — folded into each slot's REGION tail by the checkpoint.
    pub(super) fn unpublished_root_floors(
        &self,
    ) -> std::collections::BTreeMap<super::record::ForestSlot, u64> {
        self.forest()
            .map(|f| f.unpublished_root_floors())
            .unwrap_or_default()
    }

    /// The tree a defrag census target `(header tree id, node addr)`
    /// belongs to: the per-kind tree on a flat volume; on a forest volume
    /// the cached node's owner stamp (a target whose node is no longer
    /// mapped is a stale census entry — `None`).
    fn tree_for_census_target(&self, tree_id: u8, addr: u64) -> Option<Arc<KvTree>> {
        match &self.trees {
            TreeSet::Flat { .. } => self
                .all_trees()
                .into_iter()
                .find(|t| t.tree_id() == tree_id),
            TreeSet::Forest { forest, .. } => {
                let node = self.cache.try_get(addr)?;
                if node.tree_id() != tree_id {
                    return None;
                }
                forest.tree_for_node(&node).ok()
            }
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

    /// The volume's journal lane (C-2), spawned on first use; `None` under
    /// `SQUEEZEFS_JOURNAL_LANE=0`.
    fn journal_lane(&self) -> Option<&Arc<super::journal_lane::JournalLane>> {
        self.journal_lane
            .get_or_init(|| {
                if !crate::env_knobs::bool_knob("SQUEEZEFS_JOURNAL_LANE", true) {
                    return None;
                }
                let idx = super::journal_lane::JOURNAL_LANES_SPAWNED.load(Ordering::Relaxed);
                Some(super::journal_lane::JournalLane::spawn(
                    &self.path,
                    idx as usize,
                ))
            })
            .as_ref()
    }

    /// Spawn a conveyor-stage task (the apply pass, the durability lane)
    /// on the volume's journal lane — or on the shared `sqz-meta` pool
    /// when the lane is off. Both venues are plane-critical, panic-
    /// contained and counted.
    fn spawn_conveyor_task<F>(&self, site: &'static str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        match self.journal_lane() {
            Some(lane) => lane.spawn_task(site, fut),
            None => crate::meta_exec::spawn_meta(site, fut),
        }
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
    ///    **detached, panic-guarded pass task** ([`Self::conveyor_pass_task`])
    ///    and then parks on its own oneshot like every follower;
    /// 3. awaits its result. Dropping this future at the await drops
    ///    only the oneshot receiver and this frame's `Arc` refs — the
    ///    queue entry co-owns the guard set, so same-key exclusion
    ///    survives until the pass reaches the tx's terminal outcome
    ///    (§5.5 revision 2, Issue 13).
    ///
    /// The batch pipeline itself is two stages (module docs, "The
    /// two-stage commit conveyor"): the APPLY pass — one Σ admission,
    /// union leaf locks, contiguous per-tx reservations, RAM apply, one
    /// submitted `write_at_batch` — is [`Self::run_batch`]; the
    /// DURABILITY lane — write completion, completed prefix, one barrier,
    /// fan-out in journal order — is [`Self::run_windows`]. A batch of 1
    /// runs the per-tx pipeline stages byte-for-byte on the ring (the
    /// degenerate case IS the pre-conveyor entry shape).
    async fn commit_tx(&self, tx: KvTx) -> std::result::Result<(), KvError> {
        if tx.is_empty() {
            return Ok(());
        }
        // (0) The door (PR 4): every slot the tx names is this mount's
        // before the tx is queued — a slot mid-handover parks here, a
        // foreign one refuses `SlotBusy`; the tokens ride the queue entry
        // to the terminal outcome. A no-op unarmed.
        let door = self.ensure_leases_for_tx(&tx).await?;
        // (1) Exact size before anything is queued (§4.4 pt 5) — an
        // oversized / undecodable tx fails ALONE, never inside a batch.
        let weak = self.conveyor_identity()?;
        let (entry, rx) = self.build_queued_tx(tx, door)?;
        let len = entry.len;

        // (2) Enqueue + leader-elect — no await between the two.
        self.conveyor.enqueue(entry, len);
        self.lead_pass_if_elected(weak);

        // (3) Park on the fan-out.
        self.park_on_outcome(rx, len).await
    }

    /// The conveyor identity the pass task upgrades per batch — resolved
    /// BEFORE anything is enqueued so a broken wiring fails loud with
    /// nothing queued (unreachable by construction — set at every open
    /// path before the first commit). The pass task holds the QUEUE
    /// strongly but the backend only weakly: an idle pass must never keep
    /// a dropped-without-shutdown backend — and its writer flock — alive
    /// past the last user `Arc`.
    fn conveyor_identity(&self) -> std::result::Result<Weak<KvMetaBackend>, KvError> {
        self.conveyor_self.get().cloned().ok_or_else(|| {
            KvError::Corrupt(
                "conveyor identity missing (commit before open wiring?) — refusing \
                     to enqueue a tx no pass task could ever drain"
                    .to_string(),
            )
        })
    }

    /// Leader-elect after an enqueue; the winner spawns the detached,
    /// panic-guarded pass task (§5.5 lifecycle: no client-visible
    /// cancellation can drop the pass mid-flight — the journal's
    /// accounting is unforgiving: a dropped Admission leaks budget
    /// forever, an uncompleted reservation wedges completed_upto).
    fn lead_pass_if_elected(&self, weak: Weak<KvMetaBackend>) {
        if self.conveyor.try_lead() {
            let conveyor = Arc::clone(&self.conveyor);
            // Stage 1c: the pass task is PLANE-CRITICAL (it holds the 4b
            // union leaf locks and every committer parks on its fan-out)
            // — the volume's journal lane (C-2; the sqz-meta pool with the
            // lane off), never the main tokio runtime.
            self.spawn_conveyor_task("kv_conveyor_pass", Self::conveyor_pass_task(conveyor, weak));
        }
    }

    /// Park a committer on its tx's fan-out (step 3 of the pipeline). A
    /// closed channel means the pass died between drain and fan-out — the
    /// panic sentinel already failed the batch loud (EIO here is the belt,
    /// not the mechanism). The wedge census gauges the park (2026-08-07):
    /// writers parked here while passes stay flat IS the stalled-conveyor
    /// signature; the stage-1b named-wait census ages it in the
    /// watchdog's lock-wait lines (the gauge counts it; this NAMES it).
    async fn park_on_outcome(
        &self,
        rx: squeezefs_ipc::sqz_channel::oneshot::Receiver<std::result::Result<(), KvError>>,
        len: u64,
    ) -> std::result::Result<(), KvError> {
        let _parked = super::ParkedGaugeGuard::enter(&super::META_COMMIT_PARKED);
        let _census = crate::fuse_client::LockWaitToken::begin(
            crate::fuse_client::LockClass::Commit,
            0,
            0,
            len,
        );
        match rx.await {
            Ok(out) => out,
            Err(_) => Err(KvError::Io(self.eio(
                "commit conveyor pass dropped its result channel (pass panic — batch \
                 failed loud)",
            ))),
        }
    }

    /// **Group commit (D-1c — e2e perf audit §5.3 row 1, one conveyor
    /// group per shipped frame).** `commit_tx`'s pipeline over a SET of
    /// staged transactions, enqueued under ONE queue-lock acquisition
    /// ([`ConveyorCore::enqueue_many`]) so a drain can never take part of
    /// the group: N distinct-object transactions become one apply pass by
    /// construction, not by arrival timing. Nothing about the group
    /// reaches the ring — every member stays its own ordinary checksummed
    /// journal entry (one tx = one entry; the crash contract is untouched),
    /// the drain caps still govern (the byte cap may split an over-cap
    /// group, progress-first), and each member's DLM guards release at ITS
    /// terminal outcome.
    ///
    /// Per member, in input order: an empty tx answers `Ok(())` inline; a
    /// tx that fails admission (size / value cap) gets ITS error and is not
    /// enqueued — its siblings are unaffected; the rest are built first,
    /// then enqueued together, then ONE election, then every member's
    /// fan-out is awaited in order (the pass answers in journal order,
    /// which is the group's order). The parked-committer census and the
    /// op-trace stamps apply per member exactly as `commit_tx` applies them.
    pub async fn commit_tx_group(&self, txs: Vec<KvTx>) -> Vec<std::result::Result<(), KvError>> {
        let n = txs.len();
        let mut results: Vec<Option<std::result::Result<(), KvError>>> =
            (0..n).map(|_| None).collect();
        if n == 0 {
            return Vec::new();
        }
        // Unreachable by construction (wired at every open path); every
        // member gets the same loud refusal with nothing queued.
        let Ok(weak) = self.conveyor_identity() else {
            return (0..n).map(|_| self.conveyor_identity().map(drop)).collect();
        };
        let mut entries: Vec<(QueuedTx, u64)> = Vec::with_capacity(n);
        let mut waits = Vec::with_capacity(n);
        for (i, tx) in txs.into_iter().enumerate() {
            if tx.is_empty() {
                results[i] = Some(Ok(()));
                continue;
            }
            let door = match self.ensure_leases_for_tx(&tx).await {
                Ok(d) => d,
                Err(e) => {
                    results[i] = Some(Err(e));
                    continue;
                }
            };
            match self.build_queued_tx(tx, door) {
                Ok((entry, rx)) => {
                    let len = entry.len;
                    entries.push((entry, len));
                    waits.push((i, rx, len));
                }
                Err(e) => results[i] = Some(Err(e)),
            }
        }
        if !entries.is_empty() {
            let members = self.conveyor.enqueue_many(entries);
            super::META_CONVEYOR_GROUP_COMMITS.fetch_add(1, Ordering::Relaxed);
            super::META_CONVEYOR_GROUP_TXS.fetch_add(members as u64, Ordering::Relaxed);
            self.lead_pass_if_elected(weak);
        }
        for (i, rx, len) in waits {
            results[i] = Some(self.park_on_outcome(rx, len).await);
        }
        results
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(KvError::Corrupt(
                        "group commit: a member reached no outcome (unreachable — every \
                         member is answered inline, refused at admission, or awaited)"
                            .to_string(),
                    ))
                })
            })
            .collect()
    }

    /// Steps (1)–(2a) of the commit pipeline for ONE tx: encode its
    /// records, enforce the per-volume value cap and the exact-size
    /// admission (§4.4 pt 5 — an oversized / undecodable tx fails ALONE,
    /// before it can join any batch), mint its fan-out channel and stamp
    /// its op-trace enqueue. Returns the queue entry (not yet enqueued)
    /// and the committer's receiver.
    fn build_queued_tx(
        &self,
        tx: KvTx,
        door: Option<super::slot_lease::DoorPass>,
    ) -> std::result::Result<
        (
            QueuedTx,
            squeezefs_ipc::sqz_channel::oneshot::Receiver<std::result::Result<(), KvError>>,
        ),
        KvError,
    > {
        // D4.a attribution: the construction site this (about-to-be-
        // committed) tx counts against on success.
        let site = tx.site;
        // The staged key is the shipped per-kind key; on a forest volume
        // it takes its §5.2.1 kind byte HERE, once — every downstream step
        // (leaf resolution, the journal entry, replay, the migration tee)
        // sees the forest key, and the tag stays the kind. The write-side
        // audit reads the LEGACY key, so it runs on the staged tuple
        // before the framing.
        let mut recs: Vec<(u8, Record)> = Vec::with_capacity(tx.staged.len());
        for (tree_id, key, kind, value) in tx.staged {
            let mut r = Record {
                key,
                seq: 0, // stamped from the reservation, in-lock
                kind,
                // D1.c stage_put audit: `Bytes::to_vec` COPIED every
                // staged value at commit; `Vec::from(Bytes)` reclaims
                // the unique Vec-backed allocation instead (stage
                // sites build values as `Bytes::from(vec)`).
                value: Vec::from(value),
            };
            // Finding-A hardening: staged records must decode under their
            // own kind's typed decoder before any byte is persisted (debug
            // tiers).
            #[cfg(debug_assertions)]
            super::node::debug_audit_records(tree_id, 0, std::slice::from_ref(&r));
            r.key = self.stage_key(tree_id, r.key)?;
            recs.push((tree_id, r));
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
        let region = self.region_of_records(&recs)?;
        self.note_slot_ops(&recs);

        let (done, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        // op-trace (audit A2): the tx carries the ORIGINATING op's id
        // (the committer's task scope) so the pass's stages join that
        // op's chain; the enqueue instant already read is its
        // `meta_enqueue` stamp.
        let enqueued_at = std::time::Instant::now();
        let trace_id = crate::op_trace::current_op();
        crate::op_trace::stamp(trace_id, crate::op_trace::Stage::MetaEnqueue, enqueued_at);
        Ok((
            QueuedTx {
                recs,
                region,
                len,
                enqueued_at,
                trace_id,
                site,
                _guards: tx.guards,
                _door: door,
                done,
            },
            rx,
        ))
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
            let BatchProduct { outcomes, windows } = be.run_batch(batch).await;
            // Stage A → stage B handoff (D-2): each window joins the
            // durability lane's in-order queue; the enqueue and the
            // leader-elect are two uninterruptible steps (no await between
            // them), so a window can never sit in the lane leaderless.
            for window in windows {
                super::note_window_inflight();
                let lane = Arc::clone(&be.durability_lane);
                let len = window.entries.len() as u64;
                lane.enqueue(window, len);
                if lane.try_lead() {
                    be.spawn_conveyor_task(
                        "kv_conveyor_durability",
                        Self::durability_lane_task(lane, Weak::clone(&weak)),
                    );
                }
            }
            // Release the backend BEFORE waking committers: a woken
            // committer may own the last user `Arc` and re-open.
            drop(be);
            // The pre-reserve batch failures are the pass's own terminal
            // outcomes (nothing reserved, nothing applied) — fan them out
            // here; everything else answers from the durability lane.
            Self::fan_out(outcomes);
        }
    }

    /// Fan out per-tx terminal outcomes; each entry's guard set is
    /// released at ITS terminal outcome (post-rollback on failure) —
    /// and, since D-3, BEFORE the result is sent: the outcome is terminal
    /// either way, and a committer answered while its guards were still
    /// held could re-ask for the same key (its next op on the same
    /// parent) and park on its OWN previous tx for the send→drop gap —
    /// the in-process storm measured that self-wait on 11 % of many-dirs
    /// renames at 32 writers (`dlm_inode_key_waits`, p50 ≤ 16 µs). This
    /// is the one 4a scoping the D5 co-ownership law permits: the hold
    /// still spans every byte of the commit park. op-trace `fanout`: a
    /// clock read per TRACED member only (the untraced population pays a
    /// field compare).
    fn fan_out(outcomes: Vec<(QueuedTx, std::result::Result<(), KvError>)>) {
        for (q, outcome) in outcomes {
            if q.trace_id != 0 {
                crate::op_trace::stamp_now(q.trace_id, crate::op_trace::Stage::Fanout);
            }
            let QueuedTx { _guards, done, .. } = q;
            drop(_guards);
            let _ = done.send(outcome);
        }
    }

    /// Abandon a window whose write outcome will never be observed (the
    /// panic sentinels' arm): complete its reservation as **abandoned**
    /// (the §4.4 pt 4 mechanism — if the bytes landed anyway, replay
    /// applies an un-acked tx, which the crash contract permits; if not,
    /// the checksum walk drops the hole), fail every member EIO, and close
    /// the in-flight gauge. All-sync.
    fn abandon_window(&self, w: ConveyorWindow, why: &str) {
        if w.res_open {
            w.ring.complete(&w.res);
        }
        self.region_window_settled(w.region);
        for q in w.entries {
            let _ = q.done.send(Err(KvError::Io(self.eio(why))));
        }
        super::note_window_done();
    }

    /// A stage-B window of `region` came into being (the pass's handoff).
    fn region_window_opened(&self, region: u32) {
        if let Some(r) = self.declared_region(region) {
            r.windows_inflight.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A stage-B window of `region` reached its terminal outcome (acked,
    /// rolled back and compensated, or abandoned) — its ring may be
    /// replaced from here.
    fn region_window_settled(&self, region: u32) {
        if let Some(r) = self.declared_region(region) {
            r.windows_inflight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Region `region` if it is a DECLARED one (≥ 1 — region 0's fixed
    /// ring never grows, so it keeps no window count).
    fn declared_region(&self, region: u32) -> Option<&Arc<super::appender::AppenderRegion>> {
        self.appenders
            .as_ref()
            .and_then(|a| a.region(region))
            .filter(|r| r.id != 0)
    }

    /// **Stage B — the per-volume durability lane** (D-2; module docs "The
    /// two-stage commit conveyor"). Drains windows in handoff order and
    /// takes each GROUP — the head window (awaited) plus every following
    /// window whose write has already landed — to its terminal outcome:
    /// complete the reservations, ONE completed-prefix wait, the hole
    /// checkpoints, ONE strict barrier, then the members' outcomes in
    /// journal order. Never touches a node lock except in the §4.4 pt 4
    /// rollback arm of a FAILED write (the 4b argument in the module doc).
    /// Same lifecycle as the apply pass: holds the backend per group only,
    /// releases it before waking committers, exits on an empty drain via
    /// the release-then-recheck protocol (the loom-modeled core).
    async fn durability_lane_task(
        lane: Arc<ConveyorCore<ConveyorWindow>>,
        weak: Weak<KvMetaBackend>,
    ) {
        // Windows drained but not yet processed: the lane drains the whole
        // queue at once (FIFO) and works its way through in groups, so a
        // window whose write has not landed waits HERE (in order) while the
        // group ahead of it is answered.
        let mut carry: std::collections::VecDeque<ConveyorWindow> =
            std::collections::VecDeque::new();
        loop {
            if carry.is_empty() {
                carry.extend(lane.drain(usize::MAX, u64::MAX));
            }
            let Some(head) = carry.pop_front() else {
                if !lane.unlead_and_recheck() {
                    return;
                }
                continue;
            };
            let Some(be) = weak.upgrade() else {
                // Backend dropped without shutdown: live committers hold
                // `&self`, so these windows' committers are gone — their
                // writes complete on their own (the pool holds path +
                // bytes), the guards release with the entries. Answer the
                // dead receivers (the sends are the belt) and release.
                let dead = |w: ConveyorWindow| {
                    for q in w.entries {
                        let _ = q.done.send(Err(KvError::Corrupt(
                            "meta volume dropped with commit windows in flight".to_string(),
                        )));
                    }
                    super::note_window_done();
                };
                dead(head);
                for w in carry.drain(..) {
                    dead(w);
                }
                loop {
                    for w in lane.drain(usize::MAX, u64::MAX) {
                        dead(w);
                    }
                    if !lane.unlead_and_recheck() {
                        return;
                    }
                }
            };
            let outcomes = be.run_windows(head, &mut carry).await;
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
            Self::fan_out(outcomes);
        }
    }

    /// One conveyor batch through the §4.4 pipeline's APPLY stage (§5.5:
    /// "runs the pipeline ONCE for the batch"), under the panic sentinel.
    /// Never errs — it returns either the pre-reserve terminal outcomes
    /// (every member failed as a unit, nothing reserved or applied) or the
    /// applied-and-submitted window for the durability lane.
    async fn run_batch(self: &Arc<Self>, batch: Vec<QueuedTx>) -> BatchProduct {
        // One group per appender region the batch touched, in queue order
        // of first appearance: a group is one reservation in ONE ring
        // (design-symmetric-metadata §5.3 — a tx journals into its
        // appender's ring; an unpartitioned volume is the one-group case,
        // byte-identical to the pre-region pass).
        let partitioned = self.appenders.as_ref().is_some_and(|a| a.is_partitioned());
        if !partitioned {
            return self.run_batch_group(batch, 0).await;
        }
        let mut groups: Vec<(u32, Vec<QueuedTx>)> = Vec::new();
        for q in batch {
            match groups.iter_mut().find(|(r, _)| *r == q.region) {
                Some((_, g)) => g.push(q),
                None => groups.push((q.region, vec![q])),
            }
        }
        let mut product = BatchProduct {
            outcomes: Vec::new(),
            windows: Vec::new(),
        };
        for (region, group) in groups {
            let BatchProduct { outcomes, windows } = self.run_batch_group(group, region).await;
            product.outcomes.extend(outcomes);
            product.windows.extend(windows);
        }
        product
    }

    /// [`Self::run_batch`] for the members of ONE region: the pipeline
    /// once over the group, reserving in that region's ring.
    async fn run_batch_group(self: &Arc<Self>, batch: Vec<QueuedTx>, region: u32) -> BatchProduct {
        use crate::fuse_client::{
            meta_txpass_phase_record, meta_txpass_phase_record_dur, MetaTxPassPhase,
        };
        super::META_CONVEYOR_LEADER_PASSES.fetch_add(1, Ordering::Relaxed);
        super::META_COMMIT_GROUP_SIZE.record(batch.len());
        super::META_COMMIT_GROUP_BYTES
            .fetch_add(batch.iter().map(|q| q.len).sum::<u64>(), Ordering::Relaxed);
        // Pass decomposition (2026-08-01): queue residence per drained tx
        // + the whole-pass span. op-trace (audit A2): the batch's traced
        // members (a fixed-size set, collected once) receive every
        // pass-level stage; `pass_begin` is the ONE instant that closes
        // each member's `tx_queue_wait`.
        let t_pass = std::time::Instant::now();
        let mut traced = crate::op_trace::TracedBatch::new();
        for q in &batch {
            meta_txpass_phase_record_dur(
                MetaTxPassPhase::TxQueueWait,
                t_pass.saturating_duration_since(q.enqueued_at),
            );
            traced.push(q.trace_id);
        }
        traced.stamp(crate::op_trace::Stage::PassBegin, t_pass);

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
        // The Dekker pair with a region's ring GROWTH (`grow_stalled_
        // regions`): a pass announces itself inside the region's window
        // before it loads the ring, growth announces itself before it
        // reads the count — one of the two always sees the other, so a
        // pass never reserves on a ring the checkpoint task is replacing.
        let growth_gate = self
            .appenders
            .as_ref()
            .and_then(|a| a.region(region))
            .filter(|r| r.id != 0)
            .cloned();
        if let Some(r) = &growth_gate {
            loop {
                // Register-recheck-await: the wake is armed BEFORE the
                // flag is read, so growth's `end_growth` between the two
                // is never lost (the `sqz_notify` create-recheck idiom).
                let released = r.growth_done.notified();
                r.passes_inside.fetch_add(1, Ordering::SeqCst);
                if !r.growing.load(Ordering::SeqCst) {
                    break;
                }
                r.passes_inside.fetch_sub(1, Ordering::SeqCst);
                released.await;
            }
        }
        let ring = self.ring_of_region(region);
        let mut sentinel = PassSentinel {
            be: self,
            ring,
            region,
            t_pass,
            entries: batch,
            outcomes: Vec::new(),
            admission: None,
            reservation: None,
            applied_unrolled: false,
            window: None,
            traced,
        };
        self.run_batch_pipeline(&mut sentinel).await;
        if let Some(r) = &growth_gate {
            r.passes_inside.fetch_sub(1, Ordering::SeqCst);
        }
        debug_assert!(
            sentinel.entries.is_empty()
                && sentinel.admission.is_none()
                && sentinel.reservation.is_none(),
            "the batch pipeline must reach a terminal outcome or a handoff for every entry \
             on every non-panic path (the sentinel is for unwinds only)"
        );
        // `pass_total` is the SERIALIZED server's service time — drain →
        // handoff; the device wait it used to contain is the lane's.
        meta_txpass_phase_record(MetaTxPassPhase::PassTotal, t_pass, &sentinel.traced);
        BatchProduct {
            outcomes: std::mem::take(&mut sentinel.outcomes),
            windows: sentinel.window.take().into_iter().collect(),
        }
    }

    /// The APPLY-stage pipeline body. Mutates the sentinel as protocol
    /// stages pass so an unwind at ANY await leaves exactly the right
    /// cleanup state; on every normal path it either stages every member's
    /// terminal outcome (a pre-reserve batch failure) or builds the window
    /// the durability lane takes over, emptying the sentinel's entries
    /// itself.
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
        match self.admit_user_budget(&s.ring, s.region, total_len).await {
            Ok(adm) => s.admission = Some(adm),
            Err(e) => {
                self.fail_batch(s, &e);
                return;
            }
        }
        meta_txpass_phase_record(MetaTxPassPhase::PassAdmission, t_adm, &s.traced);

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
        // e2e audit D: the PURE lock-acquire wait inside that window, Σ
        // over every attempt (`lock_phase_ns.leaf_lock_wait`).
        let mut leaf_wait = std::time::Duration::ZERO;
        let mut attempt = 0usize;
        // §4.7 heap admission: whether this pass already spent its ONE
        // checkpoint cycle trying to return budget for a refused member.
        let mut cycled_for_space = false;
        let (res, undo, failed) = loop {
            attempt += 1;
            if attempt > COMMIT_RETRY_BUDGET {
                let adm = s.admission.take().expect("admission held until reserve");
                s.ring.core().release(adm);
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
                    let tree = match self.tree_for_record(*tree_id, &r.key).await {
                        Ok(t) => t,
                        Err(e) => {
                            resolve_err = Some(e);
                            break 'resolve;
                        }
                    };
                    match tree.resolve_leaf(&r.key).await {
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
                s.ring.core().release(adm);
                self.fail_batch(s, &e);
                return;
            }
            // Deduped ascending-NodeId lock order over the UNION
            // (§4.4 pt 1 / §4.9 4b verbatim, over a union set).
            let mut lock_set: Vec<Arc<CachedNode>> = leaves.iter().flatten().cloned().collect();
            lock_set.sort_by_key(|n| n.addr());
            lock_set.dedup_by_key(|n| n.addr());
            let mut guards = Vec::with_capacity(lock_set.len());
            let t_leaf = std::time::Instant::now();
            for node in &lock_set {
                guards.push(node.lock().write().await);
            }
            leaf_wait += t_leaf.elapsed();
            // e2e audit D / D-2: the union HOLD (first acquire → every
            // guard dropped) — recorded at every release site of this
            // attempt; the "never across device I/O" law's instrument.
            let record_hold = || {
                crate::fuse_client::lock_phase_record(
                    crate::fuse_client::LockPhase::LeafLockHold,
                    t_leaf.elapsed(),
                )
            };
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
                record_hold();
                drop(guards);
                super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // (3b) §4.7 HEAP ADMISSION, under the locks (design §4.7
            // "ENOSPC semantics", amended 2026-09-11): a user commit does
            // not claim an extent — its records land in leaf overlays and
            // the checkpoint's flush pass claims for the SMOs they force —
            // so the reserve was consumed by acked user growth, the pass
            // ran out of heap mid-flush, the deferred nodes pinned the
            // tail, and the wedged-tail audit fail-stopped a FULL volume
            // (the 2026-09-09 sweep's P1). Every acked record must be
            // FLUSHABLE: per leaf this member's records land on, project
            // the flush — fits in place (no SMO, nothing to promise) or
            // needs an SMO, whose extents are PROMISED now against the
            // claimable budget minus every outstanding promise, split at
            // the reserve (net-growth) or the compaction floor (net-zero);
            // a member that cannot be promised refuses `NoSpace` ALONE,
            // before anything is reserved or applied.
            let refused = self.admit_heap_locked(&s.entries, &leaves, &lock_set, &mut guards);
            if !refused.is_empty() {
                record_hold();
                drop(guards);
                let adm = s.admission.take().expect("admission held until reserve");
                s.ring.core().release(adm);
                // Budget the checkpoint can RETURN — promises held by nodes
                // the flush pass has not reached yet (turned into claims at
                // their SMO, or released at their append) and retirements
                // parked on the tail — earns the refused members ONE retry
                // behind the cycles that return it: on a full volume a
                // burst of deletes would otherwise trip over its own
                // outstanding compaction promises between two cadence
                // ticks. Two barriered cycles, because reclamation lags one
                // cycle by design (§4.6 pt 2): the first cycle's SMOs park
                // their old extents at gates past the tail that cycle
                // covers, the second cycle's tail passes them. A refusal
                // that survives (or finds nothing to return) is final:
                // ENOSPC. No node lock is held across the cycles.
                let returnable = self.cache.heap_promised() > 0 || self.alloc.pending_count() > 0;
                if !cycled_for_space && returnable {
                    cycled_for_space = true;
                    for _ in 0..2 {
                        if let Err(e) = self.checkpoint_now().await {
                            log::warn!(
                                "meta volume {}: checkpoint cycle for heap headroom failed: {e} \
                                 (the refused members will be answered on the retry)",
                                self.path.display()
                            );
                            break;
                        }
                        if self.alloc.pending_count() == 0 {
                            break;
                        }
                    }
                } else {
                    // Fan the refusals out (pre-reserve terminal outcomes).
                    // A GRANT refusal is the appender's manager dependency
                    // (EAGAIN, already counted), never the heap's space
                    // class: it neither latches `heap_full` nor counts an
                    // ENOSPC.
                    for (qi, e) in refused.into_iter().rev() {
                        if !matches!(e, KvError::GrantExhausted { .. }) {
                            self.enospc_refusals.fetch_add(1, Ordering::Relaxed);
                            self.enter_heap_full(&format!("a user commit was refused: {e}"));
                        }
                        let q = s.entries.remove(qi);
                        s.outcomes.push((q, Err(e)));
                    }
                    if s.entries.is_empty() {
                        return;
                    }
                }
                // Re-admit the ring budget — no node lock held across the
                // park (§4.4 pt 5) — and re-resolve everything.
                let survivors_len: u64 = s.entries.iter().map(|q| q.len).sum();
                match self
                    .admit_user_budget(&s.ring, s.region, survivors_len)
                    .await
                {
                    Ok(adm) => s.admission = Some(adm),
                    Err(e) => {
                        self.fail_batch(s, &e);
                        return;
                    }
                }
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
                    record_hold();
                    drop(guards);
                    let adm = s.admission.take().expect("admission held until reserve");
                    s.ring.core().release(adm);
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
            let (res, seq_base) = s.ring.reserve_registered(adm);
            s.reservation = Some(res);
            {
                let mut seq_cursor = seq_base;
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
            // The slot record frontier (Issue 11): every slot-stamped
            // node of the union carries records below this batch's end.
            for node in &lock_set {
                if let Some(slot) = node.forest_slot() {
                    self.cache.note_slot_record_frontier(slot, seq_cursor);
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
                    // The leaf's own tree (by header id on a flat volume,
                    // by its owner stamp on a forest one) — every leaf in
                    // the lock set was resolved through its tree above.
                    if let Ok(tree) = self.tree_of_node(node) {
                        tree.enqueue_maintenance(node.addr());
                        threshold_crossed = true;
                    }
                }
            }
            record_hold();
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
        meta_txpass_phase_record(MetaTxPassPhase::PassLeafLocks, t_locks, &s.traced);
        crate::fuse_client::lock_phase_record(
            crate::fuse_client::LockPhase::LeafLockWait,
            leaf_wait,
        );

        // (6) SUBMIT the surviving members' entries, outside every lock —
        // N ORDINARY checksummed entries in the one contiguous reservation,
        // one `uring_fs` submission, NO wait (D-2): the completion is the
        // durability lane's to await. A failed member's sub-range stays
        // unwritten (the §4.4 pt 4 unwritten-hole mechanism; replay's
        // checksum walk drops it). An encode refusal is a failed write
        // with nothing landed — the lane treats it exactly so.
        let write = {
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
                Ok(None)
            } else {
                s.ring
                    .submit_entries_batch(&parts, self.journal_lane().map(Arc::as_ref))
                    .map(Some)
            }
        };

        // (7) Handoff: the window now owns the reservation, the members and
        // their applied-but-unjournaled state; the lane's sentinel covers
        // them from here (the pass task takes the window the instant this
        // returns — no await in between).
        let res = s.reservation.take().expect("reservation registered");
        let entries = std::mem::take(&mut s.entries);
        s.applied_unrolled = false;
        // The window counts against its region's ring GROWTH from here to
        // its terminal outcome (`region_window_settled`) — announced while
        // the pass is still inside the region's Dekker window, so growth
        // that saw `passes_inside == 0` sees this instead. Its reservation
        // covers the ring until the lane completes it (step 8); the count
        // covers the stage-B tail past that point — the §4.4 pt 4
        // compensation reserves on the ring the window holds (review
        // round 2, Issue 23).
        self.region_window_opened(s.region);
        s.window = Some(ConveyorWindow {
            ring: Arc::clone(&s.ring),
            region: s.region,
            entries,
            failed,
            res,
            res_open: true,
            undo,
            write: Some(write),
            t_pass: s.t_pass,
            t_handoff: std::time::Instant::now(),
            submitted_at: None,
            traced: s.traced,
        });
    }

    /// **Stage B for one group of windows** (D-2): `head` (whose write is
    /// awaited) plus every window behind it in `carry` whose write has
    /// ALREADY landed — taken as one group so the deferred cadence pays one
    /// completed-prefix wait and the strict cadence ONE coalesced barrier
    /// for all of them (group commit of barriers, the §5.5 shape moved
    /// downstream). Windows are answered in handoff order: the members'
    /// terminal outcomes are staged in journal order and fanned out by the
    /// caller after it releases the backend.
    ///
    /// Per window the durability protocol is the pre-D-2 pass tail
    /// verbatim: complete the reservation on BOTH outcomes (a reservation
    /// that never completes wedges `completed_upto`); on success wait the
    /// completed prefix, reset the consecutive-failure rung, checkpoint
    /// past any apply-hole BEFORE acking survivors (§4.4 pt 4's hole
    /// discipline — later windows' entries sit behind the hole in the same
    /// ring pages and are unreachable until the tail passes it); on a
    /// failed write run the §4.4 pt 4 seq-conditional rollback over the
    /// window's range (the one arm in which this stage takes leaf locks —
    /// ascending, deduped, revalidated, never across I/O), escalate through
    /// `note_journal_failure`, and checkpoint past the permanent hole.
    async fn run_windows(
        self: &Arc<Self>,
        head: ConveyorWindow,
        carry: &mut std::collections::VecDeque<ConveyorWindow>,
    ) -> Vec<(QueuedTx, std::result::Result<(), KvError>)> {
        use crate::fuse_client::{
            meta_txpass_phase_record, meta_txpass_phase_record_span, MetaTxPassPhase,
        };
        super::META_CONVEYOR_DURABILITY_PASSES.fetch_add(1, Ordering::Relaxed);
        let mut s = LaneSentinel {
            be: self,
            windows: vec![head],
            outcomes: Vec::new(),
        };

        // (8) Await the head's write; then take every landed successor.
        let head_out = self.await_window_write(&mut s.windows[0]).await;
        while carry.front_mut().is_some_and(|w| w.write_landed()) {
            s.windows.push(carry.pop_front().expect("probed front"));
        }
        let mut write_outs: Vec<std::result::Result<(), KvError>> =
            Vec::with_capacity(s.windows.len());
        write_outs.push(head_out);
        for i in 1..s.windows.len() {
            let out = self.await_window_write(&mut s.windows[i]).await;
            write_outs.push(out);
        }
        // Completion is unconditional and lane-owned: every window's
        // reservation completes now that its write's outcome is known
        // (an unwritten / failed range still completes — an abandoned
        // hole), in journal order so `completed_upto` walks forward.
        for w in s.windows.iter_mut() {
            w.ring.complete(&w.res);
            w.res_open = false;
        }
        // Pre-rollback test seam (the Issue-23 schedule: reservations
        // closed, verdicts not yet taken). Register-recheck-await.
        if TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed) == TEST_CONVEYOR_HOLD_PRE_ROLLBACK {
            TEST_CONVEYOR_PRE_ROLLBACK_PARKED.fetch_add(1, Ordering::AcqRel);
            while TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed)
                == TEST_CONVEYOR_HOLD_PRE_ROLLBACK
            {
                let notified = TEST_CONVEYOR_HOLD_NOTIFY.notified();
                if TEST_CONVEYOR_HOLD_STAGE.load(Ordering::Relaxed)
                    != TEST_CONVEYOR_HOLD_PRE_ROLLBACK
                {
                    break;
                }
                notified.await;
            }
        }

        // (9) Per window, in order: the success arm or the rollback arm.
        // `hole_end` accumulates the furthest position replay's chain walk
        // must start past before any survivor in the group is acked.
        // Apply-holes per REGION: a hole's end is a position in its own
        // ring, so the covering checkpoint is that region's.
        let mut hole_end: Vec<(u32, u64)> = Vec::new();
        let mut any_ok = false;
        // Per-window verdict feeding the outcomes: `Ok(())` = ack pending
        // the group barrier; `Err(msg)` = every survivor fails with `msg`.
        let mut verdicts: Vec<std::result::Result<(), KvError>> =
            Vec::with_capacity(s.windows.len());
        let t_pfx = std::time::Instant::now();
        if write_outs.iter().any(|o| o.is_ok()) {
            // One completed-prefix wait covers every member of the group
            // (their entries all end at-or-before the group end; chain-
            // reachability per the K3 barrier observation) — per RING when
            // the group spans appender regions (each ring has its own
            // completed prefix; positions are not comparable across rings).
            let mut waited: Vec<(Arc<JournalRing>, u64)> = Vec::new();
            for w in &s.windows {
                match waited.iter_mut().find(|(r, _)| Arc::ptr_eq(r, &w.ring)) {
                    Some((_, end)) => *end = (*end).max(w.res.end()),
                    None => waited.push((Arc::clone(&w.ring), w.res.end())),
                }
            }
            for (ring, end) in waited {
                ring.wait_completed_upto(end).await;
            }
        }
        meta_txpass_phase_record(
            MetaTxPassPhase::JournalPrefixWait,
            t_pfx,
            &s.windows[0].traced,
        );
        for (w, write_out) in s.windows.iter().zip(write_outs) {
            match write_out {
                Ok(()) => {
                    self.journal_failures.store(0, Ordering::Release);
                    if !w.failed.is_empty() {
                        match hole_end.iter_mut().find(|(r, _)| *r == w.region) {
                            Some((_, h)) => *h = (*h).max(w.res.end()),
                            None => hole_end.push((w.region, w.res.end())),
                        }
                    }
                    any_ok = true;
                    verdicts.push(Ok(()));
                }
                Err(e) => {
                    log::warn!(
                        "meta volume {}: batch journal write failed (seqs [{}, {})): {e} — \
                         rolling back {} member(s)",
                        self.path.display(),
                        w.res.start,
                        w.res.end(),
                        w.entries.len(),
                    );
                    // (9') Whole-window rollback: the §4.4 pt 4
                    // seq-conditional machinery over the window's
                    // contiguous range with the first-touch pre-images —
                    // exact against every later window's apply (only Δtime
                    // merge records can share a key across windows: every
                    // other same-key writer is still excluded by the DLM
                    // guards this window's entries hold), skip-if-newer
                    // being precisely LWW-correct for those.
                    self.rollback_failed_tx(w.res.start, w.res.end(), &w.undo, &w.ring)
                        .await;
                    self.note_journal_failure();
                    // The reserved range is now a PERMANENT hole in the
                    // ring (§4.1 discovery loses same-page successors of a
                    // dead chain): checkpoint past it — zero ring bytes by
                    // the §4.4 pt 5 progress theorem.
                    if let Err(ck) = self.checkpoint_past_region(w.region, w.res.end()).await {
                        log::error!(
                            "meta volume {}: post-failure checkpoint could not drain the \
                             journal hole: {ck} (volume escalating)",
                            self.path.display()
                        );
                        self.note_journal_failure();
                    }
                    verdicts.push(Err(e));
                }
            }
        }
        // Apply-holes in acked windows: checkpoint past them BEFORE acking
        // survivors, so an immediate crash cannot strand chain-
        // reachability of what is about to be acked (§4.4 pt 4's hole
        // discipline, applied window-mid). One cycle covers the furthest.
        let mut hole_err: Option<String> = None;
        for (region, end) in hole_end {
            if let Err(ck) = self.checkpoint_past_region(region, end).await {
                log::error!(
                    "meta volume {}: post-isolation checkpoint could not cover the batch \
                     hole: {ck} (volume escalating; failing the survivors loud rather than \
                     acking unreachable entries)",
                    self.path.display()
                );
                self.note_journal_failure();
                hole_err = Some(format!(
                    "batch hole checkpoint failed after a member rollback: {ck}"
                ));
            }
        }
        // (10) Strict cadence: ONE coalesced barrier for the whole group
        // (§4.6 pt 4 / §5.5 — the G3 mechanism, now also amortized across
        // the windows that landed while the head was awaited).
        let barrier_out = if hole_err.is_none() && any_ok && self.strict {
            let t_bar = std::time::Instant::now();
            let out = self.sync_device().await.map_err(KvError::Io);
            meta_txpass_phase_record(MetaTxPassPhase::JournalBarrier, t_bar, &s.windows[0].traced);
            out
        } else {
            if hole_err.is_none() && any_ok {
                self.needs_flush.store(true, Ordering::Release);
            }
            Ok(())
        };

        // (11) Terminal outcomes, window by window in journal order. PR
        // VL5b (§5.5.2 step 3): the lane IS the key tee — every user commit
        // on the volume passes through here, so one armed-tee load per
        // group captures every migrating-keyspace key with zero cost when
        // no migration runs.
        let tee = self.migration_tee.load_full();
        let t_done = std::time::Instant::now();
        for (mut w, verdict) in std::mem::take(&mut s.windows).into_iter().zip(verdicts) {
            // The pre-D-2 lump, per window: submission → this window's
            // durability decided (comparable to the baseline's
            // `pass_journal_write`, which ended at the same protocol point).
            if let Some(t_sub) = w.submitted_at {
                meta_txpass_phase_record_span(
                    MetaTxPassPhase::PassJournalWrite,
                    t_sub,
                    t_done,
                    &w.traced,
                );
            }
            meta_txpass_phase_record_span(
                MetaTxPassPhase::WindowTotal,
                w.t_pass,
                t_done,
                &w.traced,
            );
            let mut failed = std::mem::take(&mut w.failed);
            for (qi, q) in std::mem::take(&mut w.entries).into_iter().enumerate() {
                if let Some(pos) = failed.iter().position(|(fi, _)| *fi == qi) {
                    let (_, e) = failed.swap_remove(pos);
                    s.outcomes.push((q, Err(e)));
                    continue;
                }
                let outcome = match (&verdict, &hole_err, &barrier_out) {
                    (Err(e), _, _) => Err(self.clone_kv_error(e)),
                    (Ok(()), Some(msg), _) => Err(KvError::Io(self.eio(msg))),
                    (Ok(()), None, Err(e)) => Err(self.clone_kv_error(e)),
                    (Ok(()), None, Ok(())) => {
                        // D4.a: one successful commit_tx = one journal
                        // entry against the tx's construction site.
                        super::note_commit_site(q.site);
                        if let Some(tee) = tee.as_deref() {
                            // The tee's consumer re-reads by `(kind,
                            // legacy key)`; strip the forest kind byte.
                            match self.legacy_recs(&q.recs) {
                                Ok(legacy) => tee.note_committed(&legacy),
                                Err(e) => log::error!(
                                    "migration tee: a committed record's key does not frame \
                                     as a forest key ({e}) — the slot copy will re-snapshot"
                                ),
                            }
                        }
                        Ok(())
                    }
                };
                s.outcomes.push((q, outcome));
            }
            self.region_window_settled(w.region);
            super::note_window_done();
        }
        std::mem::take(&mut s.outcomes)
    }

    /// Await one window's ring write (D-2 stage B step 8), recording
    /// `window_lane_wait` (handoff → pickup) and `journal_ring_write`
    /// (submission → observed completion). `Ok(())` when nothing was
    /// submitted (every member failed at apply); an encode refusal is the
    /// failed-write outcome it always was.
    async fn await_window_write(&self, w: &mut ConveyorWindow) -> std::result::Result<(), KvError> {
        use crate::fuse_client::{
            meta_txpass_phase_record_dur, meta_txpass_phase_record_span, MetaTxPassPhase,
        };
        let t_pick = std::time::Instant::now();
        meta_txpass_phase_record_dur(
            MetaTxPassPhase::WindowLaneWait,
            t_pick.saturating_duration_since(w.t_handoff),
        );
        match w
            .write
            .take()
            .expect("a window's write is awaited exactly once")
        {
            Ok(None) => Ok(()),
            Ok(Some(inflight)) => {
                let t_sub = inflight.submitted_at;
                w.submitted_at = Some(t_sub);
                let out = inflight.finish(&w.ring, &w.traced).await;
                if out.is_ok() {
                    meta_txpass_phase_record_span(
                        MetaTxPassPhase::JournalRingWrite,
                        t_sub,
                        std::time::Instant::now(),
                        &w.traced,
                    );
                }
                out
            }
            Err(e) => Err(e),
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
        ring: &JournalRing,
        region: u32,
        len: u64,
    ) -> std::result::Result<super::journal_core::Admission, KvError> {
        let threshold = self.timeout_threshold;
        let mut parked_since: Option<std::time::Instant> = None;
        loop {
            if let Some(adm) = ring.try_admit(len, AdmissionClass::User) {
                return Ok(adm);
            }
            self.stalls.fetch_add(1, Ordering::Relaxed);
            if let Some(r) = self.appenders.as_ref().and_then(|a| a.region(region)) {
                r.stalls.fetch_add(1, Ordering::Relaxed);
            }
            let notified = ring.space_notified();
            if let Some(adm) = ring.try_admit(len, AdmissionClass::User) {
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

    /// **The §4.7 heap admission** (step 3b of the pass, under the union
    /// leaf write locks). Per member in queue order, per leaf its records
    /// land on: project where the leaf's log ends after everything already
    /// pending on it (frozen delta, open delta, this batch's earlier
    /// members) plus this member's bytes. Fits ⇒ the flush appends in
    /// place, nothing to promise. Overflows ⇒ the flush needs an SMO:
    /// a fold at-or-under the node's capacity is a net-zero COMPACTION
    /// (1 extent, transient — the old one returns at the barrier) admitted
    /// down to the compaction floor; above it a SPLIT (the greedy ¾-fill
    /// part count plus the rounding and cascade extents, plus a new root
    /// for a root leaf — the tree grows) admitted only above the whole
    /// reserve. The extents
    /// are PROMISED on the node ([`NodeDirty::promise`]) so the flush
    /// pass's claims are budget this admission set aside; the projection
    /// is deliberately conservative (a promise whose remainder fits in
    /// place releases at the append; the surplus of an over-promise
    /// releases at the SMO; interior cascades draw on the flush pass's
    /// half of the reserve). Returns the members refused `NoSpace` with the index they
    /// hold in `entries`, ascending — side-effect-free on the refusal
    /// counters (the caller finalizes: a refusal may earn one retry after
    /// a checkpoint cycle); promises granted to the survivors stay on their
    /// nodes across the caller's retry (the re-projection finds them
    /// already covered).
    fn admit_heap_locked(
        &self,
        entries: &[QueuedTx],
        leaves: &[Vec<Arc<CachedNode>>],
        lock_set: &[Arc<CachedNode>],
        guards: &mut [crate::sqz_sync::SqzRwLockWriteGuard<'_, NodeDirty>],
    ) -> Vec<(usize, KvError)> {
        let layout = self.cache.config().layout;
        let node_size = layout.node_size();
        // `lock_set` is addr-sorted and deduped (the union lock order) —
        // slot lookup is a binary search.
        let slot = |leaf: &Arc<CachedNode>| -> usize {
            lock_set
                .binary_search_by_key(&leaf.addr(), |n| n.addr())
                .expect("leaf is locked")
        };
        // FAST PATH (the shape every pass on a volume with room takes):
        // per leaf, the WHOLE batch's bytes projected at once — a leaf
        // that absorbs them all in place needs no per-member accounting.
        // Stack-buffered for the usual union widths: the pass's hot path
        // must not gain an allocation per batch.
        let mut totals_buf = [0usize; 32];
        let mut totals_heap: Vec<usize> = Vec::new();
        let totals: &mut [usize] = if lock_set.len() <= totals_buf.len() {
            &mut totals_buf[..lock_set.len()]
        } else {
            totals_heap.resize(lock_set.len(), 0);
            &mut totals_heap[..]
        };
        for (q, entry_leaves) in entries.iter().zip(leaves) {
            for ((_, r), leaf) in q.recs.iter().zip(entry_leaves) {
                totals[slot(leaf)] += r.record_ref().encoded_len();
            }
        }
        let any_overflow = totals
            .iter()
            .enumerate()
            .any(|(gi, t)| *t > 0 && guards[gi].projected_log_end(&layout, *t) > node_size);
        if !any_overflow {
            return Vec::new();
        }

        let reserve = self.alloc.reserve_extents();
        let compaction_floor = super::alloc_ext::compaction_floor_extents(reserve);
        // Per locked leaf: bytes this batch's ADMITTED members land on it
        // (the in-place projection's input; the records themselves are
        // re-derived from `entries` only when a walk needs them).
        let mut pending: Vec<usize> = vec![0; lock_set.len()];
        // Per member: (leaf slot, bytes) — small, deduped by slot.
        let mut member: Vec<(usize, usize)> = Vec::new();
        // Per member: the promise mutations made for its leaves so far —
        // `(leaf slot, extents promised, growth noted)` — undone when a
        // LATER leaf of the same member is refused (§4.7 P1: a promise
        // left on a leaf whose member never landed has no SMO to consume
        // it and understates claimable for ever).
        let mut taken: Vec<(usize, u64, usize)> = Vec::new();
        let mut refused: Vec<(usize, KvError)> = Vec::new();
        // The batch's records landing on leaf `gi`, members `..=upto`, in
        // apply order — the walk's pending input (refused members were
        // never applied and are skipped).
        let pending_records = |gi: usize, upto: usize, refused: &[(usize, KvError)]| {
            entries[..=upto]
                .iter()
                .zip(leaves)
                .enumerate()
                .filter(|(qi, _)| !refused.iter().any(|(r, _)| r == qi))
                .flat_map(|(_, (q, entry_leaves))| {
                    q.recs
                        .iter()
                        .zip(entry_leaves)
                        .filter(move |(_, leaf)| leaf.addr() == lock_set[gi].addr())
                        .map(|((_, r), _)| (&r.key[..], r.kind, r.record_ref().encoded_len()))
                })
                .collect::<Vec<(&[u8], RecordKind, usize)>>()
        };
        for (qi, (q, entry_leaves)) in entries.iter().zip(leaves).enumerate() {
            member.clear();
            taken.clear();
            for ((_, r), leaf) in q.recs.iter().zip(entry_leaves) {
                let gi = slot(leaf);
                let bytes = r.record_ref().encoded_len();
                match member.iter_mut().find(|(g, _)| *g == gi) {
                    Some((_, b)) => *b += bytes,
                    None => member.push((gi, bytes)),
                }
            }
            let mut verdict: Option<KvError> = None;
            let mut admitted_split = false;
            for &(gi, bytes) in &member {
                if guards[gi].projected_log_end(&layout, pending[gi] + bytes) <= node_size {
                    continue; // in-place append at the flush
                }
                let is_root = self
                    .tree_of_node(&lock_set[gi])
                    .map(|t| t.is_root_addr(lock_set[gi].addr()))
                    .unwrap_or(false);
                // A root leaf's split also mints a new root.
                let with_root = |need: u64| if need > 1 && is_root { need + 1 } else { need };
                // Cheap re-check on an already-promised node — never the
                // O(node) walk per commit on a hot leaf between two
                // flushes: a SPLIT promise carries a cascade extent, so
                // growth below the layout's window since the last exact
                // walk cannot need more; a COMPACTION promise (1) covers
                // growth while the walk's fold plus every byte since stays
                // within one node (no shadowing credited — an upper bound).
                let promised = guards[gi].promised();
                if promised > 0 {
                    let added = guards[gi].promise_added() + bytes;
                    let bound = guards[gi].promise_basis() + added;
                    let covered = if promised == 1 {
                        bound <= layout.fold_capacity()
                    } else {
                        added < layout.split_growth_window()
                    };
                    if covered {
                        guards[gi].note_promise_growth(bytes);
                        taken.push((gi, 0, bytes));
                        continue;
                    }
                }
                let mut extra = pending_records(gi, qi, &refused);
                // Tail 0: the admission never credits tombstone elision
                // (the conservative posture 862b8077 landed with).
                let (fold, parts) = lock_set[gi].snapshot().fold_bytes_upper_with(
                    &mut extra,
                    layout.split_part_capacity(),
                    0,
                );
                let need = with_root(layout.smo_extents_for_parts(fold, parts));
                let is_split = need > 1;
                let delta = need.saturating_sub(promised);
                if delta == 0 {
                    // Covered by an earlier member/attempt: the exact
                    // estimate becomes the new basis.
                    guards[gi].set_promise_basis(fold);
                    continue;
                }
                // A leaf of a LEASED slot draws its lessee's GRANT, never
                // the bitmap (§5.3.3): its promise rides the REGION's
                // ledger (`grant_promised` — the leaf's dirty half is
                // pointed at it, so the heap's claimable never carries a
                // leased SMO's extents); admitted while the grant's
                // headroom covers the SMO; short of it with the manager
                // unreachable the member refuses EAGAIN-class
                // (`GrantExhausted` — the appender's dependency, counted);
                // short of it with the manager live it is admitted — the
                // cadence refills before the flush pass needs the extent,
                // or that pass defers and counts the stall. The
                // reachability read is the test seam until PR 4's manager
                // lease renewal carries the live signal
                // (`ManagerLease::Peer` liveness) — its product successor.
                if let Some(region) = lock_set[gi]
                    .forest_slot()
                    .map(|s| self.region_of_slot(s))
                    .filter(|id| *id != 0)
                    .and_then(|id| self.appenders().and_then(|a| a.region(id)))
                {
                    let (headroom, ledger) = {
                        let g = region.grant();
                        (g.headroom(), g.promise_ledger())
                    };
                    if headroom < delta && super::appender::test_manager_unreachable() {
                        region.dependency_stalls.fetch_add(1, Ordering::Relaxed);
                        verdict = Some(KvError::GrantExhausted {
                            appender: region.id,
                            unclaimed: headroom,
                        });
                        break;
                    }
                    guards[gi].set_promise_ledger(ledger);
                    guards[gi].promise(delta, fold);
                    taken.push((gi, delta, 0));
                    admitted_split |= is_split;
                    continue;
                }
                let floor = if is_split { reserve } else { compaction_floor };
                let claimable = self
                    .alloc
                    .free_extents()
                    .saturating_sub(self.cache.heap_promised());
                if claimable.saturating_sub(delta) >= floor {
                    guards[gi].promise(delta, fold);
                    taken.push((gi, delta, 0));
                    admitted_split |= is_split;
                } else {
                    log::debug!(
                        "heap admission refused: tree {} leaf {:#x} projects a fold of {fold} B \
                         (capacity {}) needing {need} extent(s), {} promised; claimable {claimable} \
                         − {delta} < floor {floor} (free {}, ledger {}, pending-free {})",
                        lock_set[gi].tree_id(),
                        lock_set[gi].addr(),
                        layout.fold_capacity(),
                        guards[gi].promised(),
                        self.alloc.free_extents(),
                        self.cache.heap_promised(),
                        self.alloc.pending_count(),
                    );
                    verdict = Some(KvError::NoSpace {
                        free: claimable,
                        reserve: floor,
                    });
                    break;
                }
            }
            match verdict {
                None => {
                    for &(gi, bytes) in &member {
                        pending[gi] += bytes;
                    }
                    if admitted_split {
                        // A leaf was minted-in-budget for a member that
                        // LANDS: the growth floor is clear.
                        self.leave_heap_full("a growth commit was admitted a new leaf");
                    }
                }
                Some(e) => {
                    // The member never lands: give back every promise its
                    // earlier leaves drew and every growth they noted.
                    for &(gi, extents, grew) in &taken {
                        if extents > 0 {
                            guards[gi].retract_promise(extents);
                        }
                        if grew > 0 {
                            guards[gi].retract_promise_growth(grew);
                        }
                    }
                    refused.push((qi, e));
                }
            }
        }
        refused
    }

    /// Latch the §4.7 heap-full posture (`meta_kv_heap_full` = 1) — loud
    /// ONCE per transition, never terminal: growth needing a new leaf is
    /// ENOSPC from here; reads, deletes and in-place appends continue.
    pub(super) fn enter_heap_full(&self, why: &str) {
        if !self.heap_full.swap(true, Ordering::AcqRel) {
            log::warn!(
                "meta volume {}: metadata heap FULL — {why} (free={} claimable, {} promised to \
                 pending SMOs, reserve={}); growth needing a new leaf answers ENOSPC, reads and \
                 deletes continue, the volume is NOT failed (§4.7 space standstill class)",
                self.path.display(),
                self.alloc.free_extents(),
                self.cache.heap_promised(),
                self.alloc.reserve_extents(),
            );
        }
    }

    /// Clear the heap-full posture (loud once per transition).
    pub(super) fn leave_heap_full(&self, why: &str) {
        if self.heap_full.swap(false, Ordering::AcqRel) {
            log::info!(
                "meta volume {}: metadata heap no longer full — {why} (free={} claimable, {} \
                 promised, reserve={})",
                self.path.display(),
                self.alloc.free_extents(),
                self.cache.heap_promised(),
                self.alloc.reserve_extents(),
            );
        }
    }

    /// Whether the growth floor is clear right now: the SMALLEST split (a
    /// fold one byte over capacity — two parts, plus the rounding and
    /// cascade extents) could be promised above the reserve — the same arithmetic
    /// the admission runs, so the posture word and the refusals agree
    /// (the checkpoint cycle's re-check).
    pub(super) fn heap_growth_floor_clear(&self) -> bool {
        let layout = self.cache.config().layout;
        let min_split = layout.smo_extents_for_parts(layout.fold_capacity() + 1, 2);
        self.alloc
            .free_extents()
            .saturating_sub(self.cache.heap_promised())
            .saturating_sub(min_split)
            >= self.alloc.reserve_extents()
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
            // §4.7: the no-space class owes userspace ENOSPC (POSIX-6) —
            // flattening it to `Corrupt` read as EINVAL.
            KvError::NoSpace { free, reserve } => KvError::NoSpace {
                free: *free,
                reserve: *reserve,
            },
            other => KvError::Corrupt(other.to_string()),
        }
    }

    /// `eio` over an owned message (the batch paths format contexts).
    fn eio_str(&self, what: &str) -> KvError {
        KvError::Io(self.eio(what))
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
    ///
    /// `ring` is the ring the failed window reserved in: `lo`/`hi` are
    /// positions in it, and the compensation is journaled into IT —
    /// compensation is CONTENT of the window's appender region, and one
    /// key lives in one ring (KD-SYM-4); in the manager's ring it would
    /// be the `Lease` violation the next mount refuses on (review round
    /// 1, Issue 3). On a flat volume this is the one ring there is.
    async fn rollback_failed_tx(&self, lo: u64, hi: u64, undo: &[UndoKey], ring: &JournalRing) {
        // Phase 1: removal under re-acquired ascending locks.
        let mut comp: Vec<(u8, Vec<u8>, RecordKind, Bytes)> = Vec::new();
        for attempt in 0..COMMIT_RETRY_BUDGET {
            comp.clear();
            let mut leaves: Vec<Arc<CachedNode>> = Vec::with_capacity(undo.len());
            let mut resolve_failed = false;
            for u in undo {
                let tree = match self.tree_for_record(u.tree_id, &u.key).await {
                    Ok(t) => t,
                    Err(e) => {
                        log::error!("rollback: tree resolution failed for a dirty key: {e}");
                        resolve_failed = true;
                        break;
                    }
                };
                match tree.resolve_leaf(&u.key).await {
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
        if let Err(e) = self.commit_compensation(tx, ring).await {
            log::error!(
                "meta volume {}: rollback compensation failed ({e}) — volume escalating",
                self.path.display()
            );
            self.note_journal_failure();
        }
    }

    /// Commit a compensation tx through the checkpoint-class reserve of
    /// `ring` — the failed window's own (never parks behind user
    /// admissions; skip-if-newer re-checked under the locks).
    async fn commit_compensation(
        &self,
        tx: KvTx,
        ring: &JournalRing,
    ) -> std::result::Result<(), KvError> {
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
        // Compensation records re-stage the pass's ALREADY-FRAMED undo keys
        // (forest keys on a forest volume): the audit reads the legacy
        // form the per-kind decoders were written for.
        #[cfg(debug_assertions)]
        for (tree_id, r) in self.legacy_recs(&recs)?.iter() {
            super::node::debug_audit_records(*tree_id, 0, std::slice::from_ref(r));
        }
        let len = entry_len_for(&recs)?;
        let Some(adm) = ring.try_admit(len, AdmissionClass::Checkpoint) else {
            return Err(KvError::JournalReserveExhausted { needed: len });
        };
        for attempt in 0..COMMIT_RETRY_BUDGET {
            let mut leaves: Vec<Arc<CachedNode>> = Vec::with_capacity(recs.len());
            for (tree_id, r) in &recs {
                leaves.push(
                    self.tree_for_record(*tree_id, &r.key)
                        .await?
                        .resolve_leaf(&r.key)
                        .await?,
                );
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
                    ring.core().release(adm);
                    return Err(KvError::Corrupt(
                        "compensation retry budget exhausted".to_string(),
                    ));
                }
                continue;
            }
            let (res, seq_base) = ring.reserve_registered(adm);
            for (i, (_t, r)) in recs.iter_mut().enumerate() {
                r.seq = seq_base + i as u64;
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
            return ring.commit_entry(&res, &recs).await;
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
        for (k, v) in self.chain_scan(TREE_DENTRIES, &start, &end).await? {
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
        chain_kind: u8,
        start: &[u8],
        end: &[u8],
    ) -> std::result::Result<Vec<u8>, KvError> {
        let mut occ: std::collections::BTreeSet<u8> = self
            .chain_scan(chain_kind, start, end)
            .await?
            .iter()
            .map(|(k, _)| k[k.len() - 1])
            .collect();
        for (t, k, kind, _) in &tx.staged {
            if *t == chain_kind && k[..] >= *start && k[..] <= *end {
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
            .chain_occupancy(tx, TREE_DENTRIES, &start, &end)
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
        for (k, v) in self.chain_scan(TREE_XATTRS, &start, &end).await? {
            if XattrValue::decode(&v)?.name == name.as_bytes() {
                let mut key = [0u8; 16];
                key.copy_from_slice(&k);
                return Ok((true, key));
            }
        }
        let occupied = self.chain_occupancy(tx, TREE_XATTRS, &start, &end).await?;
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
        let items: Vec<(Ino, &[super::block_refs::BlockRefOp])> =
            inos.iter().map(|&ino| (ino, &[][..])).collect();
        self.destroy_inode_records(&items, true).await.map(|_| ())
    }

    /// RECLAIM-ATOMIC: [`Self::destroy_inodes`] with each ino's durable
    /// reference RELEASES riding the SAME journal entry as its record and
    /// xattr `Delete`s — the reclaim path's commit. A release can then
    /// never land without its destroy (the released-but-undestroyed
    /// corpse the ledger gate makes harmless) and a destroy can never
    /// land without its release (the inode gone, its references on the
    /// ledger forever — the corpse-sweep record's residual A). Every
    /// release is WITNESSED under the ino's exclusive 4a guard before the
    /// commit ([`super::block_refs::DestroyVerdict::Destroyed`]'s `held`),
    /// the RAM-decrement budget the caller frees under. A live or missing
    /// ino stages NOTHING — not even its releases — and answers `Skipped`.
    ///
    /// One entry: the caller packs `items` to [`super::journal::
    /// entry_payload_cap`] with [`Self::destroy_entry_bytes`] +
    /// [`Self::release_records_bytes`]; a single ino past the cap takes
    /// [`Self::destroy_inode_chunked`].
    pub async fn destroy_inodes_releasing(
        &self,
        items: &[(Ino, &[super::block_refs::BlockRefOp])],
    ) -> Result<Vec<super::block_refs::DestroyVerdict>> {
        self.destroy_inode_records(items, true).await
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
        let items: Vec<(Ino, &[super::block_refs::BlockRefOp])> =
            inos.iter().map(|&ino| (ino, &[][..])).collect();
        self.destroy_inode_records(&items, false).await.map(|_| ())
    }

    /// The journal payload bytes [`Self::destroy_inodes`] stages for
    /// `ino`: its inode `Delete` plus one `Delete` per xattr key (empty
    /// values), priced with the admission's own record framing
    /// ([`super::journal::record_frame_len`]) — the corpse sweep's chunk
    /// planner input, so a planned chunk fits ONE entry by the arithmetic
    /// the commit enforces rather than by a guessed constant. A live or
    /// missing ino prices as if destroyed (the destroy stages nothing for
    /// it — an over-estimate in the safe direction).
    pub async fn destroy_entry_bytes(&self, ino: Ino) -> Result<u64> {
        let mut bytes = self.staged_frame_len(TREE_INODES, 0);
        let start = xattr_key(ino, 0, 0);
        let end = xattr_key(ino, HASH56_MAX, u8::MAX);
        let mut cursor: Vec<u8> = start.to_vec();
        loop {
            let page = self
                .range_kind(TREE_XATTRS, &cursor, &end, SCAN_PAGE)
                .await?;
            let Some((last, _)) = page.last() else { break };
            cursor = key_successor(last);
            bytes += page.len() as u64 * self.staged_frame_len(TREE_XATTRS, 0);
        }
        Ok(bytes)
    }

    /// Every xattr key of `ino`, in tree order.
    async fn xattr_keys_of(&self, ino: Ino) -> std::result::Result<Vec<Vec<u8>>, KvError> {
        let start = xattr_key(ino, 0, 0);
        let end = xattr_key(ino, HASH56_MAX, u8::MAX);
        let mut cursor: Vec<u8> = start.to_vec();
        let mut keys = Vec::new();
        loop {
            let page = self
                .range_kind(TREE_XATTRS, &cursor, &end, SCAN_PAGE)
                .await?;
            let Some((last, _)) = page.last() else { break };
            cursor = key_successor(last);
            keys.extend(page.into_iter().map(|(k, _)| k.to_vec()));
        }
        Ok(keys)
    }

    /// The release WITNESS (the generic/749 ledger gate): which of the
    /// released references have a record RIGHT NOW — point lookups on the
    /// RAM-authoritative tree under the caller's exclusive 4a guard.
    /// `None` on a volume without the ledger.
    async fn witness_releases(
        &self,
        ops: &[super::block_refs::BlockRefOp],
    ) -> Result<Option<Vec<super::block_refs::BlockRef>>> {
        if !self.block_refs_engaged() {
            return Ok(None);
        }
        let mut held = Vec::new();
        for op in ops.iter().filter(|op| !op.take) {
            if self
                .lookup_kind(super::record::TREE_BLOCK_REFS, &op.reference.key())
                .await?
                .is_some()
            {
                held.push(op.reference);
            }
        }
        Ok(Some(held))
    }

    async fn destroy_inode_records(
        &self,
        items: &[(Ino, &[super::block_refs::BlockRefOp])],
        skip_live: bool,
    ) -> Result<Vec<super::block_refs::DestroyVerdict>> {
        use super::block_refs::DestroyVerdict;
        self.write_gate()?;
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.iter().any(|(_, ops)| ops.iter().any(|op| !op.take)) {
            Self::reclaim_release_seam_gate()?;
        }
        let lock_plan: Vec<(u64, LockMode)> = items
            .iter()
            .map(|&(ino, _)| (ino, LockMode::Exclusive))
            .collect();
        let guards: Arc<[DlmGuard]> = Arc::from(self.dlm.lock_many(&lock_plan, &[]).await);

        let mut tx = KvTx::new();
        let mut doomed = 0usize;
        let mut releases = 0usize;
        let mut verdicts = Vec::with_capacity(items.len());
        for &(ino, ops) in items {
            match self.read_inode_value(ino).await? {
                Some(v) if skip_live && v.nlink > 0 => {
                    log::debug!("destroy_inodes: ino {ino} has nlink {}, skipping", v.nlink);
                    verdicts.push(DestroyVerdict::Skipped);
                }
                Some(_) => {
                    doomed += 1;
                    // RECLAIM-ATOMIC: the ino's reference releases ride
                    // THIS entry (witnessed first, under the guard the
                    // commit holds). Silently skipped on a volume without
                    // the ledger — its ownership answers stay derived.
                    let held = self.witness_releases(ops).await?;
                    if self.block_refs_engaged() && !ops.is_empty() {
                        tx.stage_block_refs(ops);
                        releases += ops.iter().filter(|op| !op.take).count();
                    }
                    tx.stage_delete(TREE_INODES, inode_key(ino));
                    // Reap the corpse's xattrs in the SAME entry (§4.8).
                    for k in self.xattr_keys_of(ino).await? {
                        tx.stage_delete(TREE_XATTRS, k);
                    }
                    verdicts.push(DestroyVerdict::Destroyed { held });
                }
                // Missing or already destroyed: nothing to do — and its
                // releases stand (no record can justify a decrement).
                None => verdicts.push(DestroyVerdict::Skipped),
            }
        }
        if doomed == 0 {
            return Ok(verdicts);
        }
        crate::fuse_client::METRICS
            .meta_reclaim_batch_size
            .record(doomed);
        tx.hold_guards(guards.clone());
        self.commit_tx(tx).await?;
        if releases > 0 {
            crate::fuse_client::METRICS
                .reclaim_release_destroy_joint_commits
                .fetch_add(1, Ordering::Relaxed);
        }
        // POSIX-1: the live-inode gauge moves only on a COMMITTED destroy
        // (a failed commit leaves the records live, and the bisect retry
        // re-counts the halves it actually lands).
        self.destroyed_inodes
            .fetch_add(doomed as u64, Ordering::Relaxed);
        // PR M6: a destroyed corpse's pending times refinement is moot —
        // GC it under the exclusive locks (the drain would drop it on the
        // missing-inode read anyway; this keeps the map tight).
        for &(ino, _) in items {
            self.retire_pending_times(ino);
        }
        Ok(verdicts)
    }

    /// RECLAIM-ATOMIC residual B: destroy ONE ino whose releases + xattrs
    /// + record exceed the whole-entry cap, across several entries under
    /// one held exclusive 4a guard, in the order that keeps every
    /// committed prefix a corpse the next sweep converges on:
    ///
    /// 1. the reference releases (their RAM frees follow each commit —
    ///    the ledger says free, so RAM must);
    /// 2. every xattr but `layout` (inert to the reclaim);
    /// 3. the `layout` xattr and the inode record, LAST.
    ///
    /// A crash (or a failed entry) after step 1 or mid-step 2 leaves the
    /// record with its layout: the next sweep re-reads the layout, its
    /// releases witness NO record and are counted skipped, the remaining
    /// xattrs and the record destroy — no leak, no second free. Packed
    /// greedily to [`super::journal::entry_payload_cap`] in the admission's
    /// own framing; an ino that fits one entry commits one.
    pub async fn destroy_inode_chunked(
        &self,
        ino: Ino,
        refs: &[super::block_refs::BlockRefOp],
    ) -> Result<super::block_refs::ChunkedDestroy> {
        use super::block_refs::ChunkedDestroy;
        self.write_gate()?;
        if refs.iter().any(|op| !op.take) {
            Self::reclaim_release_seam_gate()?;
        }
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let skipped = ChunkedDestroy {
            skipped: true,
            completed: false,
            held: None,
            entries: 0,
            stopped: None,
        };
        match self.read_inode_value(ino).await? {
            Some(v) if v.nlink > 0 => {
                log::debug!(
                    "destroy_inode_chunked: ino {ino} has nlink {}, skipping",
                    v.nlink
                );
                return Ok(skipped);
            }
            Some(_) => {}
            None => return Ok(skipped),
        }
        // The witness, once, under the guard: per release op, whether its
        // record existed before THIS destroy touched anything.
        let witnessed: Option<Vec<Option<super::block_refs::BlockRef>>> =
            if !self.block_refs_engaged() {
                None
            } else {
                let mut w = Vec::with_capacity(refs.len());
                for op in refs {
                    w.push(if op.take {
                        None
                    } else {
                        self.lookup_kind(super::record::TREE_BLOCK_REFS, &op.reference.key())
                            .await?
                            .map(|_| op.reference)
                    });
                }
                Some(w)
            };
        let layout_key = {
            let probe = KvTx::empty();
            let (present, key) = self.xattr_slot(&probe, ino, "layout").await?;
            present.then_some(key.to_vec())
        };
        let other_xattrs: Vec<Vec<u8>> = self
            .xattr_keys_of(ino)
            .await?
            .into_iter()
            .filter(|k| layout_key.as_ref() != Some(k))
            .collect();

        enum Rec<'a> {
            Release(&'a super::block_refs::BlockRefOp),
            Xattr(Vec<u8>),
            Inode,
        }
        let cap = super::journal::entry_payload_cap();
        let ledger = self.block_refs_engaged();
        let mut records: Vec<(Rec<'_>, u64)> = Vec::new();
        if ledger {
            let release = self.staged_frame_len(super::record::TREE_BLOCK_REFS, 0);
            records.extend(refs.iter().map(|op| (Rec::Release(op), release)));
        }
        let xattr = self.staged_frame_len(TREE_XATTRS, 0);
        records.extend(
            other_xattrs
                .into_iter()
                .chain(layout_key)
                .map(|k| (Rec::Xattr(k), xattr)),
        );
        records.push((Rec::Inode, self.staged_frame_len(TREE_INODES, 0)));

        let stop_after = {
            let seam = TEST_DESTROY_CHUNK_STOP_AFTER.load(Ordering::Relaxed);
            if seam != 0 {
                seam
            } else {
                crate::env_knobs::int_knob("SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER", 0u32)
            }
        };
        let mut out = ChunkedDestroy {
            skipped: false,
            completed: false,
            held: ledger.then(Vec::new),
            entries: 0,
            stopped: None,
        };
        let mut tx = KvTx::new();
        let mut tx_bytes = 0u64;
        let mut staged_releases = 0usize;
        let mut committed_releases = 0usize;
        let total = records.len();
        for (i, (rec, len)) in records.into_iter().enumerate() {
            if tx_bytes > 0 && tx_bytes + len > cap {
                tx.hold_guards(Arc::clone(&guards));
                if let Err(e) = self
                    .commit_tx(std::mem::replace(&mut tx, KvTx::new()))
                    .await
                {
                    out.stopped = Some(e.to_string());
                    break;
                }
                out.entries += 1;
                committed_releases = staged_releases;
                tx_bytes = 0;
                if stop_after != 0 && out.entries >= stop_after {
                    out.stopped = Some(format!(
                        "stopped after {} entr(ies) by SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER \
                         (test seam)",
                        out.entries
                    ));
                    break;
                }
            }
            match rec {
                Rec::Release(op) => {
                    tx.stage_block_refs(std::slice::from_ref(op));
                    staged_releases += 1;
                }
                Rec::Xattr(k) => tx.stage_delete(TREE_XATTRS, k),
                Rec::Inode => tx.stage_delete(TREE_INODES, inode_key(ino)),
            }
            tx_bytes += len;
            if i + 1 == total {
                tx.hold_guards(Arc::clone(&guards));
                match self
                    .commit_tx(std::mem::replace(&mut tx, KvTx::new()))
                    .await
                {
                    Ok(()) => {
                        out.entries += 1;
                        committed_releases = staged_releases;
                        out.completed = true;
                    }
                    Err(e) => out.stopped = Some(e.to_string()),
                }
            }
        }
        if let (Some(held), Some(w)) = (out.held.as_mut(), witnessed.as_ref()) {
            held.extend(w.iter().take(committed_releases).filter_map(|r| *r));
        }
        if out.completed {
            crate::fuse_client::METRICS
                .meta_reclaim_batch_size
                .record(1);
            self.destroyed_inodes.fetch_add(1, Ordering::Relaxed);
            self.retire_pending_times(ino);
        }
        if out.entries > 1 || (out.entries == 1 && !out.completed) {
            crate::fuse_client::METRICS
                .reclaim_single_ino_chunked_destroys
                .fetch_add(1, Ordering::Relaxed);
        }
        if committed_releases > 0 {
            crate::fuse_client::METRICS
                .reclaim_release_destroy_joint_commits
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(out)
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
        Ok(!self
            .range_kind(TREE_DENTRIES, &start, &end, 1)
            .await?
            .is_empty())
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
        let tx = self
            .stage_layout_and_size(ino, layout, size, block_refs)
            .await?;
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// The STAGE half of [`Self::set_layout_and_size`] (D-1c): the write
    /// gate, the ino's own exclusive 4a guard, and the staged two-record
    /// transaction (layout Put + inode Put + accounting) — everything up
    /// to and excluding the commit. The returned tx co-owns the guard;
    /// commit it alone (`set_layout_and_size` does) or as a member of a
    /// [`Self::commit_tx_group`].
    pub async fn stage_layout_and_size(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
    ) -> Result<KvTx> {
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.stage_layout_and_size_with_map_holding(ino, layout, size, block_refs, &[], guards)
            .await
    }

    /// [`Self::stage_layout_and_size`] under a CALLER-HELD guard set — the
    /// group path's form (D-1c): a set of publishes takes ONE canonical
    /// `lock_many` over its members' inos (two distinct inos may share a
    /// DLM stripe; per-member guards taken while siblings hold theirs
    /// would self-deadlock on a collision — `dlm.rs`'s acquisition law),
    /// stages every member under that shared set, then commits the set
    /// as one group. Callers own the write gate's timing only insofar as
    /// it is re-checked here (a fenced volume refuses to stage).
    pub async fn stage_layout_and_size_holding(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        guards: Arc<[DlmGuard]>,
    ) -> Result<KvTx> {
        self.write_gate()?;
        self.stage_layout_and_size_with_map_holding(ino, layout, size, block_refs, &[], guards)
            .await
    }

    /// [`Self::set_layout_and_size`] carrying **block-map operations** in
    /// the SAME transaction — the design §3 publish/head-flip tx shape
    /// (layout head Put + inode Put + map records + BlockRefOps = one
    /// conveyor pass = one journal entry), which PR 2's crossing and
    /// spill switch consume. PR 1 has no production caller with a
    /// non-empty `block_map`; the contract tests pin the one-tx
    /// atomicity (journal-entry equality vs an un-stamped volume).
    ///
    /// Unlike `block_refs` (accounting the derived walk can rebuild —
    /// silently skipped on an un-stamped volume), map records ARE the
    /// mapping: non-empty ops on a volume without incompat bit 16 REFUSE
    /// loud rather than silently losing where the data lives.
    pub async fn set_layout_and_size_with_map(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        block_map: &[super::block_map::BlockMapOp],
    ) -> Result<()> {
        if !block_map.is_empty() && !self.block_map_tree_engaged() {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "block-map records staged on meta volume {} which does not carry \
                 incompat bit 16 (KV_BLOCK_MAP_TREE): map records ARE the mapping, so \
                 dropping them silently would lose data — stamp the bit before the \
                 first record (the PR 2 crossing's own ordering law)",
                self.path.display()
            )));
        }
        self.write_gate()?;
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        self.set_layout_and_size_with_map_holding(ino, layout, size, block_refs, block_map, guards)
            .await
    }

    /// [`Self::set_layout_and_size_with_map`] with the ino's 4a guard
    /// ALREADY HELD (the `routed_*` guards-parameter precedent): the PR 2
    /// crossing train holds ONE exclusive I-guard across sweep → chunks →
    /// flip (design A3), and the DLM stripes are non-reentrant, so the
    /// flip transaction cannot re-acquire. Callers own `write_gate` and
    /// the un-engaged-map refusal.
    async fn set_layout_and_size_with_map_holding(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        block_map: &[super::block_map::BlockMapOp],
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        let tx = self
            .stage_layout_and_size_with_map_holding(
                ino, layout, size, block_refs, block_map, guards,
            )
            .await?;
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// The stage half of [`Self::set_layout_and_size_with_map_holding`]:
    /// the two-record transaction (layout Put + inode Put + the accounting
    /// and map records that ride it), holding `guards`, NOT yet committed.
    async fn stage_layout_and_size_with_map_holding(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[super::block_refs::BlockRefOp],
        block_map: &[super::block_map::BlockMapOp],
        guards: Arc<[DlmGuard]>,
    ) -> Result<KvTx> {
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
        if self.block_refs_engaged() {
            tx.stage_block_refs(block_refs);
        }
        // Design §3: the map records ride THIS tx too (no second
        // commit). The un-engaged case refused loud above.
        tx.stage_block_map(block_map)?;
        tx.hold_guards(guards);
        Ok(tx)
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
        self.commit_block_refs_inner(ino, ops, false)
            .await
            .map(|_held| ())
    }

    /// [`Self::commit_block_refs`] that WITNESSES the releases — `None` on
    /// a volume without the ledger (the derived posture), `Some(held)`
    /// otherwise, where `held` is every released reference whose record
    /// EXISTED at commit time (the reclaim path's RAM-decrement gate —
    /// [`super::block_refs::ReleaseWitness`]). The existence probes are
    /// point lookups on the RAM-authoritative tree under the same 4a
    /// I-guard the commit holds, so no concurrent commit of this ino's
    /// ownership set can slip between the probe and the `Delete`.
    pub async fn commit_block_refs_witnessed(
        &self,
        ino: Ino,
        ops: &[super::block_refs::BlockRefOp],
    ) -> Result<Option<Vec<super::block_refs::BlockRef>>> {
        self.commit_block_refs_inner(ino, ops, true).await
    }

    /// [`TEST_FAIL_RECLAIM_RELEASE`]: refuse the commit that would carry a
    /// reclaimed ino's reference releases.
    fn reclaim_release_seam_gate() -> Result<()> {
        if TEST_FAIL_RECLAIM_RELEASE.load(Ordering::Relaxed) {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "reclaim release commit refused by TEST_FAIL_RECLAIM_RELEASE (test seam)"
                    .to_string(),
            ));
        }
        Ok(())
    }

    async fn commit_block_refs_inner(
        &self,
        ino: Ino,
        ops: &[super::block_refs::BlockRefOp],
        witness: bool,
    ) -> Result<Option<Vec<super::block_refs::BlockRef>>> {
        if !self.block_refs_engaged() {
            return Ok(None);
        }
        if ops.is_empty() {
            return Ok(Some(Vec::new()));
        }
        self.write_gate()?;
        if witness && ops.iter().any(|op| !op.take) {
            Self::reclaim_release_seam_gate()?;
        }
        // The ino's I-guard: the records belong to this ino's ownership
        // set, so the same 4a lock that serializes its layout commits
        // serializes their release (lock order unchanged — 4a before 4b,
        // which `commit_tx` takes).
        let guards: Arc<[DlmGuard]> = Arc::from(vec![self.dlm.lock_inode_exclusive(ino).await]);
        let mut held = Vec::new();
        if witness {
            for op in ops.iter().filter(|op| !op.take) {
                if self
                    .lookup_kind(super::record::TREE_BLOCK_REFS, &op.reference.key())
                    .await?
                    .is_some()
                {
                    held.push(op.reference);
                }
            }
        }
        let mut tx = KvTx::new();
        tx.stage_block_refs(ops);
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(Some(held))
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
            .map(|(used, _version, _recomputed)| used)
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
        self.merge_layout_and_size_chained_accounted(
            ino,
            refs_owner,
            delta,
            full_layout,
            size,
            block_refs,
        )
        .await
        .map(|(used, version, _recomputed)| (used, version))
    }

    /// [`Self::merge_layout_and_size_chained`] surfacing the finding-36
    /// owner-recompute verdict (see [`RecomputedReleases`]) — the S9
    /// publish serve consumes the released set (its post-commit free
    /// ladder) and the reply's `recomputed` flag; every other caller drops
    /// it, keeping the pre-fix shape verbatim.
    pub async fn merge_layout_and_size_chained_accounted(
        &self,
        ino: Ino,
        refs_owner: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: Bytes,
        size: u64,
        block_refs: Vec<super::block_refs::BlockRefOp>,
    ) -> Result<MergeOutcome> {
        self.merge_layout_and_size_ext(ino, refs_owner, delta, full_layout, size, block_refs, true)
            .await
    }

    /// PR 5a (design §11 row 3): does this stored layout record carry a
    /// `kvmap:` head sentinel? Probed from the head's own `block_map_id`
    /// (the KVMAP_HEAD_PREFIX law) — NEVER from the fold error text: the
    /// rung-19 `head_indirect` gates key on the WORD "indirect" in the
    /// base-decode error, and a kvmap head's refusal deliberately lacks
    /// it, which is exactly how a chained merge reached `use_delta` and
    /// staged a delta onto the delta-ineligible kvmap base.
    fn stored_layout_is_kvmap_head(cur: &[u8]) -> bool {
        XattrValue::decode(cur).is_ok_and(|x| {
            crate::layout_wire::decode_layout_any(&x.value).is_ok_and(|l| {
                l.block_map_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(super::block_map::KVMAP_HEAD_PREFIX))
            })
        })
    }

    /// PR 5a (design §11 row 3): the chained-merge-onto-a-kvmap-base
    /// refusal, shared by the direct arm and the aggregated pass.
    /// Retried-class ("layout delta base unusable" — the shipper's
    /// error arm resets its RAM provenance, so the retry refetches the
    /// kvmap head and re-ships the A4 crossing train), staged NOTHING.
    fn kvmap_chain_refusal(ino: Ino) -> crate::error::SqueezefsError {
        crate::error::SqueezefsError::InvalidOperation(format!(
            "layout delta base unusable: kvmap base — ino {ino}'s durable head lives in \
             the block-map tree; a chained delta staged onto it poisons every later fold \
             and a partial full Put would clobber it. The chained-merge ∘ kvmap compose \
             lands in PR 5b — refetch and recompose (the reset provenance re-reads the \
             kvmap head and the save re-ships the crossing train)"
        ))
    }

    /// Finding 36: the DATA-block references a recomputed op set RELEASES
    /// — the device frees the publish serve owns post-commit. Map-blob
    /// releases are excluded (their free is the displaced-blob
    /// post-commit arm's, never the block ladder's).
    fn released_data_refs(
        ops: &[super::block_refs::BlockRefOp],
    ) -> Vec<super::block_refs::BlockRef> {
        ops.iter()
            .filter(|o| !o.take && !o.reference.is_map_blob())
            .map(|o| o.reference)
            .collect()
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
    ) -> Result<MergeOutcome> {
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
        // Finding 38: the weight is the member's ENTRY contribution — the
        // refs are journal bytes too (46 B/op measured), and a weight
        // that ignored them let the group's one-entry transaction blow
        // the whole-entry cap.
        let weight = (delta_wire.len() + full_layout.len() + block_refs.len() * 46 + 256) as u64;
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
            // Finding 38: this conveyor's batch is ONE KvTx = ONE journal
            // entry (unlike the M7 conveyor's N entries), so its byte
            // bound is the whole-entry cap with headroom for the shared
            // framing — the ring-scale `batch_max_bytes` let a group
            // compose an entry no journal could ever admit.
            let batch = conveyor.drain(cap, super::journal::MAX_ENTRY_LEN / 2);
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
            squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<MergeOutcome>>,
            bool,
            u64,
            Option<super::indirect_map::IndirectBlobGuard>,
            RecomputedReleases,
        )> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut failed: Vec<(
            squeezefs_ipc::sqz_channel::oneshot::Sender<crate::error::Result<MergeOutcome>>,
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
            // Finding 36: whether this member's accounting was RECOMPUTED
            // (rung 19/20 replaced the caller's frame) — the released
            // set's device frees then belong to the serve's post-commit
            // ladder, never to the caller's frame-derived stream.
            let mut refs_recomputed = false;
            // Finding 15: the recompute's RAM-only releases (staged
            // nowhere; freed with the released set).
            let mut ram_only_releases: Vec<super::block_refs::BlockRef> = Vec::new();
            if existing {
                match self.lookup_kind(TREE_XATTRS, &key).await {
                    Ok(Some(cur)) => {
                        let base_ok = XattrValue::decode(&cur)
                            .map(|x| !x.value.starts_with(b"{"))
                            .unwrap_or(false);
                        // PR 5a (design §11 row 3 — the aggregated twin
                        // of the direct arm's screen, see there): a
                        // chained member on a `kvmap:` base refuses
                        // BEFORE any staging. No memo probe needed: kvmap
                        // trains never ride this pass, so a batch-prior
                        // mate cannot have staged a kvmap head the lookup
                        // misses (the composed_heads memo is indirect-
                        // compose state only).
                        if op.chain && base_ok && Self::stored_layout_is_kvmap_head(&cur) {
                            failed.push((op.done, Self::kvmap_chain_refusal(op.ino)));
                            continue;
                        }
                        // DUR-8b (aggregated twin of the direct path):
                        // the cap must bound the DURABLE chain, not the
                        // caller's RAM counter, which a metadata-cache
                        // refill resets to 0. An earlier member of THIS
                        // pass already moved the ino's head: its staged
                        // state wins over the committed probe.
                        let max_chain = crate::routing::layout_delta_max_chain();
                        let (depth, head_versions) = match batch_heads.get(&op.ino) {
                            Some(&h) => h,
                            None => match self.delta_chain_probe_kind(TREE_XATTRS, &key).await {
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
                            let (mut composed_refs, composed_ram_only) = match self
                                .block_refs_engaged()
                                .then(|| {
                                    state.layout.block_map.as_ref().and_then(|memo_map| {
                                        Self::recompute_refs_against_map(
                                            memo_map,
                                            &d.entries,
                                            op.refs_owner,
                                            &op.block_refs,
                                        )
                                    })
                                })
                                .flatten()
                            {
                                Some(frame) => (Some(frame.ops), frame.ram_only_releases),
                                None => (None, Vec::new()),
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
                                refs_recomputed = true;
                                ram_only_releases = composed_ram_only;
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
                        if op.chain && self.block_refs_engaged() && !head_indirect {
                            if let Ok(d) = crate::layout_wire::LayoutDelta::decode(&op.delta_wire) {
                                if let Some(frame) = Self::recompute_chained_refs(
                                    &cur,
                                    &d.entries,
                                    op.refs_owner,
                                    &op.block_refs,
                                ) {
                                    op.block_refs = frame.ops;
                                    refs_recomputed = true;
                                    ram_only_releases = frame.ram_only_releases;
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
            if self.block_refs_engaged() {
                tx.stage_block_refs(&op.block_refs);
            }
            // Finding 36: the recompute verdict travels with the member's
            // outcome — released DATA refs only (the map-blob transfers
            // free through `displaced_blobs` below, never the ladder) plus
            // the frame's RAM-only lifetimes (finding 15).
            let recomputed = refs_recomputed.then(|| {
                let mut released = Self::released_data_refs(&op.block_refs);
                released.append(&mut ram_only_releases);
                released
            });
            staged.push((
                op.done,
                use_delta,
                staged_version,
                member_blob_guard,
                recomputed,
            ));
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
                for (done, use_delta, staged_version, blob_guard, recomputed) in staged {
                    // Rung 20: the batch commit named this member's fresh
                    // blob — custody transfers.
                    if let Some(mut g) = blob_guard {
                        g.disarm();
                    }
                    let _ = done.send(Ok((use_delta, staged_version, recomputed)));
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
                for (done, _, _, _blob_guard, _) in staged {
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
    ) -> Option<RecomputedFrame> {
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
    ///
    /// Finding 15 (the co-writer supply leak): the frame's RAM-only
    /// lifetimes travel back beside the ops
    /// ([`RecomputedFrame::ram_only_releases`]) — a block the caller took
    /// AND released inside this one frame that neither the head nor the
    /// composed view names was never durably referenced, so the diff
    /// cannot release it and the owner's post-commit ladder must free it
    /// itself. The head/composed check is what keeps a skewed frame's
    /// re-take of a durable block (whose release the diff already carries)
    /// out of the set; it resolves the head's keys only when a candidate
    /// exists (the steady-state frame has none).
    fn recompute_refs_against_map(
        head: &std::collections::HashMap<u32, String>,
        entries: &[(u32, String)],
        ino: Ino,
        caller: &[super::block_refs::BlockRefOp],
    ) -> Option<RecomputedFrame> {
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
        let mut ram_only_releases = super::block_refs::frame_ram_only_candidates(caller, &out);
        if !ram_only_releases.is_empty() {
            // Every block the head or the composed view names, resolved
            // once: the composed view is `head` under `view`'s overrides.
            let mut named: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
            let mut name = |key: &str, idx: u32| {
                if let Some(r) = resolver(key, ino, idx) {
                    named.insert((r.vol_tag, r.block_idx));
                }
            };
            for (idx, key) in head {
                name(key, *idx);
            }
            for (idx, key) in &view {
                name(key, *idx);
            }
            ram_only_releases.retain(|r| !named.contains(&(r.vol_tag, r.block_idx)));
        }
        out.extend(caller.iter().filter(|o| o.reference.is_map_blob()).copied());
        Some(RecomputedFrame {
            ops: out,
            ram_only_releases,
        })
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
    ) -> Result<MergeOutcome> {
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
        // Finding 15: the recompute's RAM-only releases (staged nowhere;
        // freed with the released set).
        let mut ram_only_releases: Vec<super::block_refs::BlockRef> = Vec::new();
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
            if let Some(cur) = self.lookup_kind(TREE_XATTRS, &key).await? {
                let base_ok = XattrValue::decode(&cur)
                    .map(|x| !x.value.starts_with(b"{"))
                    .unwrap_or(false);
                // PR 5a (design §11 row 3): a `kvmap:` base is bincode-
                // decodable (`base_ok`), and the rung-19 indirect gate
                // below matches only the WORD "indirect" in the decode
                // error — so a chained merge reached `use_delta` and
                // staged a versioned delta ONTO the delta-ineligible
                // kvmap base (delayed read-side poison: every later fold
                // of the key refuses). Refuse BEFORE any staging.
                if chain && base_ok && Self::stored_layout_is_kvmap_head(&cur) {
                    return Err(Self::kvmap_chain_refusal(ino));
                }
                // DUR-8b: the cap must bound the DURABLE chain. The
                // caller's `layout_delta_chain` is a RAM counter that a
                // metadata-cache refill resets to 0, so before this probe
                // the on-disk chain was bounded only by node compaction —
                // not by `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN`. One extra
                // leaf resolve on the publish path, no record decodes.
                let max_chain = crate::routing::layout_delta_max_chain();
                let (depth, head_versions) = self.delta_chain_probe_kind(TREE_XATTRS, &key).await?;
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
                    let (mut composed_refs, composed_ram_only) = match self
                        .block_refs_engaged()
                        .then(|| {
                            Self::recompute_refs_against_map(
                                &full_map,
                                &delta.entries,
                                refs_owner,
                                block_refs,
                            )
                        })
                        .flatten()
                    {
                        Some(frame) => (Some(frame.ops), frame.ram_only_releases),
                        None => (None, Vec::new()),
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
                    ram_only_releases = composed_ram_only;
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
                if chain && self.block_refs_engaged() && !head_indirect {
                    if let Some(frame) =
                        Self::recompute_chained_refs(&cur, &delta.entries, refs_owner, block_refs)
                    {
                        refs_override = Some(frame.ops);
                        ram_only_releases = frame.ram_only_releases;
                    }
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
        if self.block_refs_engaged() {
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
        // Finding 36: `refs_override` is `Some` exactly when the staged
        // accounting was RECOMPUTED (rung 19/20) — the released set's
        // device frees belong to the serve's post-commit ladder, the
        // frame's RAM-only lifetimes included (finding 15).
        let recomputed = refs_override.as_deref().map(|ops| {
            let mut released = Self::released_data_refs(ops);
            released.append(&mut ram_only_releases);
            released
        });
        Ok((use_delta, staged_version, recomputed))
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
        let (_depth, head) = self.delta_chain_probe_kind(TREE_XATTRS, &key).await?;
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
            let page = self
                .range_kind(TREE_XATTRS, &cursor, &end, SCAN_PAGE)
                .await?;
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
