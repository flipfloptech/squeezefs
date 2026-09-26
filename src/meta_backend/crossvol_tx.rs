//! **DLM stage S3.5 — cross-volume transaction machinery** (execution-plan
//! ruling **D4**, spec item **TX-1**), and the fix for the P0 durability
//! bug **DUR-7**. Normative description: `docs/design-cow-kv-metadata.md`
//! §4.10a.
//!
//! # The problem
//!
//! One tx = one checksummed journal entry is a **per-volume** guarantee
//! (§4.10). A metadata set with more than one volume therefore composed
//! `link`, `unlink`/`rmdir` and cross-parent `rename` out of two (or more)
//! independent commits — `nlink` on one volume, the dentry on another —
//! with no intent record, no compensation and no crash recovery. A crash
//! between them left a state that is *self-consistent per volume* and
//! therefore invisible to fsck's C1–C6 classes: a positive `nlink` with no
//! dentry is exactly what a hard link looks like from the child volume's
//! side. `reclaim_orphaned_batch` skips `nlink > 0` by contract, so the
//! inode and every block it named leaked permanently.
//!
//! # The protocol
//!
//! Two-phase commit with a deterministic **same-process coordinator** (the
//! D0 claim holder) over volumes it already exclusively owns — NOT
//! distributed consensus. A transaction is an ordered **plan** of steps,
//! each homed on the volume that owns its object:
//!
//! ```text
//! 1. plan            under the op's ALREADY-HELD 4a guards; every
//!                    validation happens here, and every count step
//!                    records its (pre, post) witness
//! 2. step 0 + intent ONE whole-tx entry on the coordinator (= step 0's
//!                    volume): the intent can never disagree with the
//!                    first effect, and it costs no extra entry
//! 3. barrier         the coordinator volume (cross-DEVICE ordering: the
//!                    intent must be durable before any later step is
//!                    submitted, and different volumes are different
//!                    devices with no mutual ordering)
//! 4. steps 1..k      each its own whole-tx entry on its own volume
//! 5. barrier         every participant volume touched by steps 1..k
//! 6. retire          Delete the intent record on the coordinator
//! ```
//!
//! Recovery at **mount, before the mount serves** ([`recover_open_intents`],
//! driven from `open_routed_meta_set`) scans the reserved-ino intent range
//! on every volume — bounded, and empty on a healthy set — and **rolls
//! every open intent FORWARD** through the same applier the live path uses.
//! There is no second durability mechanism: an intent is an ordinary typed
//! KV record inside an ordinary whole-tx journal entry, so its durability,
//! torn-write immunity and replay are the format's, verbatim.
//!
//! # Why roll-forward, and why every step is idempotent
//!
//! Roll-forward is right because the *first* commit is the one the caller
//! was told about (the name disappearing on `unlink`, the link count on
//! `link`), and rolling it back after an ack would be a lie. It is *sound*
//! because every step kind is idempotent under a witness carried in the
//! record — checked by the ONE applier
//! ([`super::kv::backend::KvMetaBackend::xv_apply_step`]) that both the
//! live path and recovery call, so "idempotent" is a property of one code
//! path rather than a claim about two:
//!
//! | Step | Witness | Already-applied | Foreign |
//! |---|---|---|---|
//! | [`XvStep::RemoveDentry`] | the dentry names `expect_child` | absent | names someone else ⇒ skip loud |
//! | [`XvStep::InsertDentry`] | the name is free | present ⇒ `child` | present ⇒ other ⇒ skip loud |
//! | [`XvStep::SetNlink`] | `nlink == pre` (CAS) | `nlink == post` | neither ⇒ skip loud |
//! | [`XvStep::TouchCtime`] | — (monotone Δctime) | — | missing inode ⇒ skip |
//! | [`XvStep::MintInode`] | the inode record is absent | present | — |
//!
//! Compensation is therefore **never needed at recovery**: there is no
//! deterministic refusal left to hit, because all validation ran before the
//! intent existed and every count step carries an absolute post-image
//! instead of a delta. The one place the asymmetry survives is a *live*
//! mid-plan failure (a device error on a later participant): the op returns
//! the error, the intent stays, the coordinator + participant are latched
//! into the `disabled_volumes` fail-stop lattice so nothing further can
//! touch the objects, and the next mount completes the transaction.
//!
//! # Reuse (S8) — and the CROSS-OWNER generalization (symmetric PR 6)
//!
//! [`execute`] is the whole entry point: build an [`XvPlan`] of steps over
//! GLOBAL inos, hand it the op's guard set, done. Under the armed
//! symmetric plane (`docs/design-symmetric-metadata.md` §5.6, D18
//! reversed) the participant is a SLOT HOLDER, not a volume: a step homed
//! on a slot another appender leases travels to that appender as the S8
//! verb `MetaCall::XvStep` — the remote leg replaces the per-volume call
//! inside `apply_or_ship_step` and NOTHING ELSE changes: the record
//! format is the same (steps carry global inos, so the participant is
//! resolved by routing at apply time — a slot handover between crash and
//! recovery is handled for free), and the served side is the SAME
//! `xv_apply_step` under the op's travelling guards, so idempotence stays
//! a property of one code path. The six-step ladder as built:
//!
//! ```text
//! 1. plan            under the op's 4a guards — foreign-home guards
//!                    TRAVEL ([`acquire_guards_leased`]: one canonical
//!                    `lock_many` per table, tables in ascending
//!                    appender-id order; a foreign holder parks the
//!                    initiator's guards under the op's scope)
//! 2. tx0             ONE entry in the INITIATOR's ring: step 0's records
//!                    + the intent when step 0 is local (the intent homed
//!                    in that step's slot); the intent ALONE first — its
//!                    own entry in the mount's rotor slot — when step 0 is
//!                    another appender's
//! 3. barrier         the initiator's ring
//! 4. steps           in plan order: own slot ⇒ the local applier; foreign
//!                    ⇒ shipped to the holder under the scope (tree 0 + the
//!                    endpoint table; the holder applies without taking a
//!                    guard; the reply follows its durability lane)
//! 5. barrier         every LOCAL participant volume
//! 6. retire          Delete the intent (initiator's ring); the guard
//!                    set's drop releases every travelled scope
//! ```
//!
//! A witness refusal on a LIVE shipped step (the object moved at the
//! holder between the plan's read and the apply — reachable only through
//! a stale foreign read, the S5 projection until PR 5's tokens) is the
//! op's own errno, never a fail-stop: the plan stops at the refusal and
//! the initiator's applied halves are COMPENSATED under the still-held
//! guards (`compensate_live_refusal`: a create's minted child destroyed,
//! a link's raised count restored, a rename's removed source name
//! re-inserted).
//!
//! **A ship that fails leaves the intent OPEN, never fail-stops.** The
//! S3.5 lattice latch exists because a local mid-plan device error
//! violates the witnesses' premise; a holder that is down does not — its
//! step is idempotent and its slot's next holder serves it — so the op
//! returns the error, the intent stays, and the roll-forward cadence
//! ([`roll_forward_open_intents`], the S3.5 recovery over the shipped
//! applier) completes it; an intent no holder serves past the grace
//! window is `xv_cross_owner_intents_stuck` (must-stay-0).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use super::kv::backend::{KvMetaBackend, RoutedParentUpdate};
use super::kv::record::{xattr_key, InodeValue, HASH56_MAX, XATTR_KEY_LEN};
use super::{dlm, Ino, RoutedMetaBackend};
use crate::error::{Result, SqueezefsError};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Counters (stats inode: `crossvol_tx_*`)
// ---------------------------------------------------------------------------

/// Cross-volume transactions started (an intent record was written).
pub static XV_TX_STARTED: AtomicU64 = AtomicU64::new(0);
/// Cross-volume transactions that completed and retired their intent on
/// the live path.
pub static XV_TX_COMPLETED: AtomicU64 = AtomicU64::new(0);
/// Open intents rolled forward by mount recovery. Nonzero means a crash
/// interrupted a cross-volume transaction — the machinery working.
pub static XV_TX_RECOVERED: AtomicU64 = AtomicU64::new(0);
/// Steps whose effect this applier committed.
pub static XV_STEPS_APPLIED: AtomicU64 = AtomicU64::new(0);
/// Steps found already applied (recovery's idempotence, counted).
pub static XV_STEPS_ALREADY_APPLIED: AtomicU64 = AtomicU64::new(0);
/// Steps skipped because their object moved under the intent — **must
/// stay 0**: unreachable in production (an intent is only ever observed
/// by a mount whose transaction never completed, and a live mid-plan
/// failure fail-stops the volume). Nonzero ⇒ investigate alongside
/// `crossvol_tx_midplan_escalations`.
pub static XV_STEPS_FOREIGN_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Live mid-plan failures that latched the fail-stop lattice — **must
/// stay 0** on a healthy mount.
pub static XV_MIDPLAN_ESCALATIONS: AtomicU64 = AtomicU64::new(0);

/// Test seam (the `uring_fs::arm_*` / `TEST_CONVEYOR_HOLD_STAGE`
/// precedent — a commit-boundary "crash" cannot be an env knob because a
/// suite arms and disarms it per case): `0` = off; `v > 0` lets `v - 1`
/// steps commit and then severs the plan exactly where a crash would, so
/// `1` = before any step (the pre-state window) and `steps + 1` = after
/// every step but before the intent retirement. The severed plan performs
/// NO fail-stop escalation: it models a dead process, not a device error.
pub static TEST_XV_SEAM_AFTER_STEPS: AtomicU64 = AtomicU64::new(0);

/// Companion of [`TEST_XV_SEAM_AFTER_STEPS`] (PR 12b review round 1,
/// Issue 4): `0` = the seam severs EVERY initiator's plan (the one-process
/// suites); `id + 1` = only a plan whose coordinating volume's OWN
/// appender id is `id` is severed — the two-backend venue's way to kill
/// ONE daemon's op mid-plan while the manager's own ops run on.
pub static TEST_XV_SEAM_INITIATOR: AtomicU64 = AtomicU64::new(0);

/// Test seam (the served side): the NEXT shipped step commits and its
/// reply is MISDELIVERED (a wrong correlation id — the client refuses it
/// exactly as it fails a dead session), then the seam clears. Models a
/// holder dying after its commit and before its reply.
pub static TEST_XV_SERVE_MISDELIVER_ONCE: AtomicBool = AtomicBool::new(false);

/// Test seam (the served side): every shipped step REFUSES before it
/// commits while armed — a holder that is down, from the initiator's
/// side (the intent stays open for the roll-forward cadence).
pub static TEST_XV_SERVE_REFUSE: AtomicBool = AtomicBool::new(false);

/// Test seam (the served side): the NEXT shipped step is refused
/// `SlotBusy` at the holder's door BEFORE it commits — the slot moved
/// between the initiator's plan and the apply (PR 13, defect 29) — then
/// the seam clears. The initiator re-resolves and re-dispatches.
pub static TEST_XV_SERVE_SLOT_BUSY_ONCE: AtomicBool = AtomicBool::new(false);

/// Test seam (the served side — PR 13e, F-R4's pin): PARK every served
/// step at its holder right after the lease check and before the parent
/// verdict / apply — the window a slot RELEASE lands into. Released by
/// [`test_xv_serve_park_release`]; one relaxed load per served step.
pub static TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK: AtomicBool = AtomicBool::new(false);

/// Served steps that PARKED on [`TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK`]
/// so far (the test-side barrier that the schedule formed).
static TEST_XV_SERVE_PARKED: AtomicU64 = AtomicU64::new(0);

/// Served steps parked on [`TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK`] so far.
pub fn test_xv_serve_parked() -> u64 {
    TEST_XV_SERVE_PARKED.load(Ordering::Acquire)
}

static TEST_XV_SERVE_PARK_NOTIFY: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Release every served step parked on
/// [`TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK`] (the flag is stored `false`
/// first; the notify wakes the loop).
pub fn test_xv_serve_park_release() {
    TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK.store(false, Ordering::Relaxed);
    TEST_XV_SERVE_PARK_NOTIFY.notify_waiters();
}

/// The served step's park point (one relaxed load when the seam is off).
pub(crate) async fn test_xv_serve_park_point() {
    if !TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK.load(Ordering::Relaxed) {
        return;
    }
    TEST_XV_SERVE_PARKED.fetch_add(1, Ordering::AcqRel);
    while TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK.load(Ordering::Relaxed) {
        let notified = TEST_XV_SERVE_PARK_NOTIFY.notified();
        if !TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK.load(Ordering::Relaxed) {
            break;
        }
        notified.await;
    }
}

/// Test seam (the initiator's own door): the next N LOCALLY dispatched
/// steps are refused `SlotBusy` at this mount's door BEFORE any effect —
/// a slot that moved away between the plan and the door, and kept moving
/// through the retry bound (PR 13 review round 1, Issue 13: the
/// roll-forward's local arm). Decremented per refusal; `0` = off.
pub static TEST_XV_LOCAL_STEP_SLOT_BUSY: AtomicU64 = AtomicU64::new(0);

/// Test seam: the grace window in ms after which an open intent no holder
/// serves counts as STUCK (`0` = the derived window,
/// [`stuck_grace_ms`]) — and after which a parked guard scope whose
/// initiator never released it EXPIRES (the same lease law).
pub static TEST_XV_STUCK_AFTER_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam (the served side): the NEXT shipped step answers
/// `ForeignSkipped` WITHOUT applying — the holder's witness refusing a
/// step planned on a stale foreign read (the object moved at the holder
/// between the plan and the apply); the seam clears.
pub static TEST_XV_SERVE_SKIP_ONCE: AtomicBool = AtomicBool::new(false);

/// Test seam (the served side): every shipped STEP is held this many ms
/// at the holder before it applies — a live op that spans cadence passes
/// (the review-round-1 Issue 4 shape).
pub static TEST_XV_SERVE_HOLD_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam (the cadence): a roll-forward pass parks this many ms between
/// its scan and its recovery — the window in which another pass retires
/// what it scanned.
pub static TEST_XV_CADENCE_HOLD_AFTER_SCAN_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam: the NEXT in-process take of the set-wide directory-rename
/// lease runs under this appender identity instead of the mount's own
/// (0 = off; consumed by the take) — a second INITIATOR in one process,
/// KD-SYM-14's pin. Must name a joined appender (its Live page).
pub static TEST_DIR_RENAME_IDENTITY_ONCE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Test seam: acquire an IN-PROCESS holder's guards over the wire (an
/// `XvGuards` to its endpoint) instead of in the shared table's one
/// canonical `lock_many` — the initiator's remote arm, exercised where
/// every holder shares the process (the contracts' two-appender model).
pub static TEST_XV_GUARDS_FORCE_REMOTE: AtomicBool = AtomicBool::new(false);

/// Test seam: a scope's fire-and-forget `XvRelease` is held this many ms
/// before it is submitted — the schedule where the NEXT op's `XvGuards`
/// on the same keys reaches the holder first (PR 5 review round 3, Issue
/// 26: the load-selected order made deterministic).
pub static TEST_XV_RELEASE_HOLD_MS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The Cross-owner family (design-symmetric-metadata §11; stats inode
// `xv_cross_owner_*`, `dir_rename_lock_*`). Every gauge 0 on an unarmed
// mount BY CONSTRUCTION: the shipped step and the lock exist only under
// the armed plane.
// ---------------------------------------------------------------------------

/// Intents this process came to know as OPEN — minted here, or found
/// open at a scan (an adopted intent counts as minted at discovery, so
/// `minted ≡ retired + open` holds per process).
static XV_CO_INTENTS_MINTED: AtomicU64 = AtomicU64::new(0);
/// Intents retired by this process (the live path, recovery, the cadence).
static XV_CO_INTENTS_RETIRED: AtomicU64 = AtomicU64::new(0);
/// Steps this initiator shipped to a foreign slot holder.
static XV_CO_STEPS_SHIPPED: AtomicU64 = AtomicU64::new(0);
/// Steps this holder served for a foreign initiator.
static XV_CO_STEPS_SERVED: AtomicU64 = AtomicU64::new(0);
/// Served steps REFUSED by the wire-word screen (an insert naming a child
/// nobody could have minted) — **must stay 0**: the buggy/hostile-peer
/// class, never a witness verdict.
static XV_CO_STEPS_REJECTED: AtomicU64 = AtomicU64::new(0);
/// Set-wide directory-rename lock takes (initiator side).
static DIR_RENAME_LOCK_ACQUIRES: AtomicU64 = AtomicU64::new(0);
/// Reverse dentry SCANS the ancestor walk ran under the lease (the
/// parent memo's misses — one per cold hop; growth per rename on a warm
/// mount is the memo failing).
static DIR_RENAME_PARENT_SCANS: AtomicU64 = AtomicU64::new(0);
/// `XvGuards` this initiator shipped (one per foreign holder table per
/// op — the design's "foreign-home guards travel: dlm_rpcs += 1 each"
/// face; the `dlm_rpcs` word itself is the S4 table's).
static XV_CO_GUARD_RPCS: AtomicU64 = AtomicU64::new(0);
/// Travelling-guard acquisitions retried after a holder answered "stale
/// holder view" (PR 13): the initiator re-resolved every key's slot at
/// the manager and shipped again.
static XV_CO_GUARD_STALE_RERESOLVES: AtomicU64 = AtomicU64::new(0);
/// Shipped steps a holder refused `SlotBusy` (the slot moved between the
/// plan and the apply) and the initiator re-resolved and re-dispatched
/// (PR 13, defect 29; `xv_cross_owner_step_slot_moved_retries`).
static XV_CO_STEP_SLOT_MOVED_RETRIES: AtomicU64 = AtomicU64::new(0);
/// Local steps whose slot moved TO this initiator mid-plan (the guards
/// travelled to the old holder) and that took their OWN guards for the
/// apply in the non-parking canonical form (PR 13b;
/// `xv_cross_owner_step_late_guards`).
static XV_CO_STEP_LATE_GUARDS: AtomicU64 = AtomicU64::new(0);
/// Such steps whose late guards were CONTENDED — answered the slot-moved
/// retryable class instead of an unguarded apply (PR 13b;
/// `xv_cross_owner_step_late_guard_refusals`).
static XV_CO_STEP_LATE_GUARD_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Namespace ops dispatched LOCALLY whose commit door answered `SlotBusy`
/// (the slot read unleased or ours at the plan and another appender
/// held it at the door) and were re-dispatched ONCE through the
/// cross-owner arm (PR 13, defect 30; `xv_cross_owner_op_slot_moved_
/// redispatches`).
static XV_CO_OP_SLOT_MOVED_REDISPATCHES: AtomicU64 = AtomicU64::new(0);
/// The cross-owner plan builders' "no inode record — removing the
/// dangling name and accounting nothing" arm (an unlink / rmdir / rename-
/// over whose child witness read NONE): the count step is dropped and the
/// name alone goes. **Must stay 0 on an armed mount** — a child whose slot
/// another appender leases has its witness read AT THE HOLDER (PR 13e,
/// F-R3), so a `None` here is a record genuinely gone; before it the
/// local read of a foreign lessee's slot answered this daemon's
/// PROJECTION (never the leased root — KD-SYM-3), and every `rm -rf` of
/// a directory another appender had created into orphaned one inode per
/// name (`xv_cross_owner_dangling_names`).
static XV_CO_DANGLING_NAMES: AtomicU64 = AtomicU64::new(0);
/// Witness reads REFUSED (`EAGAIN`-class) because the local read found no
/// record in a slot a LIVE foreign appender leases while no read divert
/// could reach its holder — a `None` off a projection is not a witness
/// (PR 13e, F-R3's belt; `xv_cross_owner_witness_refusals`). 0 on the
/// mount path by construction (the divert is armed on every writer);
/// the retryable class where a race moved the slot under the read.
static XV_CO_WITNESS_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Guard scopes this holder parked for a remote initiator.
static XV_CO_GUARDS_PARKED: AtomicU64 = AtomicU64::new(0);
/// Parked scopes released by the lease-expiry sweep, not their initiator
/// — **must stay 0** on a healthy set (a dead initiator's guards).
static XV_CO_GUARD_EXPIRIES: AtomicU64 = AtomicU64::new(0);

/// Phases of one cross-owner transaction (`xv_cross_owner_phase_ns`:
/// `plan + intent_barrier + Σ ship_rtt + local_steps + retire ≈ total`;
/// `guard_rtt` is taken in the arm, before `total` starts, so it is
/// priced beside the transaction rather than inside it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum XvPhase {
    /// Guards → tx0 committed (the plan + step 0 when local; the intent
    /// alone otherwise).
    Plan = 0,
    /// The initiator ring's barrier after tx0.
    IntentBarrier = 1,
    /// Σ of the shipped steps' round trips.
    ShipRtt = 2,
    /// The retirement commit.
    Retire = 3,
    /// The whole transaction.
    Total = 4,
    /// Σ of the `XvGuards` round trips (the travelling guards).
    GuardRtt = 5,
    /// Σ of the LOCAL steps applied after `tx0` and the local
    /// participants' barriers — so `plan + intent_barrier + Σ ship_rtt +
    /// local_steps + retire ≈ total` (a live refusal's compensation is
    /// outside the table).
    LocalSteps = 6,
}

const XV_PHASES: usize = 7;
const XV_PHASE_NAMES: [&str; XV_PHASES] = [
    "plan",
    "intent_barrier",
    "ship_rtt",
    "retire",
    "total",
    "guard_rtt",
    "local_steps",
];

static XV_PROF: once_cell::sync::Lazy<[crate::fuse_client::LatencyHistogram; XV_PHASES]> =
    once_cell::sync::Lazy::new(|| {
        std::array::from_fn(|_| crate::fuse_client::LatencyHistogram::default())
    });

/// `dir_rename_lock_wait_ns`: the wait for the set-wide lock.
static DIR_RENAME_LOCK_WAIT: once_cell::sync::Lazy<crate::fuse_client::LatencyHistogram> =
    once_cell::sync::Lazy::new(crate::fuse_client::LatencyHistogram::default);

/// Record one phase span started at `t0`.
#[inline]
fn phase_record(phase: XvPhase, t0: std::time::Instant) {
    XV_PROF[phase as usize].record(t0.elapsed());
}

/// Count one set-wide directory-rename lock take and its wait.
pub(crate) fn note_dir_rename_lock(waited: std::time::Duration) {
    DIR_RENAME_LOCK_ACQUIRES.fetch_add(1, Ordering::Relaxed);
    DIR_RENAME_LOCK_WAIT.record(waited);
}

/// Count one memo-miss reverse scan inside the ancestor walk.
pub(crate) fn note_dir_rename_parent_scan() {
    DIR_RENAME_PARENT_SCANS.fetch_add(1, Ordering::Relaxed);
}

/// Count one served step refused by the wire-word screen.
pub(crate) fn note_step_rejected() {
    XV_CO_STEPS_REJECTED.fetch_add(1, Ordering::Relaxed);
}

/// Count one served shipped step (the holder side).
pub(crate) fn note_step_served() {
    XV_CO_STEPS_SERVED.fetch_add(1, Ordering::Relaxed);
}

/// How this process knows an open intent (review round 1, Issue 4 — the
/// cadence adopts by STATE, never by elapsed time or a tick count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntentState {
    /// A live op of THIS process is executing it: its own, whatever its
    /// wall — the cadence never touches it.
    InFlight,
    /// Left open by an op that returned (its holder unreachable, its
    /// guards unacquirable) or found open at a scan with no live owner in
    /// this process (a predecessor incarnation's): the cadence's to
    /// complete; STUCK once older than the grace window.
    Abandoned,
}

struct IntentEntry {
    since: std::time::Instant,
    state: IntentState,
    /// Counted on `intents_minted` (once the record is durable).
    counted: bool,
}

/// The open-intent register. Its population IS `xv_cross_owner_intents_open`;
/// an ABANDONED entry older than the grace window is the STUCK class.
static OPEN_INTENTS: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<u64, IntentEntry>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// A live op registers its intent BEFORE the record is durable, so a
/// concurrent scan that sees the record always finds the owner; a failed
/// `tx0` unregisters without a count.
fn note_intent_in_flight(tx_id: u64) {
    OPEN_INTENTS.lock().entry(tx_id).or_insert(IntentEntry {
        since: std::time::Instant::now(),
        state: IntentState::InFlight,
        counted: false,
    });
}

/// The record is durable: count the mint.
fn note_intent_durable(tx_id: u64) {
    let mut open = OPEN_INTENTS.lock();
    let e = open.entry(tx_id).or_insert(IntentEntry {
        since: std::time::Instant::now(),
        state: IntentState::InFlight,
        counted: false,
    });
    if !e.counted {
        e.counted = true;
        XV_CO_INTENTS_MINTED.fetch_add(1, Ordering::Relaxed);
    }
}

/// The op left its intent open (its ship failed): the cadence's now; the
/// stuck window counts from here.
fn note_intent_abandoned(tx_id: u64) {
    let mut open = OPEN_INTENTS.lock();
    if let Some(e) = open.get_mut(&tx_id) {
        e.state = IntentState::Abandoned;
        e.since = std::time::Instant::now();
    }
}

/// An intent found open at a scan with no live owner in this process
/// (adopted at discovery — counted as minted here, so `minted ≡ retired +
/// open` holds per process). Idempotent.
fn note_intent_adopted(tx_id: u64) {
    let mut open = OPEN_INTENTS.lock();
    let e = open.entry(tx_id).or_insert(IntentEntry {
        since: std::time::Instant::now(),
        state: IntentState::Abandoned,
        counted: false,
    });
    if !e.counted {
        e.counted = true;
        XV_CO_INTENTS_MINTED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Is `tx_id` a live op's own intent in this process?
fn intent_in_flight(tx_id: u64) -> bool {
    OPEN_INTENTS
        .lock()
        .get(&tx_id)
        .is_some_and(|e| e.state == IntentState::InFlight)
}

/// Forget an intent whose record turned out retired (a stale scan met a
/// completed transaction — no ghost survives).
fn forget_intent(tx_id: u64) {
    if let Some(e) = OPEN_INTENTS.lock().remove(&tx_id) {
        if e.counted {
            XV_CO_INTENTS_RETIRED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Note an intent as retired.
fn note_intent_retired(tx_id: u64) {
    if let Some(e) = OPEN_INTENTS.lock().remove(&tx_id) {
        if e.counted {
            XV_CO_INTENTS_RETIRED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A `tx0` that never landed: unregister, uncounted.
fn note_intent_never_durable(tx_id: u64) {
    OPEN_INTENTS.lock().remove(&tx_id);
}

/// The grace window after which an ABANDONED cross-owner intent nobody
/// serves is STUCK: the membership lease TTL (`CLIENT_STALE_TTL_SECS`) — a
/// slot's dead holder is evicted and its slot recovered inside it (design
/// §5.9), so an intent still open past it names a slot no live holder
/// serves; the seam shortens it for the contracts.
pub fn stuck_grace_ms() -> u64 {
    match TEST_XV_STUCK_AFTER_MS.load(Ordering::Relaxed) {
        0 => crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000,
        ms => ms,
    }
}

/// `(open, stuck)`: the register's population and how many ABANDONED
/// entries are older than the grace window — one lock scope. A live op's
/// own intent is never stuck (its wall is the D1.b watchdog's).
fn open_and_stuck_now() -> (u64, u64) {
    let grace = std::time::Duration::from_millis(stuck_grace_ms());
    let open = OPEN_INTENTS.lock();
    let stuck = open
        .values()
        .filter(|e| e.state == IntentState::Abandoned && e.since.elapsed() > grace)
        .count();
    (open.len() as u64, stuck as u64)
}

/// Reconcile the register against a SUCCESSFUL durable scan (PR 13e review
/// round 2 — found by Issue 10's pin): an ABANDONED entry whose record the
/// scan no longer lists is no longer this process's to complete — another
/// appender's roll-forward retired it (the manager's poll adopting a
/// joiner's abandoned intent, or the reverse; the in-process fixtures
/// share one register, so only a real fleet met the ghost), or its slot
/// moved to an appender whose scan owns it now — and would otherwise ride
/// this register as open, then STUCK, for the mount's life. A live op's
/// own (`InFlight`) entry is never touched: its record may not be durable
/// yet. Counted as retired where it was counted as minted, so `minted ≡
/// retired + open` keeps holding per process. Returns how many were
/// forgotten.
fn forget_abandoned_absent(durable: &std::collections::HashSet<u64>) -> usize {
    let mut open = OPEN_INTENTS.lock();
    let ghosts: Vec<u64> = open
        .iter()
        .filter(|(tx, e)| e.state == IntentState::Abandoned && !durable.contains(*tx))
        .map(|(tx, _)| *tx)
        .collect();
    for tx in &ghosts {
        if let Some(e) = open.remove(tx) {
            if e.counted {
                XV_CO_INTENTS_RETIRED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    ghosts.len()
}

/// Does the register hold anything the cadence owns (an abandoned intent)?
fn any_abandoned() -> bool {
    OPEN_INTENTS
        .lock()
        .values()
        .any(|e| e.state == IntentState::Abandoned)
}

/// One snapshot of the Cross-owner family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrossOwnerStats {
    pub intents_minted: u64,
    pub intents_retired: u64,
    /// GAUGE: intents this process knows to be open.
    pub intents_open: u64,
    pub steps_shipped: u64,
    pub steps_served: u64,
    /// **Must stay 0**: served steps the wire-word screen refused.
    pub steps_rejected: u64,
    /// GAUGE, **must stay 0**: open intents past the grace window.
    pub intents_stuck: u64,
    pub dir_rename_lock_acquires: u64,
    pub dir_rename_lock_wait_ns_sum: u64,
    /// Reverse dentry scans the ancestor walk ran (parent-memo misses).
    pub dir_rename_parent_scans: u64,
    /// `XvGuards` shipped (initiator side).
    pub guard_rpcs: u64,
    pub guard_stale_reresolves: u64,
    /// PR 13 (defect 29): shipped steps re-dispatched after a holder's
    /// `SlotBusy` (the slot moved between the plan and the apply).
    pub step_slot_moved_retries: u64,
    /// PR 13b: local steps re-dispatched after their slot moved TO this
    /// initiator that took their own guards for the apply.
    pub step_late_guards: u64,
    /// PR 13b: such steps whose late guards were contended (the retryable
    /// class, the intent left for the cadence).
    pub step_late_guard_refusals: u64,
    /// PR 13 (defect 30): locally dispatched namespace ops re-dispatched
    /// through the cross-owner arm after the door's `SlotBusy`.
    pub op_slot_moved_redispatches: u64,
    /// Scopes parked for remote initiators (holder side).
    pub guards_parked: u64,
    /// **Must stay 0**: scopes the lease-expiry sweep released.
    pub guard_expiries: u64,
    /// PR 13e (F-R3), **must stay 0 on an armed mount**: plan builders
    /// that found no child record and dropped the count step.
    pub dangling_names: u64,
    /// PR 13e (F-R3): witness reads refused because a local `None` came
    /// off the projection of a live foreign lessee's slot.
    pub witness_refusals: u64,
}

/// Read the family.
pub fn cross_owner_stats() -> CrossOwnerStats {
    let (intents_open, intents_stuck) = open_and_stuck_now();
    CrossOwnerStats {
        intents_minted: XV_CO_INTENTS_MINTED.load(Ordering::Relaxed),
        intents_retired: XV_CO_INTENTS_RETIRED.load(Ordering::Relaxed),
        intents_open,
        steps_shipped: XV_CO_STEPS_SHIPPED.load(Ordering::Relaxed),
        steps_served: XV_CO_STEPS_SERVED.load(Ordering::Relaxed),
        steps_rejected: XV_CO_STEPS_REJECTED.load(Ordering::Relaxed),
        intents_stuck,
        dir_rename_lock_acquires: DIR_RENAME_LOCK_ACQUIRES.load(Ordering::Relaxed),
        dir_rename_lock_wait_ns_sum: DIR_RENAME_LOCK_WAIT.sum_ns(),
        dir_rename_parent_scans: DIR_RENAME_PARENT_SCANS.load(Ordering::Relaxed),
        guard_rpcs: XV_CO_GUARD_RPCS.load(Ordering::Relaxed),
        guard_stale_reresolves: XV_CO_GUARD_STALE_RERESOLVES.load(Ordering::Relaxed),
        step_slot_moved_retries: XV_CO_STEP_SLOT_MOVED_RETRIES.load(Ordering::Relaxed),
        step_late_guards: XV_CO_STEP_LATE_GUARDS.load(Ordering::Relaxed),
        step_late_guard_refusals: XV_CO_STEP_LATE_GUARD_REFUSALS.load(Ordering::Relaxed),
        op_slot_moved_redispatches: XV_CO_OP_SLOT_MOVED_REDISPATCHES.load(Ordering::Relaxed),
        guards_parked: XV_CO_GUARDS_PARKED.load(Ordering::Relaxed),
        guard_expiries: XV_CO_GUARD_EXPIRIES.load(Ordering::Relaxed),
        dangling_names: XV_CO_DANGLING_NAMES.load(Ordering::Relaxed),
        witness_refusals: XV_CO_WITNESS_REFUSALS.load(Ordering::Relaxed),
    }
}

/// A plan builder found no record for the child it removes a name of and
/// dropped the count step (the `xv_cross_owner_dangling_names` arm).
pub(crate) fn note_dangling_name() {
    XV_CO_DANGLING_NAMES.fetch_add(1, Ordering::Relaxed);
}

/// A witness read refused a projection's `None` for a live foreign
/// lessee's slot (`xv_cross_owner_witness_refusals`).
pub(crate) fn note_witness_refusal() {
    XV_CO_WITNESS_REFUSALS.fetch_add(1, Ordering::Relaxed);
}

/// The family as the stats inode serves it (its keys are §11's names).
pub fn cross_owner_stats_json() -> serde_json::Map<String, serde_json::Value> {
    let s = cross_owner_stats();
    let mut phases = serde_json::Map::new();
    for (i, name) in XV_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), XV_PROF[i].to_json());
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "xv_cross_owner_intents_minted".into(),
        s.intents_minted.into(),
    );
    out.insert(
        "xv_cross_owner_intents_retired".into(),
        s.intents_retired.into(),
    );
    out.insert("xv_cross_owner_intents_open".into(), s.intents_open.into());
    out.insert(
        "xv_cross_owner_steps_shipped".into(),
        s.steps_shipped.into(),
    );
    out.insert("xv_cross_owner_steps_served".into(), s.steps_served.into());
    out.insert(
        "xv_cross_owner_steps_rejected".into(),
        s.steps_rejected.into(),
    );
    out.insert(
        "xv_cross_owner_intents_stuck".into(),
        s.intents_stuck.into(),
    );
    out.insert(
        "xv_cross_owner_phase_ns".into(),
        serde_json::Value::Object(phases),
    );
    out.insert("xv_cross_owner_guard_rpcs".into(), s.guard_rpcs.into());
    out.insert(
        "xv_cross_owner_guard_stale_reresolves".into(),
        s.guard_stale_reresolves.into(),
    );
    out.insert(
        "xv_cross_owner_step_slot_moved_retries".into(),
        s.step_slot_moved_retries.into(),
    );
    out.insert(
        "xv_cross_owner_step_late_guards".into(),
        s.step_late_guards.into(),
    );
    out.insert(
        "xv_cross_owner_step_late_guard_refusals".into(),
        s.step_late_guard_refusals.into(),
    );
    out.insert(
        "xv_cross_owner_op_slot_moved_redispatches".into(),
        s.op_slot_moved_redispatches.into(),
    );
    out.insert(
        "xv_cross_owner_guards_parked".into(),
        s.guards_parked.into(),
    );
    out.insert(
        "xv_cross_owner_guard_expiries".into(),
        s.guard_expiries.into(),
    );
    out.insert(
        "xv_cross_owner_dangling_names".into(),
        s.dangling_names.into(),
    );
    out.insert(
        "xv_cross_owner_witness_refusals".into(),
        s.witness_refusals.into(),
    );
    out.insert(
        "dir_rename_lock_acquires".into(),
        s.dir_rename_lock_acquires.into(),
    );
    out.insert(
        "dir_rename_lock_wait_ns".into(),
        DIR_RENAME_LOCK_WAIT.to_json(),
    );
    out.insert(
        "dir_rename_parent_scans".into(),
        s.dir_rename_parent_scans.into(),
    );
    out
}

// ---------------------------------------------------------------------------
// The step shipper (the initiator's client half — the join ladder's rung 7
// installs it under the set's cluster secret, `sym_join::install_step_shipper`;
// the contracts that stand up their own listeners install it directly).
// ---------------------------------------------------------------------------

static XV_SHIPPER: once_cell::sync::Lazy<
    arc_swap::ArcSwapOption<crate::meta_ship::MetaShipRouter>,
> = once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::const_empty);

/// Install the process-global step shipper: the S8 client router a
/// foreign step travels on (its lanes, its same-id resend, its era
/// learning). Product caller: the join ladder's rung 7
/// (`sym_join::install_step_shipper`, PR 12); the contracts with their own
/// listeners install it directly. Inert on an unarmed mount (no step is
/// ever foreign there).
pub fn install_xv_shipper(router: Arc<crate::meta_ship::MetaShipRouter>) {
    XV_SHIPPER.store(Some(router));
}

/// Remove it (a stale install is inert: a foreign step with no shipper
/// is the un-shippable class the cadence retries).
pub fn uninstall_xv_shipper() {
    XV_SHIPPER.store(None);
}

// ---------------------------------------------------------------------------
// The travelling guard (design §5.6 line 1: "plan under the op's 4a
// guards — foreign-home guards travel").
//
// An initiator's guards on objects in a FOREIGN holder's slots are held
// AT THE HOLDER, parked under a scope, for the op's duration; the steps
// it ships under that scope apply without taking a guard. Two laws make
// the plane deadlock-free:
//
// 1. **One table, one canonical `lock_many`** — every key whose lock
//    lives in THIS process's table (the initiator's own slots and, in
//    the contracts' declared-partition model, its other regions') is
//    taken in the one stripe-canonical call the 4a law already
//    prescribes; a key in another process's table is one `XvGuards` to
//    that holder, served by the same `lock_many` in ITS table.
// 2. **Tables in ascending appender-id order** — an initiator acquires
//    its own table at its own id and each foreign table at the holder's
//    id, ascending, so a waiter for table `T` holds only tables `< T`
//    and the wait-for graph over tables is acyclic (the hierarchical
//    argument; in the shipped one-appender-per-process shape a holder's
//    id IS its table's rank).
//
// A served STEP therefore never parks on a 4a guard: under a scope it
// applies with the parked (or the in-process initiator's own) guards;
// without one (a roll-forward that travelled none) it takes the step's
// keys for the apply. Found by the scoping row: the first build took the
// step's guards at the holder while the initiator held its own across
// the ship — a stripe collision in one process, the two-mutual-initiator
// cycle across two.
// ---------------------------------------------------------------------------

/// A guard scope's wire identity: the initiator's client id + the scope
/// it minted (unique per initiator).
#[derive(Debug, Clone, Copy)]
pub struct GuardScope<'a> {
    pub client: &'a str,
    pub scope: u64,
}

/// A set of `(volume, inode class, stripe)` — what a scope's guards cover.
type StripeSet = std::collections::HashSet<(usize, bool, usize)>;

/// Scopes THIS process minted whose guards it holds in its own table,
/// with the stripes they cover — alive exactly while the op's guard set
/// is (the marker guard's drop removes the entry). The served-side
/// `Covered` verdict compares a step's stripes against this set (review
/// round 1, Issue 8b).
static LOCAL_SCOPES: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<u64, StripeSet>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// One remote initiator's parked guards: the guards, the stripes they
/// cover (`(volume, inode class, stripe)` — a later `XvGuards` under the
/// same scope skips a stripe already parked, or it would wait behind
/// itself) and when the scope was first parked.
struct ParkedScope {
    guards: Vec<dlm::DlmGuard>,
    stripes: StripeSet,
    since: std::time::Instant,
}

/// Guards parked for REMOTE initiators, keyed by `(client id, scope)`.
static PARKED_GUARDS: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<(String, u64), ParkedScope>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Mint a guard scope for one op (nonzero — 0 on the wire means "no
/// scope travelled").
pub fn mint_guard_scope() -> u64 {
    (next_seq() & XV_SEQ_MASK) | (1 << XV_SEQ_BITS)
}

/// The scope an op's guard set travels under (0 = none).
pub fn scope_of(guards: &[dlm::DlmGuard]) -> u64 {
    guards
        .iter()
        .find_map(dlm::DlmGuard::external_scope)
        .unwrap_or(0)
}

/// Register `scope` as held in THIS table; the returned marker guard's
/// drop unregisters it (it rides the op's guard set, so the served-side
/// `Held` verdict lasts exactly as long as the guards do).
fn register_local_scope(scope: u64) -> dlm::DlmGuard {
    LOCAL_SCOPES.lock().entry(scope).or_default();
    dlm::DlmGuard::external(scope, move || {
        LOCAL_SCOPES.lock().remove(&scope);
    })
}

/// Record the stripes a local scope's `lock_many` covered on `v_idx`.
fn note_local_scope_stripes(
    scope: u64,
    dlm: &dlm::DlmLockManager,
    v_idx: usize,
    inos: &[(Ino, dlm::LockMode)],
    dents: &[(Ino, &str, dlm::LockMode)],
) {
    let mut scopes = LOCAL_SCOPES.lock();
    let Some(set) = scopes.get_mut(&scope) else {
        return;
    };
    set.extend(
        inos.iter()
            .map(|(l, _)| (v_idx, true, dlm.inode_stripe(*l))),
    );
    set.extend(
        dents
            .iter()
            .map(|(l, n, _)| (v_idx, false, dlm.dentry_stripe(*l, n))),
    );
}

/// The stripes a step's guards would take on `v_idx` (the served side's
/// coverage question, and the local applier's tripwire).
pub(crate) fn step_stripes(
    dlm: &dlm::DlmLockManager,
    v_idx: usize,
    local: &XvLocalStep,
) -> StripeSet {
    let (inos, dents) = step_guard_keys(local);
    inos.iter()
        .map(|(l, _)| (v_idx, true, dlm.inode_stripe(*l)))
        .chain(
            dents
                .iter()
                .map(|(l, n)| (v_idx, false, dlm.dentry_stripe(*l, n))),
        )
        .collect()
}

/// Does the scope this op's guards travel under cover `needed` in THIS
/// table? `None` = the op has no local scope (unarmed / a plain guard set).
fn local_scope_covers(scope: u64, needed: &StripeSet) -> Option<bool> {
    LOCAL_SCOPES
        .lock()
        .get(&scope)
        .map(|set| needed.is_subset(set))
}

/// Release the guards parked under `(client, scope)`; `false` when the
/// holder has none (a release for a scope already released or expired —
/// answered `Unit` all the same, the idempotent shape).
pub fn release_parked_guards(client: &str, scope: u64) -> bool {
    PARKED_GUARDS
        .lock()
        .remove(&(client.to_string(), scope))
        .is_some()
}

/// Release every parked scope whose initiator's LEASE is gone (review
/// round 1, Issue 9 — the expiry law): where this process is the S6
/// membership authority, a scope is kept exactly while its client is a
/// live member (`MembershipOwner::epoch_of`) and released the sweep after
/// the owner evicts it — never by elapsed time; a client this owner does
/// NOT know (a cross-shard initiator, a joiner the ladder has not bound
/// yet — review round 2, Issue 22) and every client where no plane is
/// armed (the shipped dark posture) keep the grace window as the belt —
/// the lease TTL the eviction would have fired at. Safety: a scope
/// released early costs an initiator its isolation (its steps fall to
/// `Take` under the witness — never corruption), so the belt errs toward
/// KEEPING an unknown client's scope. The cadence runs this every tick.
/// Returns the count (`xv_cross_owner_guard_expiries`).
pub fn sweep_expired_guards() -> u64 {
    let grace = std::time::Duration::from_millis(stuck_grace_ms());
    let owner = crate::membership::installed_owner();
    let mut parked = PARKED_GUARDS.lock();
    let before = parked.len();
    parked.retain(|(client, _), p| match &owner {
        Some(o) if o.epoch_of(client).is_some() => true,
        _ => p.since.elapsed() <= grace,
    });
    let expired = (before - parked.len()) as u64;
    if expired > 0 {
        XV_CO_GUARD_EXPIRIES.fetch_add(expired, Ordering::Relaxed);
        log::warn!(
            "cross-owner guards: {expired} parked scope(s) outlived the grace window and were \
             released — their initiator died without releasing (xv_cross_owner_guard_expiries)"
        );
    }
    expired
}

/// What a served step applies under.
pub(crate) enum ServeGuards {
    /// The scope's guards are held for the step already — by an
    /// in-process initiator in this table, or parked here for a remote
    /// one (the parked set outlives the step: the release comes after
    /// the reply, which comes after the commit's terminal outcome) — so
    /// the apply takes NOTHING.
    Covered,
    /// No scope travelled (or the holder no longer has it): take the
    /// step's keys for the apply.
    Take,
}

/// Resolve a served step's guard verdict: `Covered` only when the scope
/// is known AND its guards cover every stripe the step would take
/// (Issue 8b — a scope that parked unrelated keys never lets a step apply
/// unguarded); anything else takes the step's keys.
pub(crate) fn serve_guards_for(scope: GuardScope<'_>, needed: &StripeSet) -> ServeGuards {
    if scope.scope == 0 {
        return ServeGuards::Take;
    }
    if let Some(router) = XV_SHIPPER.load_full() {
        if router.peer_id() == scope.client && local_scope_covers(scope.scope, needed) == Some(true)
        {
            return ServeGuards::Covered;
        }
    }
    if PARKED_GUARDS
        .lock()
        .get(&(scope.client.to_string(), scope.scope))
        .is_some_and(|p| needed.is_subset(&p.stripes))
    {
        return ServeGuards::Covered;
    }
    ServeGuards::Take
}

/// Does a scope THIS mount minted have guards parked in THIS table covering
/// `needed` — the one-process holder model, where the initiator's
/// travelling guards land in its own table under its own client id?
fn own_parked_scope_covers(scope: u64, needed: &StripeSet) -> bool {
    let Some(router) = XV_SHIPPER.load_full() else {
        return false;
    };
    PARKED_GUARDS
        .lock()
        .get(&(router.peer_id().to_string(), scope))
        .is_some_and(|p| needed.is_subset(&p.stripes))
}

/// Test seam: drop every parked scope — the fleet's two-table reality in
/// one process (a scope parked at the OLD holder covers nothing in the
/// initiator's table once the slot moved to it). Returns the count.
#[doc(hidden)]
pub fn test_release_all_parked_guards() -> usize {
    let mut parked = PARKED_GUARDS.lock();
    let n = parked.len();
    parked.clear();
    n
}

/// Park `guards` for `(client, scope)` — extending a scope already parked.
pub(crate) fn park_guards(
    client: &str,
    scope: u64,
    guards: Vec<dlm::DlmGuard>,
    stripes: impl IntoIterator<Item = (usize, bool, usize)>,
) {
    let mut parked = PARKED_GUARDS.lock();
    let entry = parked
        .entry((client.to_string(), scope))
        .or_insert_with(|| {
            XV_CO_GUARDS_PARKED.fetch_add(1, Ordering::Relaxed);
            ParkedScope {
                guards: Vec::new(),
                stripes: std::collections::HashSet::new(),
                since: std::time::Instant::now(),
            }
        });
    entry.guards.extend(guards);
    entry.stripes.extend(stripes);
}

/// Scopes currently parked for remote initiators (a gauge — the contracts
/// observe a fire-and-forget release landing).
pub fn parked_scopes() -> usize {
    PARKED_GUARDS.lock().len()
}

/// The stripes already parked under `(client, scope)`.
pub(crate) fn parked_stripes(client: &str, scope: u64) -> StripeSet {
    PARKED_GUARDS
        .lock()
        .get(&(client.to_string(), scope))
        .map(|p| p.stripes.clone())
        .unwrap_or_default()
}

/// **The initiator's 4a acquisition on an armed volume** (the two laws
/// above): `inos` / `dents` are the op's LOCAL keys on volume `v_idx`,
/// `scope` the op's scope cell — minted on the FIRST armed acquisition
/// (review round 1, Issue 11: an unarmed op never mints, so the shipped
/// path pays no shared-line RMW) and shared by every per-volume call of
/// one op. Keys in this process's table ride one canonical `lock_many`;
/// keys in a foreign holder's table ride one `XvGuards` each, tables
/// ascending by appender id; the returned set carries the scope marker
/// and one external guard per remote table whose drop releases it.
/// `discovery` = a first-phase read that revalidates under the op's full
/// set (the unlink shape): foreign tables are NOT acquired — the read
/// holds nothing at the holder and the op pays ONE round trip per holder,
/// at its full acquisition (Issue 13). Unarmed (or inside a served verb):
/// the plain `lock_many`.
pub async fn acquire_guards_leased(
    routed: &RoutedMetaBackend,
    v_idx: usize,
    scope: &mut Option<u64>,
    inos: &[(Ino, dlm::LockMode)],
    dents: &[(Ino, &str, dlm::LockMode)],
    discovery: bool,
) -> Result<Vec<dlm::DlmGuard>> {
    let vol = &routed.volumes[v_idx];
    if !vol.slot_lease_armed() || crate::meta_ship::executing_for_ship_client() {
        return Ok(vol.dlm().lock_many(inos, dents).await);
    }
    // A holder's refusal "this mount does not lease the slot — the
    // initiator re-resolves through tree 0" (`serve_xv_guards`) IS the
    // re-resolve's trigger (PR 13, the fleet's shared-directory row: a
    // stripe's slot handed to a dominating requester between two of a
    // creator's ops left the creator's projection naming the old holder,
    // and the refusal surfaced as EAGAIN to the application). Every key's
    // slot is re-resolved at the manager and the acquisition retried —
    // bounded: a second stale answer is the retryable class the caller
    // sees.
    const STALE_HOLDER_RETRIES: usize = 2;
    let mut attempt = 0;
    loop {
        match acquire_guards_leased_once(routed, v_idx, scope, inos, dents, discovery).await {
            Err(e) if is_stale_holder_refusal(&e) && attempt < STALE_HOLDER_RETRIES => {
                attempt += 1;
                XV_CO_GUARD_STALE_RERESOLVES.fetch_add(1, Ordering::Relaxed);
                for local in inos
                    .iter()
                    .map(|(l, _)| *l)
                    .chain(dents.iter().map(|(p, _, _)| *p))
                {
                    let slot = crate::meta_backend::kv::record::forest_slot_of_ino(local);
                    let _ = vol.reresolve_slot_holder(slot).await;
                }
                log::debug!(
                    "cross-owner guards: a holder answered stale ({e}); the slots re-resolved at \
                     the manager, attempt {attempt} of {STALE_HOLDER_RETRIES}"
                );
            }
            other => return other,
        }
    }
}

/// The refusal a travelling `XvGuards` earns at a holder whose lease of
/// the slot has moved (`RoutedMetaBackend::serve_xv_guards`) — the TYPED
/// class, minted at the served side and carried as `WireError::class`
/// (PR 13 review round 1, Issue 7): the prose is never read.
fn is_stale_holder_refusal(e: &SqueezefsError) -> bool {
    matches!(
        e.refusal_class(),
        Some(crate::error::RefusalClass::StaleHolderView)
    )
}

async fn acquire_guards_leased_once(
    routed: &RoutedMetaBackend,
    v_idx: usize,
    scope: &mut Option<u64>,
    inos: &[(Ino, dlm::LockMode)],
    dents: &[(Ino, &str, dlm::LockMode)],
    discovery: bool,
) -> Result<Vec<dlm::DlmGuard>> {
    let vol = &routed.volumes[v_idx];
    let scope = *scope.get_or_insert_with(mint_guard_scope);
    let force_remote = TEST_XV_GUARDS_FORCE_REMOTE.load(Ordering::Relaxed);
    let own_id = vol.own_appender_id();
    // A holder the rung-7 census did not know (it joined after this
    // mount's ladder — PR 12b, N ≥ 3) is bound ON DEMAND before the
    // table is consulted, once per key population.
    for local in inos
        .iter()
        .map(|(l, _)| *l)
        .chain(dents.iter().map(|(p, _, _)| *p))
    {
        if let StepHome::Unreachable { holder } = step_home(routed, v_idx, local) {
            if !vol.is_own_region(holder) {
                crate::sym_join::bind_holder_endpoint_on_demand(vol, holder).await;
            }
        }
    }
    struct RemoteSet {
        endpoint: Arc<str>,
        inos: Vec<(u64, bool)>,
        dents: Vec<(u64, String, bool)>,
    }
    let mut local_inos: Vec<(Ino, dlm::LockMode)> = Vec::new();
    let mut local_dents: Vec<(Ino, &str, dlm::LockMode)> = Vec::new();
    let mut remote: std::collections::BTreeMap<u32, RemoteSet> = std::collections::BTreeMap::new();
    let global = |local: Ino| -> Result<Ino> {
        routed.try_make_global_ino(local, v_idx).ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "cross-owner guards: local ino {local} of volume {v_idx} has no global form"
            ))
        })
    };
    // Where a key's lock lives: this table, or a holder's.
    let table_of = |local: Ino| -> Result<Option<(u32, Arc<str>)>> {
        match step_home(routed, v_idx, local) {
            StepHome::Local => Ok(None),
            // One of this mount's own regions shares its table whether or
            // not an endpoint is bound for it.
            StepHome::Foreign { holder, .. } | StepHome::Unreachable { holder }
                if !force_remote && vol.is_own_region(holder) =>
            {
                Ok(None)
            }
            StepHome::Foreign { holder, endpoint } => Ok(Some((holder, endpoint))),
            StepHome::Unreachable { holder } => Err(unreachable_error(holder, "4a guards")),
        }
    };
    for &(local, mode) in inos {
        match table_of(local)? {
            None => local_inos.push((local, mode)),
            Some(_) if discovery => {}
            Some((holder, endpoint)) => remote
                .entry(holder)
                .or_insert_with(|| RemoteSet {
                    endpoint,
                    inos: Vec::new(),
                    dents: Vec::new(),
                })
                .inos
                .push((global(local)?, mode == dlm::LockMode::Exclusive)),
        }
    }
    for &(local, name, mode) in dents {
        match table_of(local)? {
            None => local_dents.push((local, name, mode)),
            Some(_) if discovery => {}
            Some((holder, endpoint)) => remote
                .entry(holder)
                .or_insert_with(|| RemoteSet {
                    endpoint,
                    inos: Vec::new(),
                    dents: Vec::new(),
                })
                .dents
                .push((
                    global(local)?,
                    name.to_string(),
                    mode == dlm::LockMode::Exclusive,
                )),
        }
    }
    let mut guards: Vec<dlm::DlmGuard> = Vec::new();
    if remote.is_empty() {
        if local_inos.is_empty() && local_dents.is_empty() {
            return Ok(guards);
        }
        // Every key in this table: the shipped path verbatim, plus the
        // marker that lets an in-process holder's serve recognize the
        // scope as held — and the stripes it covers.
        guards.push(register_local_scope(scope));
        guards.extend(vol.dlm().lock_many(&local_inos, &local_dents).await);
        note_local_scope_stripes(scope, vol.dlm(), v_idx, &local_inos, &local_dents);
        return Ok(guards);
    }
    let Some(router) = XV_SHIPPER.load_full() else {
        let holder = *remote.keys().next().unwrap_or(&0);
        return Err(unreachable_error(holder, "4a guards"));
    };
    guards.push(register_local_scope(scope));
    let mut local_taken = false;
    for (holder, set) in remote {
        if !local_taken && own_id < holder {
            guards.extend(vol.dlm().lock_many(&local_inos, &local_dents).await);
            note_local_scope_stripes(scope, vol.dlm(), v_idx, &local_inos, &local_dents);
            local_taken = true;
        }
        let peer = Arc::new(crate::meta_ship::PeerOwner::new(
            format!("appender-{holder}"),
            &*set.endpoint,
        ));
        let release_ino = set
            .inos
            .first()
            .map(|(i, _)| *i)
            .or_else(|| set.dents.first().map(|(p, _, _)| *p))
            .unwrap_or(0);
        let op = crate::meta_ship::MetaOp {
            id: router.next_request_id(),
            call: crate::meta_ship::MetaCall::XvGuards {
                scope,
                inodes: set.inos,
                dentries: set.dents,
            },
        };
        let t = std::time::Instant::now();
        XV_CO_GUARD_RPCS.fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "cross-owner guards: shipping scope {scope:#x} to appender {holder} at {} (local \
             table taken first: {local_taken})",
            peer.endpoint
        );
        let acquired = match router.ship_ops(&peer, vec![op]).await {
            Ok(mut results) => match results.pop() {
                Some(r) => r
                    .outcome
                    .map_err(|e| e.into_error())
                    .and_then(|reply| match reply {
                        crate::meta_ship::MetaReply::Unit => Ok(()),
                        other => Err(SqueezefsError::InvalidOperation(format!(
                            "cross-owner guards: the holder answered {other:?} where Unit was due"
                        ))),
                    }),
                None => Err(SqueezefsError::InvalidOperation(
                    "cross-owner guards: the holder returned an empty result set for a one-op \
                     batch"
                        .into(),
                )),
            },
            Err(e) => Err(e),
        };
        phase_record(XvPhase::GuardRtt, t);
        if let Err(e) = acquired {
            // The initiator gives up on this holder (a wire timeout, a
            // refusal): the holder's serve may still park the guards
            // late, so a fire-and-forget release rides behind (review
            // round 1, Issue 7) — the drop of `guards` releases every
            // table acquired before this one the same way.
            spawn_scope_release(Arc::clone(&router), Arc::clone(&peer), scope, release_ino);
            return Err(e);
        }
        // The release rides the guard's drop — the op's terminal outcome
        // — as one fire-and-forget verb; the holder's lease-expiry sweep
        // is the backstop for an initiator that dies first.
        let router_for_release = Arc::clone(&router);
        guards.push(dlm::DlmGuard::external(scope, move || {
            spawn_scope_release(router_for_release, peer, scope, release_ino);
        }));
    }
    if !local_taken {
        guards.extend(vol.dlm().lock_many(&local_inos, &local_dents).await);
        note_local_scope_stripes(scope, vol.dlm(), v_idx, &local_inos, &local_dents);
    }
    Ok(guards)
}

/// One fire-and-forget `XvRelease` of `scope` at `peer`.
fn spawn_scope_release(
    router: Arc<crate::meta_ship::MetaShipRouter>,
    peer: Arc<crate::meta_ship::PeerOwner>,
    scope: u64,
    release_ino: u64,
) {
    crate::meta_exec::spawn_meta("xv_guard_release", async move {
        let hold = TEST_XV_RELEASE_HOLD_MS.load(Ordering::Relaxed);
        if hold > 0 {
            squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold)).await;
        }
        let op = crate::meta_ship::MetaOp {
            id: router.next_request_id(),
            call: crate::meta_ship::MetaCall::XvRelease {
                scope,
                ino: release_ino,
            },
        };
        if let Err(e) = router.ship_ops(&peer, vec![op]).await {
            log::warn!(
                "cross-owner guards: releasing scope {scope:#x} at {} failed ({e}) — the \
                 holder's lease-expiry sweep releases it",
                peer.endpoint
            );
        }
    });
}

// ---------------------------------------------------------------------------
// The intent record wire (`SQZXTX01`)
// ---------------------------------------------------------------------------

/// The reserved local ino the intent records key on. Inodes are minted
/// from 2 (1 is the root), so ino 0 is a keyspace no inode can ever own:
/// intents live in `TREE_XATTRS` at `(0, tx_id)` — a key derived ENTIRELY
/// from the tx id, so writing one needs **no collision-chain probe** and
/// therefore **no new lock**, which is what keeps the machinery free of
/// new lock-order edges (see the acquisition-order rule in §4.10a).
pub const XV_INTENT_INO: Ino = 0;

/// Intent-record magic.
pub const XV_MAGIC: [u8; 8] = *b"SQZXTX01";
/// Intent-record format version (`decode` refuses anything else loud —
/// a future version's plan cannot be safely guessed).
pub const XV_VERSION: u16 = 1;
/// Header length: magic 8 | version 2 | op 1 | flags 1 | tx_id 8 |
/// step_count 2 | reserved 2 | checksum 8.
const XV_HEADER_LEN: usize = 32;
/// Offset of the checksum field inside the header.
const XV_CHECKSUM_OFF: usize = 24;
/// Encoding-budget cap on a plan's step count — the decoder's bound, not a
/// tuning knob. The largest plan the converted ops build is 9 (a
/// cross-volume directory rename replacing a destination with
/// `RENAME_WHITEOUT`); symmetric PR 7b's striped-directory plans are the
/// widest: a flip writes `K + 2` marker inserts and the ONE-intent
/// `rmdir` of a striped directory (review round 1, Issue 2) removes the
/// parent dentry + `K + 2` markers and writes `K + 1` counts (`SetNlink`
/// on every stripe and on the directory), with `K ≤ MINT_SPREAD` (64) —
/// so the bound is `2 × MINT_SPREAD + 8`, the widest plan plus the pre-7b
/// headroom (≈ 5 KiB encoded at the widest, inside the KV value cap).
pub const XV_MAX_STEPS: usize = 2 * super::MINT_SPREAD + 8;

/// The intent key of `tx_id`: the id IS the key (56 bits in the hash
/// field, 8 in the collision field — the full 64 bits, injectively).
pub fn intent_key(tx_id: u64) -> [u8; XATTR_KEY_LEN] {
    intent_key_at(XV_INTENT_INO, tx_id)
}

/// [`intent_key`] homed on `intent_ino` — [`XV_INTENT_INO`] on the flat
/// and unarmed paths; under the armed symmetric plane the reserved local
/// 0 of the initiator's SLOT namespace ([`intent_ino_for_slot`]), so the
/// intent and the initiator's half are one region's records.
pub fn intent_key_at(intent_ino: Ino, tx_id: u64) -> [u8; XATTR_KEY_LEN] {
    xattr_key(intent_ino, tx_id & HASH56_MAX, (tx_id >> 56) as u8)
}

/// The intent key ino of forest slot `slot`: local 0 of the slot's
/// namespace — the native slot's is [`XV_INTENT_INO`] itself (byte-
/// identical to the shipped path), a guest slot's is `guest_local_ino(s,
/// 0)` (guest cursors mint from 2, so no inode ever owns it — the ino-0
/// argument, per slot).
pub fn intent_ino_for_slot(slot: super::kv::record::ForestSlot) -> Ino {
    if slot == super::kv::record::NATIVE_FOREST_SLOT {
        XV_INTENT_INO
    } else {
        super::guest_local_ino((slot - 1) as u16, 0)
    }
}

/// The intent record's human-readable xattr name (the value's name field;
/// the `job:` record precedent, KD-2 — offline-probe readable).
pub fn intent_name(tx_id: u64) -> String {
    format!("xtx:{tx_id:016x}")
}

/// Which product operation a plan implements — informational (recovery
/// rolls the STEPS forward, never the op), but it is what makes a log line
/// or an offline probe legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XvOp {
    Link = 1,
    Unlink = 2,
    Rmdir = 3,
    Rename = 4,
    Exchange = 5,
    /// A create whose parent directory another appender holds (symmetric
    /// PR 6): `[CreateInode @ own, InsertDentry @ foreign]`.
    Create = 6,
    /// PR 7b (design §5.6.5): a directory's stripe FLIP (`K + 2` marker
    /// inserts, the commit marker last), one name's lazy re-homing
    /// (`[InsertDentry @ stripe, RemoveDentry @ dir]`), or the migration
    /// flag's clear — every one rolled forward whole by the same
    /// machinery.
    StripeDir = 7,
}

impl XvOp {
    fn code(self) -> u8 {
        self as u8
    }
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Self::Link,
            2 => Self::Unlink,
            3 => Self::Rmdir,
            4 => Self::Rename,
            5 => Self::Exchange,
            6 => Self::Create,
            7 => Self::StripeDir,
            _ => return None,
        })
    }
}

/// One step of a cross-volume plan, over **global** inos: the participant
/// is resolved by routing at apply time, so global-ino stability (the VL5a
/// law) makes a plan survive a slot migration between crash and recovery.
///
/// `Serialize`/`Deserialize` are the WIRE form of a shipped step
/// (`MetaCall::XvStep`, bincode like every S8 frame); the intent RECORD
/// keeps its own hand-rolled, checksummed codec below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum XvStep {
    /// Remove `name` from `parent` (+ the routed parent update). Witness:
    /// the dentry must name `expect_child`.
    RemoveDentry {
        parent: Ino,
        name: String,
        expect_child: Ino,
        /// [`RoutedParentUpdate`] code — see [`parent_update_from_code`].
        parent_update: u8,
    },
    /// Insert `name` → `child` in `parent` (+ the routed parent update).
    InsertDentry {
        parent: Ino,
        name: String,
        child: Ino,
        ft_bits: u32,
        parent_update: u8,
    },
    /// Absolute link-count write with a CAS witness. `ctime = Some(t)`
    /// additionally bumps ctime monotonically (the `link`/`unlink`/
    /// destination-replace shape); `None` leaves times untouched (the
    /// directory-move parent shift's shape).
    SetNlink {
        ino: Ino,
        pre: u32,
        post: u32,
        ctime: Option<u64>,
    },
    /// Monotone Δctime on a moved inode (idempotent by construction).
    TouchCtime { ino: Ino, ctime: u64 },
    /// Mint an inode record (the `RENAME_WHITEOUT` char-0:0 device).
    /// Witness: the record must be absent.
    MintInode {
        ino: Ino,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    },
    /// The CREATE mint (symmetric PR 6, §5.6's `tx0 = [inode(child), …]`):
    /// a full inode record at the creator's mint instant (`ts_ns`, so a
    /// replayed apply answers the same times — the `IntentCreatePreset`
    /// law) with the record's size (a symlink's target length) and the
    /// directory link count. Witness: the record must be absent; present
    /// ⇒ already applied, answered from the stored record.
    CreateInode {
        ino: Ino,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        size: u64,
        ts_ns: u64,
    },
}

impl XvStep {
    /// The global ino whose volume HOMES this step (dentry steps home on
    /// the parent; the rest on their own inode).
    pub fn home_ino(&self) -> Ino {
        match self {
            Self::RemoveDentry { parent, .. } | Self::InsertDentry { parent, .. } => *parent,
            Self::SetNlink { ino, .. }
            | Self::TouchCtime { ino, .. }
            | Self::MintInode { ino, .. }
            | Self::CreateInode { ino, .. } => *ino,
        }
    }

    /// The step's name (log lines).
    pub fn name(&self) -> &'static str {
        match self {
            Self::RemoveDentry { .. } => "remove_dentry",
            Self::InsertDentry { .. } => "insert_dentry",
            Self::SetNlink { .. } => "set_nlink",
            Self::TouchCtime { .. } => "touch_ctime",
            Self::MintInode { .. } => "mint_inode",
            Self::CreateInode { .. } => "create_inode",
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Self::RemoveDentry { .. } => 1,
            Self::InsertDentry { .. } => 2,
            Self::SetNlink { .. } => 3,
            Self::TouchCtime { .. } => 4,
            Self::MintInode { .. } => 5,
            Self::CreateInode { .. } => 6,
        }
    }
}

/// [`RoutedParentUpdate`] ⇄ wire code (the record must not depend on a
/// Rust enum's discriminant).
pub fn parent_update_code(u: RoutedParentUpdate) -> u8 {
    match u {
        RoutedParentUpdate::None => 0,
        RoutedParentUpdate::SharedTimes => 1,
        RoutedParentUpdate::ExclusiveTimes => 2,
        RoutedParentUpdate::ExclusiveTimesBump => 3,
    }
}

/// [`parent_update_code`]'s inverse; an unknown code decodes as `None`
/// (times are cosmetic — a decoder must never fail a recovery over one).
pub fn parent_update_from_code(c: u8) -> RoutedParentUpdate {
    match c {
        1 => RoutedParentUpdate::SharedTimes,
        2 => RoutedParentUpdate::ExclusiveTimes,
        3 => RoutedParentUpdate::ExclusiveTimesBump,
        _ => RoutedParentUpdate::None,
    }
}

/// A durable cross-volume intent: the whole plan, checksummed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentRecord {
    pub tx_id: u64,
    pub op: XvOp,
    pub steps: Vec<XvStep>,
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_name(out: &mut Vec<u8>, name: &str) -> Result<()> {
    if name.len() > 255 {
        return Err(SqueezefsError::InvalidOperation(format!(
            "cross-volume intent: name of {} bytes exceeds 255",
            name.len()
        )));
    }
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

/// Bounded reader: every decode path is length-checked, so hostile bytes
/// are an error and never a panic (the standing decoder law).
struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn corrupt(what: &str) -> SqueezefsError {
        SqueezefsError::InvalidOperation(format!("cross-volume intent record: truncated {what}"))
    }
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| Self::corrupt(what))?;
        if end > self.b.len() {
            return Err(Self::corrupt(what));
        }
        let out = &self.b[self.at..end];
        self.at = end;
        Ok(out)
    }
    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &str) -> Result<u16> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self, what: &str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self, what: &str) -> Result<u64> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn name(&mut self, what: &str) -> Result<String> {
        let len = usize::from(self.u8(what)?);
        let raw = self.take(len, what)?;
        std::str::from_utf8(raw)
            .map(|s| s.to_string())
            .map_err(|_| {
                SqueezefsError::InvalidOperation(format!(
                    "cross-volume intent record: {what} is not UTF-8"
                ))
            })
    }
}

impl IntentRecord {
    /// Encode the record; the checksum covers the whole image with its own
    /// field zeroed (the format's standing digest discipline).
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.steps.is_empty() || self.steps.len() > XV_MAX_STEPS {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cross-volume intent: {} steps (admissible 1..={XV_MAX_STEPS})",
                self.steps.len()
            )));
        }
        let mut out = Vec::with_capacity(XV_HEADER_LEN + self.steps.len() * 32);
        out.extend_from_slice(&XV_MAGIC);
        put_u16(&mut out, XV_VERSION);
        out.push(self.op.code());
        out.push(0); // flags
        put_u64(&mut out, self.tx_id);
        put_u16(&mut out, self.steps.len() as u16);
        put_u16(&mut out, 0); // reserved
        put_u64(&mut out, 0); // checksum, filled below
        debug_assert_eq!(out.len(), XV_HEADER_LEN);
        for s in &self.steps {
            out.push(s.kind());
            match s {
                XvStep::RemoveDentry {
                    parent,
                    name,
                    expect_child,
                    parent_update,
                } => {
                    put_u64(&mut out, *parent);
                    put_u64(&mut out, *expect_child);
                    out.push(*parent_update);
                    put_name(&mut out, name)?;
                }
                XvStep::InsertDentry {
                    parent,
                    name,
                    child,
                    ft_bits,
                    parent_update,
                } => {
                    put_u64(&mut out, *parent);
                    put_u64(&mut out, *child);
                    put_u32(&mut out, *ft_bits);
                    out.push(*parent_update);
                    put_name(&mut out, name)?;
                }
                XvStep::SetNlink {
                    ino,
                    pre,
                    post,
                    ctime,
                } => {
                    put_u64(&mut out, *ino);
                    put_u32(&mut out, *pre);
                    put_u32(&mut out, *post);
                    out.push(u8::from(ctime.is_some()));
                    put_u64(&mut out, ctime.unwrap_or(0));
                }
                XvStep::TouchCtime { ino, ctime } => {
                    put_u64(&mut out, *ino);
                    put_u64(&mut out, *ctime);
                }
                XvStep::MintInode {
                    ino,
                    mode,
                    uid,
                    gid,
                    rdev,
                } => {
                    put_u64(&mut out, *ino);
                    put_u32(&mut out, *mode);
                    put_u32(&mut out, *uid);
                    put_u32(&mut out, *gid);
                    put_u32(&mut out, *rdev);
                }
                XvStep::CreateInode {
                    ino,
                    mode,
                    uid,
                    gid,
                    rdev,
                    size,
                    ts_ns,
                } => {
                    put_u64(&mut out, *ino);
                    put_u32(&mut out, *mode);
                    put_u32(&mut out, *uid);
                    put_u32(&mut out, *gid);
                    put_u32(&mut out, *rdev);
                    put_u64(&mut out, *size);
                    put_u64(&mut out, *ts_ns);
                }
            }
        }
        let sum = xxhash_rust::xxh3::xxh3_64(&out);
        out[XV_CHECKSUM_OFF..XV_CHECKSUM_OFF + 8].copy_from_slice(&sum.to_le_bytes());
        Ok(out)
    }

    /// Decode + verify. A wrong magic, an unknown version, a bad checksum
    /// or a truncated image is an error, never a guess: whole-tx atomicity
    /// means a torn intent cannot exist, so a bad one is real corruption
    /// and the mount says so loud.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Rd { b: bytes, at: 0 };
        let magic = r.take(8, "magic")?;
        if magic != XV_MAGIC {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cross-volume intent record: bad magic {magic:02x?} (expected SQZXTX01)"
            )));
        }
        let version = r.u16("version")?;
        if version != XV_VERSION {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cross-volume intent record: version {version} is newer than this binary \
                 understands ({XV_VERSION}) — refusing to guess a half-applied \
                 transaction's plan"
            )));
        }
        let op = XvOp::from_code(r.u8("op")?).ok_or_else(|| {
            SqueezefsError::InvalidOperation("cross-volume intent record: unknown op".to_string())
        })?;
        let _flags = r.u8("flags")?;
        let tx_id = r.u64("tx_id")?;
        let step_count = usize::from(r.u16("step_count")?);
        let _reserved = r.u16("reserved")?;
        let stored_sum = r.u64("checksum")?;
        if step_count == 0 || step_count > XV_MAX_STEPS {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cross-volume intent record: {step_count} steps (admissible 1..={XV_MAX_STEPS})"
            )));
        }
        let mut zeroed = bytes.to_vec();
        zeroed[XV_CHECKSUM_OFF..XV_CHECKSUM_OFF + 8].fill(0);
        if xxhash_rust::xxh3::xxh3_64(&zeroed) != stored_sum {
            return Err(SqueezefsError::InvalidOperation(
                "cross-volume intent record: checksum mismatch".to_string(),
            ));
        }
        let mut steps = Vec::with_capacity(step_count);
        for _ in 0..step_count {
            let kind = r.u8("step kind")?;
            steps.push(match kind {
                1 => XvStep::RemoveDentry {
                    parent: r.u64("parent")?,
                    expect_child: r.u64("expect_child")?,
                    parent_update: r.u8("parent_update")?,
                    name: r.name("name")?,
                },
                2 => XvStep::InsertDentry {
                    parent: r.u64("parent")?,
                    child: r.u64("child")?,
                    ft_bits: r.u32("ft_bits")?,
                    parent_update: r.u8("parent_update")?,
                    name: r.name("name")?,
                },
                3 => {
                    let ino = r.u64("ino")?;
                    let pre = r.u32("pre")?;
                    let post = r.u32("post")?;
                    let has_ctime = r.u8("ctime flag")? != 0;
                    let ct = r.u64("ctime")?;
                    XvStep::SetNlink {
                        ino,
                        pre,
                        post,
                        ctime: has_ctime.then_some(ct),
                    }
                }
                4 => XvStep::TouchCtime {
                    ino: r.u64("ino")?,
                    ctime: r.u64("ctime")?,
                },
                5 => XvStep::MintInode {
                    ino: r.u64("ino")?,
                    mode: r.u32("mode")?,
                    uid: r.u32("uid")?,
                    gid: r.u32("gid")?,
                    rdev: r.u32("rdev")?,
                },
                6 => XvStep::CreateInode {
                    ino: r.u64("ino")?,
                    mode: r.u32("mode")?,
                    uid: r.u32("uid")?,
                    gid: r.u32("gid")?,
                    rdev: r.u32("rdev")?,
                    size: r.u64("size")?,
                    ts_ns: r.u64("ts_ns")?,
                },
                other => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "cross-volume intent record: unknown step kind {other}"
                    )))
                }
            });
        }
        if r.at != bytes.len() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "cross-volume intent record: {} trailing byte(s)",
                bytes.len() - r.at
            )));
        }
        Ok(Self { tx_id, op, steps })
    }
}

// ---------------------------------------------------------------------------
// Localised steps + the applier's contract (implemented in kv::backend)
// ---------------------------------------------------------------------------

/// A step with its inos resolved to ONE volume's local keyspace — what
/// [`KvMetaBackend::xv_apply_step`] consumes. Dentry values still carry
/// GLOBAL child inos (the routed dentry format).
#[derive(Debug, Clone)]
pub enum XvLocalStep {
    RemoveDentry {
        local_parent: Ino,
        name: String,
        expect_child: Ino,
        parent_update: RoutedParentUpdate,
    },
    InsertDentry {
        local_parent: Ino,
        name: String,
        child: Ino,
        ft_bits: u32,
        parent_update: RoutedParentUpdate,
    },
    SetNlink {
        local_ino: Ino,
        pre: u32,
        post: u32,
        ctime: Option<u64>,
    },
    TouchCtime {
        local_ino: Ino,
        ctime: u64,
    },
    MintInode {
        local_ino: Ino,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    },
    CreateInode {
        local_ino: Ino,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        size: u64,
        ts_ns: u64,
    },
}

impl XvLocalStep {
    /// The LOCAL key ino the step's records live under (its slot's).
    pub fn local_home(&self) -> Ino {
        match self {
            Self::RemoveDentry { local_parent, .. } | Self::InsertDentry { local_parent, .. } => {
                *local_parent
            }
            Self::SetNlink { local_ino, .. }
            | Self::TouchCtime { local_ino, .. }
            | Self::MintInode { local_ino, .. }
            | Self::CreateInode { local_ino, .. } => *local_ino,
        }
    }

    /// Whether the step writes a record for an ino THIS op minted — one
    /// nothing else can name until the op's own dentry step lands. The 4a
    /// law takes no `I{ino}` guard on a fresh mint (the local create path
    /// holds none), so a coverage check has nothing to judge here.
    pub fn is_fresh_mint(&self) -> bool {
        matches!(self, Self::MintInode { .. } | Self::CreateInode { .. })
    }
}

/// The intent record's rider on a step's transaction: the `Put` rides step
/// 0 (intent and first effect are then ONE checksummed entry — atomic by
/// construction, and free), the `Delete` is the retirement. `intent_ino`
/// is the key's home ([`intent_key_at`]).
#[derive(Debug, Clone)]
pub enum XvRider {
    Put {
        intent_ino: Ino,
        tx_id: u64,
        image: Vec<u8>,
    },
    Delete {
        intent_ino: Ino,
        tx_id: u64,
    },
}

/// What the witness decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XvStepStatus {
    /// The step's effect was committed by this call.
    Applied,
    /// The effect was already present (recovery meeting its own work, or
    /// the live path meeting a redundant step).
    AlreadyApplied,
    /// The object moved under the plan: skipped LOUD, never clobbered.
    ForeignSkipped,
}

/// One step's outcome; `inode` carries the post-image where the caller
/// needs it (the `link` reply).
#[derive(Debug, Clone)]
pub struct XvStepOutcome {
    pub status: XvStepStatus,
    pub inode: Option<InodeValue>,
}

impl XvStepOutcome {
    /// Count the outcome on the stats-inode ledger.
    pub fn count(&self) {
        match self.status {
            XvStepStatus::Applied => XV_STEPS_APPLIED.fetch_add(1, Ordering::Relaxed),
            XvStepStatus::AlreadyApplied => {
                XV_STEPS_ALREADY_APPLIED.fetch_add(1, Ordering::Relaxed)
            }
            XvStepStatus::ForeignSkipped => {
                XV_STEPS_FOREIGN_SKIPPED.fetch_add(1, Ordering::Relaxed)
            }
        };
    }
}

// ---------------------------------------------------------------------------
// Plans and execution
// ---------------------------------------------------------------------------

/// An ordered cross-volume plan. Step 0's volume is the **coordinator**:
/// it hosts the intent record, and the effect order is the caller's
/// (unchanged from the pre-S3.5 fragment order, so no user-visible
/// sequencing moves).
#[derive(Debug, Clone)]
pub struct XvPlan {
    pub op: XvOp,
    pub steps: Vec<XvStep>,
}

/// A completed execution: one outcome per step, in plan order.
#[derive(Debug, Clone)]
pub struct XvExecution {
    pub tx_id: u64,
    pub outcomes: Vec<XvStepOutcome>,
}

impl XvExecution {
    /// The post-image of the `idx`-th step, where it produced one.
    pub fn inode(&self, idx: usize) -> Option<&InodeValue> {
        self.outcomes.get(idx).and_then(|o| o.inode.as_ref())
    }
}

/// The seam's severance error — distinguishable, and deliberately NOT an
/// escalation: it models a dead process.
fn seam_error() -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(
        "cross-volume transaction severed at its commit-boundary test seam",
    ))
}

/// Monotone-per-process transaction ids. Composed with the volume's
/// durable writer term (S2) so ids never repeat across mounts; seeded
/// from the clock so that even a term-less (pre-bit-7) volume cannot
/// collide with an id a previous mount left behind.
static XV_SEQ: AtomicU64 = AtomicU64::new(0);
const XV_SEQ_BITS: u32 = 40;
const XV_SEQ_MASK: u64 = (1 << XV_SEQ_BITS) - 1;

fn next_seq() -> u64 {
    let mut cur = XV_SEQ.load(Ordering::Acquire);
    loop {
        let next = if cur == 0 {
            // First use: seed from the clock, so that even a term-less
            // (pre-incompat-bit-7) volume cannot mint an id a previous
            // mount already used. `| 1` keeps the seed non-zero.
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1))
                | 1
        } else {
            cur.wrapping_add(1)
        };
        match XV_SEQ.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(observed) => cur = observed,
        }
    }
}

fn mint_tx_id(coordinator: &KvMetaBackend) -> u64 {
    (coordinator.writer_term() << XV_SEQ_BITS) | (next_seq() & XV_SEQ_MASK)
}

/// Localise `step` for the volume that homes it.
///
/// Routing here (rather than at plan time) is deliberate twice over: the
/// live path's routes are pinned for the operation's duration by the
/// §5.5.2a cutover gate the operation entered before it took a lock, so
/// re-deriving them is free and cannot disagree; and the RECOVERY path
/// gets slot-migration tolerance for nothing — an intent written before a
/// slot moved resolves to the slot's new home.
fn localise(routed: &RoutedMetaBackend, step: &XvStep) -> (usize, XvLocalStep) {
    match step {
        XvStep::RemoveDentry {
            parent,
            name,
            expect_child,
            parent_update,
        } => {
            let (v, local_parent) = routed.route_ino(*parent);
            (
                v,
                XvLocalStep::RemoveDentry {
                    local_parent,
                    name: name.clone(),
                    expect_child: *expect_child,
                    parent_update: parent_update_from_code(*parent_update),
                },
            )
        }
        XvStep::InsertDentry {
            parent,
            name,
            child,
            ft_bits,
            parent_update,
        } => {
            let (v, local_parent) = routed.route_ino(*parent);
            (
                v,
                XvLocalStep::InsertDentry {
                    local_parent,
                    name: name.clone(),
                    child: *child,
                    ft_bits: *ft_bits,
                    parent_update: parent_update_from_code(*parent_update),
                },
            )
        }
        XvStep::SetNlink {
            ino,
            pre,
            post,
            ctime,
        } => {
            let (v, local_ino) = routed.route_ino(*ino);
            (
                v,
                XvLocalStep::SetNlink {
                    local_ino,
                    pre: *pre,
                    post: *post,
                    ctime: *ctime,
                },
            )
        }
        XvStep::TouchCtime { ino, ctime } => {
            let (v, local_ino) = routed.route_ino(*ino);
            (
                v,
                XvLocalStep::TouchCtime {
                    local_ino,
                    ctime: *ctime,
                },
            )
        }
        XvStep::MintInode {
            ino,
            mode,
            uid,
            gid,
            rdev,
        } => {
            let (v, local_ino) = routed.route_ino(*ino);
            (
                v,
                XvLocalStep::MintInode {
                    local_ino,
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                    rdev: *rdev,
                },
            )
        }
        XvStep::CreateInode {
            ino,
            mode,
            uid,
            gid,
            rdev,
            size,
            ts_ns,
        } => {
            let (v, local_ino) = routed.route_ino(*ino);
            (
                v,
                XvLocalStep::CreateInode {
                    local_ino,
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                    rdev: *rdev,
                    size: *size,
                    ts_ns: *ts_ns,
                },
            )
        }
    }
}

/// The public face of `localise` — the served side of a shipped step
/// resolves the same way (`meta_ship::service`).
pub fn localise_step(routed: &RoutedMetaBackend, step: &XvStep) -> (usize, XvLocalStep) {
    localise(routed, step)
}

// ---------------------------------------------------------------------------
// Where a step executes (symmetric PR 6): own slot ⇒ here; a slot another
// appender leases ⇒ its holder, over the wire.
// ---------------------------------------------------------------------------

/// A step's home under the armed plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepHome {
    /// This mount leases the step's slot (or nobody does — the door
    /// acquires it first-touch): the local applier.
    Local,
    /// Appender `holder` leases the slot and serves at `endpoint`.
    Foreign { holder: u32, endpoint: Arc<str> },
    /// Appender `holder` leases the slot and this mount knows no endpoint
    /// for it (the join ladder has not bound it; a dead member's) — the
    /// un-shippable class the cadence retries.
    Unreachable { holder: u32 },
}

/// The forest slot of a LOCAL key ino on `v_idx`, and whether that slot's
/// holder is another appender (`None` = unarmed, or ours).
pub(crate) fn foreign_holder_of(
    routed: &RoutedMetaBackend,
    v_idx: usize,
    local_ino: Ino,
) -> Option<crate::slot_holder_cache::SlotHolder> {
    let vol = routed.volumes.get(v_idx)?;
    let plane = vol.slot_leases()?;
    let slot = super::kv::record::forest_slot_of_ino(local_ino);
    // The lease TABLE, not the gate: the gate says "some region of this
    // mount", and a declared region's slot is another appender's for
    // every cross-owner decision (the two-holder model in one process).
    match plane.table.resolve(slot) {
        crate::slot_lease_core::Resolved::Holder { holder, g }
            if holder != vol.own_appender_id() =>
        {
            Some(crate::slot_holder_cache::SlotHolder {
                appender_id: holder,
                g,
            })
        }
        _ => None,
    }
}

/// Where a step homed on LOCAL key ino `local_ino` of `v_idx` executes.
pub fn step_home(routed: &RoutedMetaBackend, v_idx: usize, local_ino: Ino) -> StepHome {
    let Some(holder) = foreign_holder_of(routed, v_idx, local_ino) else {
        return StepHome::Local;
    };
    let endpoint = routed.volumes[v_idx]
        .slot_leases()
        .and_then(|p| p.holders.endpoint(holder.appender_id));
    match endpoint {
        Some(endpoint) => StepHome::Foreign {
            holder: holder.appender_id,
            endpoint,
        },
        None => StepHome::Unreachable {
            holder: holder.appender_id,
        },
    }
}

/// [`step_home`] with the holder's endpoint bound ON DEMAND when the
/// table does not know it (an appender that joined after this mount's
/// ladder ran — PR 12b, N ≥ 3; `sym_join::bind_holder_endpoint_on_demand`
/// off durable state, once). The async form for the sites that SHIP;
/// the sync form stays the `Local`-or-not predicate.
pub async fn step_home_bound(routed: &RoutedMetaBackend, v_idx: usize, local_ino: Ino) -> StepHome {
    match step_home(routed, v_idx, local_ino) {
        StepHome::Unreachable { holder } if !routed.volumes[v_idx].is_own_region(holder) => {
            crate::sym_join::bind_holder_endpoint_on_demand(&routed.volumes[v_idx], holder).await;
            step_home(routed, v_idx, local_ino)
        }
        home => home,
    }
}

/// Whether any of `inos` (GLOBAL) lives in a slot another appender
/// leases — the arms' "this op is cross-owner" predicate. One `Option`
/// test per ino on an unarmed mount.
pub fn spans_foreign_slot(routed: &RoutedMetaBackend, inos: &[Ino]) -> bool {
    inos.iter().any(|&ino| {
        let (v, local) = routed.route_ino(ino);
        foreign_holder_of(routed, v, local).is_some()
    })
}

/// The error a step whose holder is unreachable answers (EAGAIN-class:
/// the intent stays open, the cadence retries).
fn unreachable_error(holder: u32, what: &str) -> SqueezefsError {
    SqueezefsError::refused(
        libc::EAGAIN,
        format!(
            "cross-owner {what} homes on a slot appender {holder} leases and this mount \
             cannot reach it (no endpoint bound, or no shipper installed) — the intent stays \
             open and the roll-forward cadence re-ships it once the holder is bound \
             (xv_cross_owner_intents_stuck past the grace window; PR 10's recovery re-leases \
             a dead holder's slots)"
        ),
    )
}

/// The EXACT `(parent, name)` resolution (design §5.6.4's ancestor
/// check): a parent in a slot this mount leases answers from its own
/// RAM-authoritative tree; a foreign parent's answer is its holder's,
/// over the wire (`MetaCall::LookupExact`, one RPC per link).
pub async fn lookup_exact(
    routed: &RoutedMetaBackend,
    parent: Ino,
    name: &str,
) -> Result<Option<(Ino, u32)>> {
    let (v_idx, local_parent) = routed.route_ino(parent);
    let (holder, endpoint) = match step_home_bound(routed, v_idx, local_parent).await {
        // Guard-free (Issue 2): the walk holds the initiator's own
        // exclusive `D{}` guards; a shared guard here could park behind
        // them on a stripe collision. The set-wide lease is the read's
        // consistency.
        StepHome::Local => return routed.lookup_dentry_exact_unguarded(parent, name).await,
        StepHome::Foreign { holder, endpoint } => (holder, endpoint),
        StepHome::Unreachable { holder } => {
            return Err(SqueezefsError::refused(
                libc::EAGAIN,
                format!(
                    "exact lookup of {name:?} under {parent}: its slot's holder (appender \
                     {holder}) has no endpoint bound on this mount"
                ),
            ))
        }
    };
    let Some(router) = XV_SHIPPER.load_full() else {
        return Err(unreachable_error(holder, "exact lookup"));
    };
    let peer = Arc::new(crate::meta_ship::PeerOwner::new(
        format!("appender-{holder}"),
        &*endpoint,
    ));
    let op = crate::meta_ship::MetaOp {
        id: router.next_request_id(),
        call: crate::meta_ship::MetaCall::LookupExact {
            parent,
            name: name.to_string(),
        },
    };
    let mut results = router.ship_ops(&peer, vec![op]).await?;
    let result = results.pop().ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "exact lookup: the holder returned an empty result set for a one-op batch".into(),
        )
    })?;
    match result.outcome {
        Ok(crate::meta_ship::MetaReply::DentryExact(d)) => Ok(d),
        Ok(other) => Err(SqueezefsError::InvalidOperation(format!(
            "exact lookup: the holder answered {other:?} where a dentry was due"
        ))),
        Err(e) => Err(e.into_error()),
    }
}

/// Ship ONE metadata call to appender `holder` at `endpoint` through the
/// installed shipper and answer its reply — the striping verbs' wire
/// (PR 7b: `SupplyStripeIno` / `IsEmpty` / `DestroyStripe`), the same
/// one-op batch [`ship_step`] and [`lookup_exact`] ride.
pub(crate) async fn ship_meta_call(
    endpoint: &str,
    holder: u32,
    call: crate::meta_ship::MetaCall,
) -> Result<crate::meta_ship::MetaReply> {
    let verb = call.verb().name();
    let Some(router) = XV_SHIPPER.load_full() else {
        return Err(unreachable_error(holder, verb));
    };
    let peer = Arc::new(crate::meta_ship::PeerOwner::new(
        format!("appender-{holder}"),
        endpoint,
    ));
    let op = crate::meta_ship::MetaOp {
        id: router.next_request_id(),
        call,
    };
    let mut results = router.ship_ops(&peer, vec![op]).await?;
    let result = results.pop().ok_or_else(|| {
        SqueezefsError::InvalidOperation(format!(
            "{verb}: the holder returned an empty result set for a one-op batch"
        ))
    })?;
    result.outcome.map_err(|e| e.into_error())
}

/// Ship one step to its holder through the installed shipper.
async fn ship_step(
    endpoint: &str,
    holder: u32,
    tx_id: u64,
    step_idx: usize,
    step: &XvStep,
    scope: u64,
) -> Result<XvStepOutcome> {
    let Some(router) = XV_SHIPPER.load_full() else {
        return Err(unreachable_error(holder, step.name()));
    };
    let peer = Arc::new(crate::meta_ship::PeerOwner::new(
        format!("appender-{holder}"),
        endpoint,
    ));
    let op = crate::meta_ship::MetaOp {
        id: router.next_request_id(),
        call: crate::meta_ship::MetaCall::XvStep {
            tx_id,
            step_idx: step_idx as u32,
            step: step.clone(),
            scope,
        },
    };
    let t = std::time::Instant::now();
    XV_CO_STEPS_SHIPPED.fetch_add(1, Ordering::Relaxed);
    let mut results = router.ship_ops(&peer, vec![op]).await?;
    phase_record(XvPhase::ShipRtt, t);
    let result = results.pop().ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "cross-owner step: the holder returned an empty result set for a one-op batch".into(),
        )
    })?;
    match result.outcome {
        Ok(crate::meta_ship::MetaReply::XvStep { status, inode }) => {
            let status = match status {
                0 => XvStepStatus::Applied,
                1 => XvStepStatus::AlreadyApplied,
                _ => XvStepStatus::ForeignSkipped,
            };
            Ok(XvStepOutcome {
                status,
                inode: inode.map(|w| InodeValue {
                    mode: w.mode,
                    uid: w.uid,
                    gid: w.gid,
                    nlink: w.nlink,
                    flags: w.flags,
                    rdev: w.rdev,
                    size: w.size,
                    atime: w.atime,
                    mtime: w.mtime,
                    ctime: w.ctime,
                }),
            })
        }
        Ok(other) => Err(SqueezefsError::InvalidOperation(format!(
            "cross-owner step: the holder answered {other:?} where a step outcome was due"
        ))),
        Err(e) => Err(e.into_error()),
    }
}

/// Apply one localised step on its volume, or SHIP it to the slot's
/// holder (symmetric PR 6 — the seam the module docs name, made real:
/// the remote leg replaces this call and only this call). `rider` rides
/// a LOCAL step only (an intent is the initiator's ring's record).
/// The arm [`apply_or_ship_step`] TOOK for one attempt — answered beside
/// the outcome so a caller classifies the outcome by the dispatch that
/// produced it (PR 13e review round 1, Issue 3: a second `step_home` read
/// ahead of the dispatch could disagree with the arm the dispatch
/// resolved — a release landing between the two reads dispatched SHIPPED
/// under a `Local` word, and a holder's witness refusal read as a local
/// skip: the "acked with its name nowhere" face, narrowed to a two-read
/// window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// Applied at this mount's door.
    Local,
    /// Shipped to the slot's holder (or found it unreachable — a foreign
    /// home either way).
    Shipped,
}

async fn apply_or_ship_step(
    routed: &RoutedMetaBackend,
    tx_id: u64,
    step_idx: usize,
    v_idx: usize,
    step: &XvStep,
    local: &XvLocalStep,
    rider: Option<&XvRider>,
    guards: Arc<[dlm::DlmGuard]>,
) -> (Dispatch, Result<XvStepOutcome>) {
    match step_home_bound(routed, v_idx, local.local_home()).await {
        StepHome::Local => (
            Dispatch::Local,
            apply_step_locally(routed, tx_id, step_idx, v_idx, step, local, rider, guards).await,
        ),
        // The HOLDER counts the outcome on the S3.5 step ledger (the
        // effect committed there); the initiator counts the ship. The
        // step travels under the op's guard scope.
        StepHome::Foreign { holder, endpoint } => (
            Dispatch::Shipped,
            ship_step(&endpoint, holder, tx_id, step_idx, step, scope_of(&guards)).await,
        ),
        StepHome::Unreachable { holder } => (
            Dispatch::Shipped,
            Err(unreachable_error(holder, step.name())),
        ),
    }
}

/// The LOCAL arm of [`apply_or_ship_step`].
async fn apply_step_locally(
    routed: &RoutedMetaBackend,
    tx_id: u64,
    step_idx: usize,
    v_idx: usize,
    step: &XvStep,
    local: &XvLocalStep,
    rider: Option<&XvRider>,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<XvStepOutcome> {
    // The op's guard set must cover the step's keys (Issue 8c). A
    // FRESH MINT is exempt (PR 12): its ino was allocated by this
    // op and nothing names it until the op's own dentry step
    // lands, so the 4a law takes no `I{ino}` on it — the local
    // create path holds none either — and there is no guard the
    // scope could cover. A key the scope does NOT cover is a slot
    // that moved TO this initiator between the acquisition and
    // this step — its guards travelled to the old holder (PR 13b:
    // defect 29's re-dispatch, one step further — the dominance
    // rule or PR 10's recovery handed the slot to the initiator
    // under a shipped step; the `sym-storm` manager applied its
    // `set_nlink` unguarded and tripped `xv_local_step_unguarded`).
    // A legal schedule, so the step takes its OWN guards for the
    // apply — in the NON-PARKING canonical form: a parking acquire
    // while the op holds its other guards could cycle with a
    // peer's canonical set. A contended stripe is the slot-moved
    // retryable class (the bounded re-dispatch; past it the intent
    // stays open for the cadence), never an unguarded apply.
    let scope = scope_of(&guards);
    let mut late_guards: Vec<dlm::DlmGuard> = Vec::new();
    if scope != 0 && !local.is_fresh_mint() {
        let dlm = routed.volumes[v_idx].dlm();
        let needed = step_stripes(dlm, v_idx, local);
        // Covered by the op's local set, or by its OWN scope parked
        // in this table (the one-process holder model) — nothing to
        // take; only a key guarded NOWHERE here takes late guards.
        if local_scope_covers(scope, &needed) == Some(false)
            && !own_parked_scope_covers(scope, &needed)
        {
            let (inos, dents) = step_guard_keys(local);
            let d: Vec<(Ino, &str, dlm::LockMode)> = dents
                .iter()
                .map(|(p, n)| (*p, n.as_str(), dlm::LockMode::Exclusive))
                .collect();
            match dlm.try_lock_many(&inos, &d) {
                Some(g) => {
                    XV_CO_STEP_LATE_GUARDS.fetch_add(1, Ordering::Relaxed);
                    late_guards = g;
                }
                None => {
                    XV_CO_STEP_LATE_GUARD_REFUSALS.fetch_add(1, Ordering::Relaxed);
                    let slot =
                        crate::meta_backend::kv::record::forest_slot_of_ino(local.local_home());
                    return Err(SqueezefsError::retryable(
                        crate::error::RefusalClass::SlotMoved { slot, holder: 0 },
                        format!(
                            "cross-owner transaction {tx_id:016x}: local step \
                             {step_idx} ({}) of slot {slot} — moved to this mount \
                             mid-plan, its guards travelled to the old holder — found \
                             its own guards contended; retry \
                             (xv_cross_owner_step_late_guard_refusals)",
                            step.name()
                        ),
                    ));
                }
            }
        }
    }
    if TEST_XV_LOCAL_STEP_SLOT_BUSY
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
    {
        // The door's own word for a slot that moved: refused
        // before any effect, exactly as `ensure_leases_for_tx`
        // answers a foreign slot.
        return Err(crate::meta_backend::kv::KvError::SlotBusy {
            slot: crate::meta_backend::kv::record::forest_slot_of_ino(local.local_home()),
            holder: 0,
            g: 0,
        }
        .into());
    }
    let out = routed.volumes[v_idx]
        .xv_apply_step(local, rider, guards, false)
        .await;
    // The step's own guards (if any) live to its terminal outcome.
    drop(late_guards);
    if out.is_err() {
        routed.mirror_volume_failure(v_idx);
    }
    let out = out?;
    out.count();
    Ok(out)
}

/// [`apply_or_ship_step`] with the SLOT-MOVED retry (PR 13, defect 29 —
/// found by the fleet's `sym-storm` round 1 from zero: a joiner's `rm -rf`
/// of its recovered round directory shipped its removals to the manager,
/// whose dominance rule handed the slot to the INITIATOR mid-plan; the
/// manager's commit door then answered the shipped step `SlotBusy { slot,
/// holder: the initiator }`, and the initiator — classifying the failure
/// by re-resolving the step's home, which now read `Local` — took it for
/// a local device error and FAIL-STOPPED both volumes). A holder's
/// `SlotBusy` at a shipped step is defect 11's class at the STEP: the slot
/// moved between the plan and the apply — re-resolve it at the manager
/// and dispatch again, locally when it is ours now, to the new holder
/// otherwise; bounded — a second stale answer is the retryable class
/// the caller sees (the intent stays open, the cadence completes it).
/// Answers the outcome WITH the arm the producing attempt TOOK
/// (`Dispatch` — the one `apply_or_ship_step` resolved and dispatched on,
/// never a second table read): a witness refusal is classified by how the
/// step travelled, never by re-resolving its home after the fact (PR
/// 13e, F-R4 — a slot that moved to the initiator during the ship read
/// `Local` after it, and a holder's refusal was taken for a local skip:
/// the plan completed and the create ACKED with its name nowhere).
async fn apply_or_ship_step_retrying(
    routed: &RoutedMetaBackend,
    tx_id: u64,
    step_idx: usize,
    v_idx: usize,
    step: &XvStep,
    local: &XvLocalStep,
    rider: Option<&XvRider>,
    guards: Arc<[dlm::DlmGuard]>,
) -> (Dispatch, Result<XvStepOutcome>) {
    const SLOT_MOVED_RETRIES: usize = 2;
    let mut attempt = 0;
    loop {
        let (dispatch, r) = apply_or_ship_step(
            routed,
            tx_id,
            step_idx,
            v_idx,
            step,
            local,
            rider,
            guards.clone(),
        )
        .await;
        match r {
            // A shipped step's holder (defect 29) OR this mount's own door
            // (defect 35, PR 13): a LOCAL step whose slot another appender
            // took between the plan and the door is the same slot-moved
            // class — the door learnt the holder, the re-resolve confirms
            // it, and the step SHIPS on the next attempt. Before it a
            // local mid-plan `SlotBusy` was a "device error" and the
            // lattice fail-stopped both volumes.
            Err(e) if is_slot_moved_refusal(&e) && attempt < SLOT_MOVED_RETRIES => {
                attempt += 1;
                XV_CO_STEP_SLOT_MOVED_RETRIES.fetch_add(1, Ordering::Relaxed);
                let slot = crate::meta_backend::kv::record::forest_slot_of_ino(local.local_home());
                let _ = routed.volumes[v_idx].reresolve_slot_holder(slot).await;
                log::debug!(
                    "cross-owner transaction {tx_id:016x}: step {step_idx} ({}) was refused \
                     {} because the slot moved ({e}); re-resolved, attempt {attempt} of \
                     {SLOT_MOVED_RETRIES}",
                    step.name(),
                    match dispatch {
                        Dispatch::Shipped => "at its holder",
                        Dispatch::Local => "at this mount's door",
                    }
                );
            }
            other => return (dispatch, other),
        }
    }
}

/// The refusal a step earns at a commit door that does not lease the
/// slot (`KvError::SlotBusy` — the EAGAIN class naming the lessee): the
/// slot moved between the plan and the apply. A shipped step's at its
/// holder (defect 29); a LOCAL step's at this mount's own door (defect
/// 30 — the plan read the slot unleased or ours, the door's first touch
/// lost to another appender, whose identity the door learnt).
pub(crate) fn is_slot_moved_refusal(e: &SqueezefsError) -> bool {
    // The typed class, minted at the ONE `KvError::SlotBusy` conversion
    // (`kv::mod`) and rebuilt from `WireError::class` at a shipped step's
    // initiator (PR 13 review round 1, Issue 7) — a message-text rename
    // can never turn a slot-moved refusal back into a device error.
    matches!(
        e.refusal_class(),
        Some(crate::error::RefusalClass::SlotMoved { .. })
    )
}

/// Count one locally dispatched op re-dispatched through the cross-owner
/// arm after its door's `SlotBusy` (defect 30).
pub(crate) fn note_op_slot_moved_redispatch() {
    XV_CO_OP_SLOT_MOVED_REDISPATCHES.fetch_add(1, Ordering::Relaxed);
}

/// The intent's key ino: `step0`'s slot's local 0 when step 0 is local
/// (the intent rides its entry), or — step 0 another appender's — the
/// mount's own rotor slot on the coordinator volume (the intent's own
/// entry). Flat and unarmed: [`XV_INTENT_INO`].
fn intent_ino_for(
    routed: &RoutedMetaBackend,
    coord: usize,
    step0: Option<&XvLocalStep>,
) -> Result<Ino> {
    let vol = &routed.volumes[coord];
    let Some(plane) = vol.slot_leases() else {
        return Ok(XV_INTENT_INO);
    };
    let slot = match step0 {
        Some(step) => super::kv::record::forest_slot_of_ino(step.local_home()),
        None => *plane.rotor.load().first().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "cross-owner transaction: this mount leases no rotor slot on the coordinator \
                 volume to home its intent in"
                    .into(),
            )
        })?,
    };
    Ok(intent_ino_for_slot(slot))
}

/// The errno a LIVE witness refusal of a shipped step answers: the
/// object moved under the plan at the holder — for an insert the name is
/// taken, for a removal it is gone, for a count the record moved.
fn foreign_skipped_errno(step: &XvStep) -> i32 {
    match step {
        XvStep::InsertDentry { .. } | XvStep::MintInode { .. } | XvStep::CreateInode { .. } => {
            libc::EEXIST
        }
        XvStep::RemoveDentry { .. } => libc::ENOENT,
        XvStep::SetNlink { .. } | XvStep::TouchCtime { .. } => libc::ESTALE,
    }
}

/// Execute a cross-volume transaction under the op's ALREADY-HELD 4a
/// guard set (see §4.10a's acquisition-order rule: this machinery acquires
/// nothing, which is exactly why it adds no wait-for edge).
pub async fn execute(
    routed: &RoutedMetaBackend,
    plan: &XvPlan,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<XvExecution> {
    let t_total = std::time::Instant::now();
    let localised: Vec<(usize, XvLocalStep)> =
        plan.steps.iter().map(|s| localise(routed, s)).collect();
    let Some(coord) = localised.first().map(|(v, _)| *v) else {
        return Ok(XvExecution {
            tx_id: 0,
            outcomes: Vec::new(),
        });
    };
    for &(v, _) in &localised {
        routed.check_volume_enabled(v)?;
    }
    let seam = TEST_XV_SEAM_AFTER_STEPS.load(Ordering::Relaxed);
    let seam_mine = match TEST_XV_SEAM_INITIATOR.load(Ordering::Relaxed) {
        0 => true,
        id => u64::from(routed.volumes[coord].own_appender_id()) + 1 == id,
    };
    let allowed = if seam == 0 || !seam_mine {
        usize::MAX
    } else {
        (seam - 1) as usize
    };

    let tx_id = mint_tx_id(&routed.volumes[coord]);
    let image = IntentRecord {
        tx_id,
        op: plan.op,
        steps: plan.steps.clone(),
    }
    .encode()?;
    // The intent rides the FIRST LOCAL step when step 0 is local (the
    // shipped path verbatim); a plan whose step 0 is foreign writes the
    // intent alone first — the initiator's ring is what the roll-forward
    // recovers, so nothing ships before the intent is durable there.
    let step0_local = matches!(
        step_home(routed, coord, localised[0].1.local_home()),
        StepHome::Local
    );
    let intent_ino = intent_ino_for(routed, coord, step0_local.then_some(&localised[0].1))?;
    let armed = routed.volumes[coord].slot_lease_armed();
    let put = XvRider::Put {
        intent_ino,
        tx_id,
        image: image.clone(),
    };

    let mut outcomes = Vec::with_capacity(localised.len());
    let mut foreign_refusal: Option<(usize, i32)> = None;
    // Registered IN FLIGHT before the record exists: a cadence scan that
    // sees the record always finds its live owner (Issue 4).
    note_intent_in_flight(tx_id);
    if !step0_local {
        if allowed == 0 {
            note_intent_never_durable(tx_id);
            return Err(seam_error());
        }
        // The intent alone: the initiator's half of this plan is nothing
        // (every step is another appender's) or later in the order.
        let out = routed.volumes[coord]
            .xv_write_intent(&put, guards.clone())
            .await;
        if out.is_err() {
            routed.mirror_volume_failure(coord);
            note_intent_never_durable(tx_id);
        }
        out?;
        XV_TX_STARTED.fetch_add(1, Ordering::Relaxed);
        note_intent_durable(tx_id);
        phase_record(XvPhase::Plan, t_total);
        let t = std::time::Instant::now();
        if let Err(e) = barrier(routed, coord).await {
            escalate_midplan(routed, &localised, tx_id, 0, &e);
            note_intent_abandoned(tx_id);
            return Err(e);
        }
        phase_record(XvPhase::IntentBarrier, t);
    }
    for (i, (v_idx, local)) in localised.iter().enumerate() {
        if i >= allowed {
            // The severed plan models a dead process: a durable intent is
            // a predecessor's at the next mount (abandoned here — the
            // same process's reopen adopts it); one never written is
            // nobody's.
            if i == 0 && step0_local {
                note_intent_never_durable(tx_id);
            } else {
                note_intent_abandoned(tx_id);
            }
            return Err(seam_error());
        }
        let rider = (i == 0 && step0_local).then_some(&put);
        let t_step = std::time::Instant::now();
        let (dispatch, r) = apply_or_ship_step_retrying(
            routed,
            tx_id,
            i,
            *v_idx,
            &plan.steps[i],
            local,
            rider,
            guards.clone(),
        )
        .await;
        // The arm the producing attempt TOOK (review round 1, Issue 3) —
        // never a second table read ahead of the dispatch.
        let shipped = dispatch == Dispatch::Shipped;
        let local_step = !(i == 0 && step0_local) && !shipped;
        match r {
            Ok(o) => {
                if i == 0 && step0_local {
                    // Counted only once the intent record is DURABLE (it
                    // rode this commit): `started` therefore means "an
                    // intent exists", which is what makes
                    // `started == completed` the steady-state law.
                    XV_TX_STARTED.fetch_add(1, Ordering::Relaxed);
                    note_intent_durable(tx_id);
                    phase_record(XvPhase::Plan, t_total);
                } else if local_step {
                    phase_record(XvPhase::LocalSteps, t_step);
                }
                // A LIVE witness refusal at a SHIPPED step: the object
                // moved at the holder between the plan and the apply. The
                // plan completes (every later step still runs — a skipped
                // insert never undoes a committed removal), the op answers
                // the step's errno, and a create's minted child — the one
                // half nothing names — is destroyed before the retirement.
                // Judged by the mode the refusing attempt was DISPATCHED
                // in (PR 13e, F-R4): re-resolving the home here read a
                // slot that had moved to THIS initiator as `Local`, the
                // holder's refusal passed as a skipped local step, and
                // the create acked with no name anywhere.
                let refused_here = o.status == XvStepStatus::ForeignSkipped && armed && shipped;
                outcomes.push(o);
                if refused_here {
                    // The plan STOPS here (review round 1, Issue 15): the
                    // applied steps before it are compensated below, so
                    // the op's halves never outlive its errno.
                    foreign_refusal = Some((i, foreign_skipped_errno(&plan.steps[i])));
                    break;
                }
            }
            Err(e) => {
                if i == 0 && step0_local {
                    // Step 0 and the intent are ONE entry: a failure here
                    // committed neither, so there is nothing to complete
                    // and nothing to escalate.
                    note_intent_never_durable(tx_id);
                    return Err(e);
                }
                // Classified by the arm the step was DISPATCHED on
                // (`Dispatch`, the attempt's own), never by re-resolving
                // its home after the fact: a slot that moved
                // to THIS initiator during the ship reads `Local` now, and
                // the shipped refusal was taken for a local device error —
                // the fail-stop (defect 29).
                // A slot-moved refusal (defect 35) is the retryable class
                // whatever the dispatch mode: the door refused BEFORE any
                // effect, the plan's applied steps stand, and the intent's
                // roll-forward ships the step to the holder the door named
                // — never the lattice (a device error is what the lattice
                // guards; a slot that moved is the plane doing its job).
                if armed && (!local_step || is_slot_moved_refusal(&e)) {
                    log::warn!(
                        "cross-owner transaction {tx_id:016x} ({:?}): step {i} of {} could not \
                         be {} ({e}) — the intent stays open and the roll-forward cadence \
                         completes it (design-symmetric-metadata §5.6)",
                        plan.op,
                        localised.len(),
                        if local_step {
                            "applied at this mount's door (the slot moved)"
                        } else {
                            "shipped to its holder"
                        }
                    );
                    note_intent_abandoned(tx_id);
                    return Err(e);
                }
                escalate_midplan(routed, &localised, tx_id, i, &e);
                note_intent_abandoned(tx_id);
                return Err(e);
            }
        }
        if i == 0 && step0_local {
            // Cross-DEVICE ordering: the intent must be durable before any
            // later participant's entry is submitted, or a power cut could
            // keep a later effect and lose the intent that explains it. A
            // barrier that FAILS leaves exactly the durable-but-incomplete
            // shape a mid-plan step failure leaves, so it escalates the
            // same way.
            let t = std::time::Instant::now();
            if let Err(e) = barrier(routed, *v_idx).await {
                escalate_midplan(routed, &localised, tx_id, i, &e);
                note_intent_abandoned(tx_id);
                return Err(e);
            }
            phase_record(XvPhase::IntentBarrier, t);
        }
    }

    // Every LOCAL participant durable BEFORE the retirement: otherwise a
    // power cut could keep the retirement and lose a step. A shipped
    // step's durability is its holder's lane — its reply came after it.
    let mut participants: Vec<usize> = localised
        .iter()
        .skip(1)
        .filter(|(v, l)| matches!(step_home(routed, *v, l.local_home()), StepHome::Local))
        .map(|(v, _)| *v)
        .collect();
    participants.sort_unstable();
    participants.dedup();
    for v in participants {
        if v == coord {
            continue;
        }
        let t = std::time::Instant::now();
        if let Err(e) = barrier(routed, v).await {
            escalate_midplan(routed, &localised, tx_id, localised.len(), &e);
            note_intent_abandoned(tx_id);
            return Err(e);
        }
        phase_record(XvPhase::LocalSteps, t);
    }
    if allowed <= localised.len() {
        note_intent_abandoned(tx_id);
        return Err(seam_error());
    }
    if let Some((at, errno)) = foreign_refusal {
        let retired = compensate_live_refusal(
            routed,
            tx_id,
            plan,
            &localised,
            &outcomes,
            at,
            coord,
            intent_ino,
            guards.clone(),
        )
        .await?;
        let e = SqueezefsError::refused(
            errno,
            format!(
                "cross-owner {:?}: step {at} ({}) found its object moved at the holder \
                 between the plan and the apply — refused by the holder's witness",
                plan.op,
                plan.steps[at].name()
            ),
        );
        if retired {
            XV_TX_COMPLETED.fetch_add(1, Ordering::Relaxed);
            note_intent_retired(tx_id);
        } else {
            retire(routed, coord, intent_ino, tx_id, guards).await?;
        }
        return Err(e);
    }
    retire(routed, coord, intent_ino, tx_id, guards).await?;
    phase_record(XvPhase::Total, t_total);
    Ok(XvExecution { tx_id, outcomes })
}

/// The retirement — SYNCHRONOUS, before the op releases its guards. An
/// intent that outlived its transaction could meet a later, legitimate
/// mutation of the same objects, and recovery's witnesses assume no such
/// interleaving exists (§4.10a).
async fn retire(
    routed: &RoutedMetaBackend,
    coord: usize,
    intent_ino: Ino,
    tx_id: u64,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<()> {
    let t = std::time::Instant::now();
    if let Err(e) = routed.volumes[coord]
        .xv_retire_intent_at(intent_ino, tx_id, guards)
        .await
    {
        routed.mirror_volume_failure(coord);
        log::error!(
            "cross-volume transaction {tx_id:016x}: the retirement failed ({e}) — the intent \
             stays durable and the next roll-forward retires it"
        );
        note_intent_abandoned(tx_id);
        return Err(e);
    }
    phase_record(XvPhase::Retire, t);
    XV_TX_COMPLETED.fetch_add(1, Ordering::Relaxed);
    note_intent_retired(tx_id);
    Ok(())
}

/// The live-refusal compensation (review round 1, Issue 15): the plan
/// stopped at step `at`, a shipped step the holder's witness refused
/// (the object moved at the holder between the plan's read and the
/// apply — a stale foreign read, the S5 projection until PR 5). Every
/// step APPLIED before it is undone in reverse under the op's still-held
/// guards, through the same applier: a count step by its inverse CAS
/// (`SetNlink { post → pre }` — a `link`'s raised count comes back), a
/// removed dentry re-inserted (a `rename`'s source name returns, with the
/// child's type read from its record), an applied insert removed, a
/// minted child destroyed (nothing names it); a `TouchCtime` is
/// monotone and stands. A foreign inverse ships to its holder under the
/// same scope. **The intent's retirement rides the LAST inverse's own
/// entry when that inverse is local to the intent's volume** (review
/// round 2, Issue 26 — the rider pattern), so compensation and
/// retirement are ONE commit and the plan is either open-and-forward or
/// gone; the op then answers the step's errno. Returns whether the
/// retirement rode (the caller retires separately otherwise). **The
/// crash window that remains**: a kill between an applied step and the
/// last inverse leaves the intent open, and the roll-forward applies the
/// FORWARD plan — re-meeting the refusal at the holder and leaving the
/// applied halves (a raised count with no name, a removed source name):
/// the C9/C10 census classes; when the last inverse is FOREIGN the
/// window extends to the separate retirement — the abort marker on the
/// record is the recovery-side answer this rung does not build.
#[allow(clippy::too_many_arguments)]
async fn compensate_live_refusal(
    routed: &RoutedMetaBackend,
    tx_id: u64,
    plan: &XvPlan,
    localised: &[(usize, XvLocalStep)],
    outcomes: &[XvStepOutcome],
    at: usize,
    coord: usize,
    intent_ino: Ino,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<bool> {
    let applied: Vec<usize> = (0..at)
        .rev()
        .filter(|i| {
            outcomes.get(*i).map(|o| o.status) == Some(XvStepStatus::Applied)
                && !matches!(plan.steps[*i], XvStep::TouchCtime { .. })
        })
        .collect();
    let last = applied.last().copied();
    let delete = XvRider::Delete { intent_ino, tx_id };
    let mut retired = false;
    for i in applied {
        // The retirement rides the last inverse when it commits on the
        // intent's own volume as a LOCAL apply.
        let (v_idx, local) = &localised[i];
        let rides_here = last == Some(i)
            && *v_idx == coord
            && matches!(
                step_home(routed, *v_idx, local.local_home()),
                StepHome::Local
            );
        let rider = rides_here.then_some(&delete);
        let inverse = match &plan.steps[i] {
            XvStep::SetNlink { ino, pre, post, .. } => Some(XvStep::SetNlink {
                ino: *ino,
                pre: *post,
                post: *pre,
                ctime: None,
            }),
            XvStep::RemoveDentry {
                parent,
                name,
                expect_child,
                parent_update,
            } => {
                // The restored name's type is the child's HOLDER's word
                // (PR 13e, F-R3): a foreign-minted child read off the
                // projection would answer no record and no inverse.
                let (cv, cl) = routed.route_ino(*expect_child);
                routed.volumes[cv]
                    .read_inode_witness(cl)
                    .await?
                    .map(|child| XvStep::InsertDentry {
                        parent: *parent,
                        name: name.clone(),
                        child: *expect_child,
                        ft_bits: child.mode & libc::S_IFMT,
                        parent_update: *parent_update,
                    })
            }
            XvStep::InsertDentry {
                parent,
                name,
                child,
                parent_update,
                ..
            } => Some(XvStep::RemoveDentry {
                parent: *parent,
                name: name.clone(),
                expect_child: *child,
                parent_update: *parent_update,
            }),
            XvStep::TouchCtime { .. } => None,
            XvStep::MintInode { .. } | XvStep::CreateInode { .. } => {
                let out = routed.volumes[*v_idx]
                    .xv_destroy_unnamed(local.local_home(), rider, guards.clone())
                    .await;
                if out.is_err() {
                    routed.mirror_volume_failure(*v_idx);
                }
                out?;
                retired |= rides_here;
                None
            }
        };
        let Some(inverse) = inverse else {
            continue;
        };
        let (iv, il) = localise(routed, &inverse);
        let (_dispatch, out) =
            apply_or_ship_step(routed, tx_id, i, iv, &inverse, &il, rider, guards.clone()).await;
        let out = out?;
        retired |= rides_here;
        if out.status == XvStepStatus::ForeignSkipped {
            log::error!(
                "cross-owner transaction {tx_id:016x} ({:?}): compensating step {i} ({}) found \
                 its object moved again — the half stays for the census (C9/C10)",
                plan.op,
                inverse.name()
            );
        }
    }
    Ok(retired)
}

/// A coalesced durability barrier on one volume.
async fn barrier(routed: &RoutedMetaBackend, v_idx: usize) -> Result<()> {
    // Strict volumes barrier inside every commit (§4.6 pt 4) — the
    // protocol's ordering is already paid.
    if routed.volumes[v_idx].xv_strict_barriers() {
        return Ok(());
    }
    let out = routed.volumes[v_idx].sync_device().await;
    if out.is_err() {
        routed.mirror_volume_failure(v_idx);
    }
    out
}

/// A live mid-plan failure: the transaction is durable-but-incomplete.
/// Latch every volume it touches into the fail-stop lattice so nothing can
/// mutate the plan's objects before the next mount completes it — the
/// invariant recovery's witnesses rest on — and say so loud.
fn escalate_midplan(
    routed: &RoutedMetaBackend,
    localised: &[(usize, XvLocalStep)],
    tx_id: u64,
    at_step: usize,
    err: &SqueezefsError,
) {
    XV_MIDPLAN_ESCALATIONS.fetch_add(1, Ordering::Relaxed);
    let mut vols: Vec<usize> = localised.iter().map(|(v, _)| *v).collect();
    vols.sort_unstable();
    vols.dedup();
    for v in &vols {
        routed.disabled_volumes.insert(*v, true);
    }
    log::error!(
        "cross-volume transaction {tx_id:016x} failed at step {at_step} of {} ({err}) — its \
         intent record is durable and the next mount will roll it forward; volume(s) {vols:?} \
         are fail-stopped until then so nothing can mutate the transaction's objects \
         (crossvol_tx_midplan_escalations)",
        localised.len()
    );
}

// ---------------------------------------------------------------------------
// Recovery (mount, before the mount serves) and the roll-forward cadence
// ---------------------------------------------------------------------------

/// One open intent as a scan found it: the volume that hosts it, the key
/// ino it is homed on, the decoded record.
struct OpenIntent {
    host: usize,
    intent_ino: Ino,
    rec: IntentRecord,
}

/// Scan every volume for open intents (every intent home: ino 0, and on
/// a forest each slot namespace's local 0), decoded and id-checked —
/// SCOPED to the homes whose slot this mount's step-home is `Local` for
/// (review round 2, Issue 21): an intent in a slot another appender
/// leases is that appender's to complete — its tree here is a projection
/// (the re-read under the guards would not be RAM-authoritative, a
/// retirement by the peer's successor invisible), so two mounts never
/// both adopt one intent. The cross-process face — a dead initiator's
/// intents adopted by the mount that recovers its ring, an intent
/// inherited with a re-leased slot scanned at the lease install — is PR
/// 12's obligation (design row 6, the note §8a); on this tree one process
/// holds every slot tree, so every intent is its own.
async fn scan_open(routed: &RoutedMetaBackend) -> Result<Vec<OpenIntent>> {
    let mut open: Vec<OpenIntent> = Vec::new();
    for (idx, vol) in routed.volumes.iter().enumerate() {
        for (intent_ino, tx_id, image) in vol.xv_scan_intents_homed().await? {
            if !matches!(step_home(routed, idx, intent_ino), StepHome::Local) {
                continue;
            }
            // A JOINED appender adopts only the intents homed in slots it
            // LEASES — its own rings' residue (PR 12b review round 1,
            // Issue 7): `step_home` answers `Local` for every UNLEASED
            // slot on every posture, so a joiner's open adopted every
            // intent of the set homed on an unleased slot — a dead peer's,
            // the manager's own in-flight one — beside the manager's
            // cadence, which is the lessee of record for an unleased slot
            // (KD-SYM-2/3) and completes them alone.
            if vol.is_joined_appender() {
                let slot = super::kv::record::forest_slot_of_ino(intent_ino);
                let leased = vol.slot_leases().is_some_and(|p| p.gate.is_leased(slot));
                if !leased {
                    continue;
                }
            }
            let rec = IntentRecord::decode(&image).map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "metadata volume {}: open cross-volume intent {tx_id:016x} does not \
                     decode ({e}) — refusing to mount a set with an unreadable half-applied \
                     transaction; run `squeezefs fsck` and see docs/operations.md \
                     (cross-volume transactions)",
                    vol.device_path().display()
                ))
            })?;
            if rec.tx_id != tx_id {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "metadata volume {}: cross-volume intent keyed {tx_id:016x} carries id \
                     {:016x} — refusing to mount",
                    vol.device_path().display(),
                    rec.tx_id
                )));
            }
            open.push(OpenIntent {
                host: idx,
                intent_ino,
                rec,
            });
        }
    }
    // Deterministic order (ids are monotone per mount, so this is also
    // submission order for anything a single crash left behind).
    open.sort_by_key(|o| o.rec.tx_id);
    Ok(open)
}

/// Roll every open cross-volume intent forward. Called from
/// `open_routed_meta_set` — i.e. on WRITE opens only (a read-only mount
/// never mints and never recovers, DLM S5), after each volume's journal
/// replay has made its records RAM-authoritative, and before the mount
/// serves anything.
///
/// Returns the number of transactions completed. The scan is bounded (one
/// 16-byte-prefix range per volume) and finds nothing on a healthy set, so
/// this is not a mount-time cost anybody pays twice.
///
/// Under the armed symmetric plane an intent whose foreign step cannot be
/// shipped yet (the shipper and the holders' endpoints are the join
/// ladder's, installed after this open) stays open — never a mount
/// refusal — for [`roll_forward_open_intents`].
pub async fn recover_open_intents(routed: &RoutedMetaBackend) -> Result<usize> {
    let open = scan_open(routed).await?;
    if open.is_empty() {
        return Ok(0);
    }
    let n = open.len();
    log::warn!(
        "cross-volume transaction recovery: {n} open intent(s) found at mount — rolling \
         forward (a crash interrupted them; this is the DUR-7 machinery working)"
    );
    let mut rolled = 0usize;
    for o in open {
        if recover_one(routed, &o).await? {
            rolled += 1;
        }
    }
    Ok(rolled)
}

/// **The roll-forward cadence's body** (symmetric PR 6): re-scan the open
/// intents and complete every one this process does NOT have in flight —
/// an intent an op of this process abandoned (its holder was down), or
/// one found with no live owner here (a predecessor incarnation's) — the
/// S3.5 recovery over the shipped applier, run after the holders'
/// endpoints are bound. Adoption is by REGISTER STATE, never by elapsed
/// time (review round 1, Issue 4): a live op's intent is its own however
/// long it runs, and `recover_one` re-reads every intent under the
/// guards it acquires, so a scan an intervening retirement outdated
/// applies nothing. Returns how many retired; an intent whose holder is
/// still unreachable stays open and, past the grace window, counts on
/// `xv_cross_owner_intents_stuck`.
pub async fn roll_forward_open_intents(routed: &RoutedMetaBackend) -> Result<usize> {
    let open = scan_open(routed).await?;
    // The register is a PROJECTION of the durable records this mount's
    // scan owns: an abandoned entry the scan no longer lists was retired
    // elsewhere (or moved with its slot) and is forgotten here, never a
    // ghost on `xv_cross_owner_intents_{open,stuck}`.
    let durable: std::collections::HashSet<u64> = open.iter().map(|o| o.rec.tx_id).collect();
    let forgotten = forget_abandoned_absent(&durable);
    if forgotten > 0 {
        log::debug!(
            "cross-volume transaction roll-forward: {forgotten} abandoned intent(s) no volume \
             of this mount's scan holds any more — retired by another appender or moved with \
             their slot; forgotten here"
        );
    }
    let hold = TEST_XV_CADENCE_HOLD_AFTER_SCAN_MS.load(Ordering::Relaxed);
    if hold > 0 {
        squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold)).await;
    }
    let mut rolled = 0usize;
    for o in open {
        if intent_in_flight(o.rec.tx_id) {
            continue;
        }
        if recover_one(routed, &o).await? {
            rolled += 1;
        }
    }
    Ok(rolled)
}

/// Roll ONE intent forward. `Ok(true)` = retired; `Ok(false)` = a shipped
/// step could not reach its holder and the intent stays open (armed
/// plane), or the record was gone under the guards (retired meanwhile —
/// a no-op); a LOCAL failure propagates as the mount refusal it always
/// was. The plan applied is the record as RE-READ under the acquired
/// guards (Issue 4): the guards the scanned plan names are the record's
/// (a retirement never changes a plan's keys), and a record that is gone
/// applies nothing and leaves no register ghost.
async fn recover_one(routed: &RoutedMetaBackend, o: &OpenIntent) -> Result<bool> {
    let scanned = &o.rec;
    let armed = routed.volumes[o.host].slot_lease_armed();
    let localised: Vec<(usize, XvLocalStep)> =
        scanned.steps.iter().map(|s| localise(routed, s)).collect();
    // A holder this mount cannot reach for the plan's guards (its endpoint
    // unbound — at the mount's own recovery every declared region is
    // still to be opened) is the same class as a step it cannot ship: the
    // intent stays open for the cadence, never a failed mount.
    let guards: Arc<[dlm::DlmGuard]> = match acquire_plan_guards(routed, &localised).await {
        Ok(g) => Arc::from(g),
        Err(e) if armed => {
            log::warn!(
                "cross-owner transaction {:016x} ({:?}): its guards could not be acquired at a \
                 holder ({e}) — left open for the next roll-forward",
                scanned.tx_id,
                scanned.op
            );
            note_intent_adopted(scanned.tx_id);
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    // The record under the guards: gone ⇒ retired meanwhile, nothing to
    // apply, no ghost; present ⇒ THIS image is the plan.
    let Some(image) = routed.volumes[o.host]
        .xv_read_intent(o.intent_ino, scanned.tx_id)
        .await?
    else {
        forget_intent(scanned.tx_id);
        return Ok(false);
    };
    let current = IntentRecord::decode(&image).map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "cross-volume intent {:016x} does not decode under its guards ({e})",
            scanned.tx_id
        ))
    })?;
    if current.steps.len() != localised.len() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "cross-volume intent {:016x} changed shape under its guards ({} → {} steps)",
            scanned.tx_id,
            localised.len(),
            current.steps.len()
        )));
    }
    let rec = &current;
    let localised: Vec<(usize, XvLocalStep)> =
        rec.steps.iter().map(|s| localise(routed, s)).collect();
    note_intent_adopted(rec.tx_id);

    for (i, (v_idx, local)) in localised.iter().enumerate() {
        // Classified by the arm the attempt TOOK (defect 29's law, made the
        // dispatch's own word — review round 1, Issue 3): a slot that moved
        // to this mount during the ship reads `Local` after it, and the
        // shipped refusal must never be taken for a local device error.
        let (dispatch, r) = apply_or_ship_step_retrying(
            routed,
            rec.tx_id,
            i,
            *v_idx,
            &rec.steps[i],
            local,
            None,
            Arc::clone(&guards),
        )
        .await;
        let shipped = dispatch == Dispatch::Shipped;
        let out = match r {
            Ok(out) => out,
            // A shipped step's unreachable holder, OR a LOCAL step whose
            // slot moved away past the retry bound (`execute`'s own arm —
            // PR 13 review round 1, Issue 13: before it the roll-forward
            // propagated the door's `SlotBusy` as the mount refusal / the
            // cadence's error a LOCAL device failure earns, for a slot
            // the plane moved on purpose). Left open: the next pass
            // re-resolves it, at whichever holder tree 0 names then.
            Err(e) if armed && (shipped || is_slot_moved_refusal(&e)) => {
                log::warn!(
                    "cross-owner transaction {:016x} ({:?}): step {i} could not be {} ({e}) — \
                     left open for the next roll-forward",
                    rec.tx_id,
                    rec.op,
                    if shipped {
                        "shipped to its holder"
                    } else {
                        "applied at this mount's door (its slot moved)"
                    }
                );
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        if out.status == XvStepStatus::ForeignSkipped {
            log::error!(
                "cross-volume transaction {:016x}: a step's object moved under the intent — \
                 skipped rather than clobbered (crossvol_tx_steps_foreign; run \
                 `squeezefs fsck`)",
                rec.tx_id
            );
        }
    }
    // The completed LOCAL steps must be durable before the intent that
    // explains them is retired (the live path's rule, verbatim).
    let mut vols: Vec<usize> = localised
        .iter()
        .filter(|(v, l)| matches!(step_home(routed, *v, l.local_home()), StepHome::Local))
        .map(|(v, _)| *v)
        .collect();
    vols.push(o.host);
    vols.sort_unstable();
    vols.dedup();
    for v in vols {
        barrier(routed, v).await?;
    }
    routed.volumes[o.host]
        .xv_retire_intent_at(o.intent_ino, rec.tx_id, guards)
        .await?;
    XV_TX_RECOVERED.fetch_add(1, Ordering::Relaxed);
    note_intent_retired(rec.tx_id);
    log::info!(
        "cross-volume transaction {:016x} ({:?}, {} step(s)) rolled forward and retired",
        rec.tx_id,
        rec.op,
        rec.steps.len()
    );
    Ok(true)
}

/// Acquire a recovered plan's 4a guard set through the initiator's own
/// acquisition ([`acquire_guards_leased`] — this table's keys in the ONE
/// canonical order, a foreign holder's in one travelling `XvGuards`
/// each, tables ascending): ascending volume index, and inside each
/// volume `I` before `D`, stripe-deduped by `lock_many`. The recoverer
/// holds nothing while it waits at a holder, so a live initiator at that
/// holder can never form a cycle with it.
async fn acquire_plan_guards(
    routed: &RoutedMetaBackend,
    localised: &[(usize, XvLocalStep)],
) -> Result<Vec<dlm::DlmGuard>> {
    let mut per_vol: Vec<(Vec<(Ino, dlm::LockMode)>, Vec<(Ino, String)>)> =
        (0..routed.volumes.len())
            .map(|_| (Vec::new(), Vec::new()))
            .collect();
    for (v_idx, step) in localised {
        let (inos, dentries) = &mut per_vol[*v_idx];
        let (i, d) = step_guard_keys(step);
        inos.extend(i);
        dentries.extend(d);
    }
    let mut scope = None;
    let mut guards = Vec::new();
    for (v_idx, (mut inos, dentries)) in per_vol.into_iter().enumerate() {
        if inos.is_empty() && dentries.is_empty() {
            continue;
        }
        inos.sort_unstable_by_key(|(l, _)| *l);
        inos.dedup_by_key(|(l, _)| *l);
        let d: Vec<(Ino, &str, dlm::LockMode)> = dentries
            .iter()
            .map(|(p, n)| (*p, n.as_str(), dlm::LockMode::Exclusive))
            .collect();
        guards.extend(acquire_guards_leased(routed, v_idx, &mut scope, &inos, &d, false).await?);
    }
    Ok(guards)
}

/// The step guards a HOLDER takes around a served step (the same keys the
/// initiator would have taken for a local step): `(I keys, D keys)` on
/// the step's volume.
pub fn step_guard_keys(local: &XvLocalStep) -> (Vec<(Ino, dlm::LockMode)>, Vec<(Ino, String)>) {
    match local {
        XvLocalStep::RemoveDentry {
            local_parent, name, ..
        }
        | XvLocalStep::InsertDentry {
            local_parent, name, ..
        } => (
            vec![(*local_parent, dlm::LockMode::Exclusive)],
            vec![(*local_parent, name.clone())],
        ),
        XvLocalStep::SetNlink { local_ino, .. }
        | XvLocalStep::TouchCtime { local_ino, .. }
        | XvLocalStep::MintInode { local_ino, .. }
        | XvLocalStep::CreateInode { local_ino, .. } => {
            (vec![(*local_ino, dlm::LockMode::Exclusive)], Vec::new())
        }
    }
}

/// The wire form of a step outcome's status (`MetaReply::XvStep`).
pub fn status_code(status: XvStepStatus) -> u8 {
    match status {
        XvStepStatus::Applied => 0,
        XvStepStatus::AlreadyApplied => 1,
        XvStepStatus::ForeignSkipped => 2,
    }
}

/// The roll-forward cadence (symmetric PR 6): an armed set's own single-
/// flight task ticking at the checkpoint LANDING ceiling — an intent an op
/// of this process left open (its holder down, its endpoint unbound at
/// the mount's own recovery) is completed within a few ceilings of the
/// holder returning, never only at the next mount. The pass runs only
/// while the register holds an ABANDONED intent (a quiet mount scans
/// nothing) and adopts by register state alone — a live op's intent is
/// never raced by its own cadence (review round 1, Issue 4: the earlier
/// two-tick rule was a timer). Each tick also sweeps the parked guard
/// scopes past the grace window. Ends when the set is dropped.
pub fn spawn_roll_forward_cadence(routed: std::sync::Weak<RoutedMetaBackend>, tick_ms: u64) {
    crate::meta_exec::spawn_meta("xv_roll_forward_cadence", async move {
        let tick = std::time::Duration::from_millis(tick_ms.max(1));
        loop {
            squeezefs_ipc::sqz_time::sleep(tick).await;
            let Some(routed) = routed.upgrade() else {
                return;
            };
            sweep_expired_guards();
            if !any_abandoned() {
                continue;
            }
            if let Err(e) = roll_forward_open_intents(&routed).await {
                log::warn!("cross-owner roll-forward cadence: {e}");
            }
        }
    });
}
