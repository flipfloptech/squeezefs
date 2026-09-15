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
//! inside [`apply_or_ship_step`] and NOTHING ELSE changes: the record
//! format is the same (steps carry global inos, so the participant is
//! resolved by routing at apply time — a slot handover between crash and
//! recovery is handled for free), and the served side is the SAME
//! `xv_apply_step` under the holder's own 4a guards, so idempotence stays
//! a property of one code path. The six-step ladder as built:
//!
//! ```text
//! 1. plan            under the op's LOCAL 4a guards (a foreign slot's key
//!                    takes none here — its guard is the holder's, taken
//!                    around the served apply)
//! 2. tx0             ONE entry in the INITIATOR's ring: the first LOCAL
//!                    step's records + the intent (homed in that step's
//!                    slot — or the mount's rotor slot when every step is
//!                    foreign), so the entry lands in ONE region
//! 3. barrier         the initiator's ring
//! 4. steps           in plan order: own slot ⇒ the local applier; foreign
//!                    ⇒ shipped to the holder (tree 0 + the endpoint table;
//!                    the reply follows the holder's durability lane)
//! 5. barrier         every LOCAL participant volume
//! 6. retire          Delete the intent (initiator's ring)
//! ```
//!
//! **No foreign 4a guard is HELD across the plan** (an as-built deviation
//! from §5.6's "foreign-home guards travel"): a guard taken at the holder
//! beside the initiator's stripe-canonical `lock_many` has no common order
//! with a peer performing the inverse operation (`A: local X → remote Y`,
//! `B: local Y → remote X` is a cycle), and Lustre's FID-ordered
//! acquisition would need every 4a site to order by ino, which the
//! shipped `lock_many` does not. The holder's apply under ITS guards plus
//! the `(pre, post)` witness give the same isolation at commit; a witness
//! refusal on a LIVE shipped step is the op's own errno, with the
//! initiator's half compensated (a minted child destroyed — the only half
//! that becomes unreachable), never a fail-stop.
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

/// Test seam (the served side): the NEXT shipped step commits and its
/// reply is MISDELIVERED (a wrong correlation id — the client refuses it
/// exactly as it fails a dead session), then the seam clears. Models a
/// holder dying after its commit and before its reply.
pub static TEST_XV_SERVE_MISDELIVER_ONCE: AtomicBool = AtomicBool::new(false);

/// Test seam (the served side): every shipped step REFUSES before it
/// commits while armed — a holder that is down, from the initiator's
/// side (the intent stays open for the roll-forward cadence).
pub static TEST_XV_SERVE_REFUSE: AtomicBool = AtomicBool::new(false);

/// Test seam: the grace window in ms after which an open intent no holder
/// serves counts as STUCK (`0` = the derived window,
/// [`stuck_grace_ms`]).
pub static TEST_XV_STUCK_AFTER_MS: AtomicU64 = AtomicU64::new(0);

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
/// Set-wide directory-rename lock takes (initiator side).
static DIR_RENAME_LOCK_ACQUIRES: AtomicU64 = AtomicU64::new(0);

/// Phases of one cross-owner transaction (`xv_cross_owner_phase_ns`,
/// exact-sum: `plan + intent_barrier + ship_rtt + retire ≈ total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum XvPhase {
    /// Guards → tx0 committed (the plan + the first local step).
    Plan = 0,
    /// The initiator ring's barrier after tx0.
    IntentBarrier = 1,
    /// Σ of the shipped steps' round trips.
    ShipRtt = 2,
    /// The retirement commit.
    Retire = 3,
    /// The whole transaction.
    Total = 4,
}

const XV_PHASES: usize = 5;
const XV_PHASE_NAMES: [&str; XV_PHASES] = ["plan", "intent_barrier", "ship_rtt", "retire", "total"];

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

/// Count one served shipped step (the holder side).
pub(crate) fn note_step_served() {
    XV_CO_STEPS_SERVED.fetch_add(1, Ordering::Relaxed);
}

/// The open-intent register: `tx_id` → the instant this process first
/// knew it open. Its population IS `xv_cross_owner_intents_open`; an
/// entry older than the grace window with no holder serving its step is
/// the STUCK class.
static OPEN_INTENTS: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<u64, std::time::Instant>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Note an intent as open (minted here, or found at a scan). Idempotent:
/// a known intent is not re-counted.
fn note_intent_open(tx_id: u64) {
    let mut open = OPEN_INTENTS.lock();
    if let std::collections::hash_map::Entry::Vacant(v) = open.entry(tx_id) {
        v.insert(std::time::Instant::now());
        XV_CO_INTENTS_MINTED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Note an intent as retired.
fn note_intent_retired(tx_id: u64) {
    if OPEN_INTENTS.lock().remove(&tx_id).is_some() {
        XV_CO_INTENTS_RETIRED.fetch_add(1, Ordering::Relaxed);
    }
}

/// The grace window after which an open cross-owner intent nobody serves
/// is STUCK: the membership lease TTL (`CLIENT_STALE_TTL_SECS`) — a slot's
/// dead holder is evicted and its slot recovered inside it (design §5.9),
/// so an intent still open past it names a slot no live holder serves;
/// the seam shortens it for the contracts.
pub fn stuck_grace_ms() -> u64 {
    match TEST_XV_STUCK_AFTER_MS.load(Ordering::Relaxed) {
        0 => crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000,
        ms => ms,
    }
}

/// `(open, stuck)`: the register's population and how many of them are
/// older than the grace window — one lock scope.
fn open_and_stuck_now() -> (u64, u64) {
    let grace = std::time::Duration::from_millis(stuck_grace_ms());
    let open = OPEN_INTENTS.lock();
    let stuck = open
        .values()
        .filter(|since| since.elapsed() > grace)
        .count();
    (open.len() as u64, stuck as u64)
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
    /// GAUGE, **must stay 0**: open intents past the grace window.
    pub intents_stuck: u64,
    pub dir_rename_lock_acquires: u64,
    pub dir_rename_lock_wait_ns_sum: u64,
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
        intents_stuck,
        dir_rename_lock_acquires: DIR_RENAME_LOCK_ACQUIRES.load(Ordering::Relaxed),
        dir_rename_lock_wait_ns_sum: DIR_RENAME_LOCK_WAIT.sum_ns(),
    }
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
        "xv_cross_owner_intents_stuck".into(),
        s.intents_stuck.into(),
    );
    out.insert(
        "xv_cross_owner_phase_ns".into(),
        serde_json::Value::Object(phases),
    );
    out.insert(
        "dir_rename_lock_acquires".into(),
        s.dir_rename_lock_acquires.into(),
    );
    out.insert(
        "dir_rename_lock_wait_ns".into(),
        DIR_RENAME_LOCK_WAIT.to_json(),
    );
    out
}

// ---------------------------------------------------------------------------
// The step shipper (the initiator's client half — PR 12's join ladder
// installs it from the census; the contracts install it directly).
// ---------------------------------------------------------------------------

static XV_SHIPPER: once_cell::sync::Lazy<
    arc_swap::ArcSwapOption<crate::meta_ship::MetaShipRouter>,
> = once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::const_empty);

/// Install the process-global step shipper: the S8 client router a
/// foreign step travels on (its lanes, its same-id resend, its era
/// learning).
pub fn install_xv_shipper(router: Arc<crate::meta_ship::MetaShipRouter>) {
    XV_SHIPPER.store(Some(router));
}

/// Remove it (a stale install is inert: a foreign step with no shipper
/// is the un-shippable class the cadence retries).
pub fn uninstall_xv_shipper() {
    XV_SHIPPER.store(None);
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
/// Encoding-budget cap on a plan's step count (the largest plan the
/// converted ops build is 9 — a cross-volume directory rename replacing a
/// destination with `RENAME_WHITEOUT`; the cap is the decoder's bound, not
/// a tuning knob).
pub const XV_MAX_STEPS: usize = 32;

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

/// The public face of [`localise`] — the served side of a shipped step
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
fn foreign_holder_of(
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
    let (holder, endpoint) = match step_home(routed, v_idx, local_parent) {
        StepHome::Local => return routed.lookup_dentry(parent, name).await,
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

/// Ship one step to its holder through the installed shipper.
async fn ship_step(
    endpoint: &str,
    holder: u32,
    tx_id: u64,
    step_idx: usize,
    step: &XvStep,
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
async fn apply_or_ship_step(
    routed: &RoutedMetaBackend,
    tx_id: u64,
    step_idx: usize,
    v_idx: usize,
    step: &XvStep,
    local: &XvLocalStep,
    rider: Option<&XvRider>,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<XvStepOutcome> {
    match step_home(routed, v_idx, local.local_home()) {
        StepHome::Local => {
            let out = routed.volumes[v_idx]
                .xv_apply_step(local, rider, guards)
                .await;
            if out.is_err() {
                routed.mirror_volume_failure(v_idx);
            }
            let out = out?;
            out.count();
            Ok(out)
        }
        // The HOLDER counts the outcome on the S3.5 step ledger (the
        // effect committed there); the initiator counts the ship.
        StepHome::Foreign { holder, endpoint } => {
            ship_step(&endpoint, holder, tx_id, step_idx, step).await
        }
        StepHome::Unreachable { holder } => Err(unreachable_error(holder, step.name())),
    }
}

/// Is `e` a SHIPPED step's failure (the holder unreachable, its session
/// dead, its refusal) rather than a local device error? The armed plane
/// leaves the intent open on the first class and fail-stops on the
/// second — the S3.5 lattice latch protects the witnesses' premise
/// against a local mid-plan device error, which a holder that is down
/// does not violate.
fn is_ship_failure(routed: &RoutedMetaBackend, v_idx: usize, local: &XvLocalStep) -> bool {
    !matches!(
        step_home(routed, v_idx, local.local_home()),
        StepHome::Local
    )
}

/// The intent's key ino for a plan whose first LOCAL step is `first_local`
/// (its slot's local 0), or — every step foreign — the mount's own rotor
/// slot on the coordinator volume. Flat and unarmed: [`XV_INTENT_INO`].
fn intent_ino_for(
    routed: &RoutedMetaBackend,
    coord: usize,
    first_local: Option<&XvLocalStep>,
) -> Result<Ino> {
    let vol = &routed.volumes[coord];
    let Some(plane) = vol.slot_leases() else {
        return Ok(XV_INTENT_INO);
    };
    let slot = match first_local {
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
    let allowed = if seam == 0 {
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
    if !step0_local {
        if allowed == 0 {
            return Err(seam_error());
        }
        // The intent alone: the initiator's half of this plan is nothing
        // (every step is another appender's) or later in the order.
        let out = routed.volumes[coord]
            .xv_write_intent(&put, guards.clone())
            .await;
        if out.is_err() {
            routed.mirror_volume_failure(coord);
        }
        out?;
        XV_TX_STARTED.fetch_add(1, Ordering::Relaxed);
        note_intent_open(tx_id);
        phase_record(XvPhase::Plan, t_total);
        let t = std::time::Instant::now();
        if let Err(e) = barrier(routed, coord).await {
            escalate_midplan(routed, &localised, tx_id, 0, &e);
            return Err(e);
        }
        phase_record(XvPhase::IntentBarrier, t);
    }
    for (i, (v_idx, local)) in localised.iter().enumerate() {
        if i >= allowed {
            return Err(seam_error());
        }
        let rider = (i == 0 && step0_local).then_some(&put);
        match apply_or_ship_step(
            routed,
            tx_id,
            i,
            *v_idx,
            &plan.steps[i],
            local,
            rider,
            guards.clone(),
        )
        .await
        {
            Ok(o) => {
                if i == 0 && step0_local {
                    // Counted only once the intent record is DURABLE (it
                    // rode this commit): `started` therefore means "an
                    // intent exists", which is what makes
                    // `started == completed` the steady-state law.
                    XV_TX_STARTED.fetch_add(1, Ordering::Relaxed);
                    note_intent_open(tx_id);
                    phase_record(XvPhase::Plan, t_total);
                }
                // A LIVE witness refusal at a shipped step: the object
                // moved at the holder between the plan and the apply. The
                // plan completes (every later step still runs — a skipped
                // insert never undoes a committed removal), the op answers
                // the step's errno, and a create's minted child — the one
                // half nothing names — is destroyed before the retirement.
                if o.status == XvStepStatus::ForeignSkipped
                    && armed
                    && foreign_refusal.is_none()
                    && !matches!(
                        step_home(routed, *v_idx, local.local_home()),
                        StepHome::Local
                    )
                {
                    foreign_refusal = Some((i, foreign_skipped_errno(&plan.steps[i])));
                }
                outcomes.push(o)
            }
            Err(e) => {
                if i == 0 && step0_local {
                    // Step 0 and the intent are ONE entry: a failure here
                    // committed neither, so there is nothing to complete
                    // and nothing to escalate.
                    return Err(e);
                }
                if armed && is_ship_failure(routed, *v_idx, local) {
                    log::warn!(
                        "cross-owner transaction {tx_id:016x} ({:?}): step {i} of {} could not \
                         be shipped to its holder ({e}) — the intent stays open and the \
                         roll-forward cadence completes it (design-symmetric-metadata §5.6)",
                        plan.op,
                        localised.len()
                    );
                    return Err(e);
                }
                escalate_midplan(routed, &localised, tx_id, i, &e);
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
        if let Err(e) = barrier(routed, v).await {
            escalate_midplan(routed, &localised, tx_id, localised.len(), &e);
            return Err(e);
        }
    }
    if allowed <= localised.len() {
        return Err(seam_error());
    }
    if let Some((at, errno)) = foreign_refusal {
        compensate_live_refusal(routed, plan, &localised, guards.clone()).await?;
        let e = SqueezefsError::refused(
            errno,
            format!(
                "cross-owner {:?}: step {at} ({}) found its object moved at the holder \
                 between the plan and the apply — refused by the holder's witness",
                plan.op,
                plan.steps[at].name()
            ),
        );
        retire(routed, coord, intent_ino, tx_id, guards).await?;
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
        return Err(e);
    }
    phase_record(XvPhase::Retire, t);
    XV_TX_COMPLETED.fetch_add(1, Ordering::Relaxed);
    note_intent_retired(tx_id);
    Ok(())
}

/// The live-refusal compensation: a `Create` whose insert the holder
/// refused leaves a minted child nothing names — destroyed here, in the
/// creator's own ring, before the intent retires (a crash before this
/// point leaves it to the C9 census, the class the intent's roll-forward
/// re-meets as the same refusal). Every other op's halves stay: a count
/// that moved and a name that was already gone are each consistent on
/// their own, and undoing an acked-visible removal would be the lie
/// roll-forward exists to avoid.
async fn compensate_live_refusal(
    routed: &RoutedMetaBackend,
    plan: &XvPlan,
    localised: &[(usize, XvLocalStep)],
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<()> {
    if plan.op != XvOp::Create {
        return Ok(());
    }
    for (v_idx, local) in localised {
        if let XvLocalStep::CreateInode { local_ino, .. } = local {
            let out = routed.volumes[*v_idx]
                .xv_destroy_unnamed(*local_ino, guards.clone())
                .await;
            if out.is_err() {
                routed.mirror_volume_failure(*v_idx);
            }
            out?;
        }
    }
    Ok(())
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
/// a forest each slot namespace's local 0), decoded and id-checked.
async fn scan_open(routed: &RoutedMetaBackend) -> Result<Vec<OpenIntent>> {
    let mut open: Vec<OpenIntent> = Vec::new();
    for (idx, vol) in routed.volumes.iter().enumerate() {
        for (intent_ino, tx_id, image) in vol.xv_scan_intents_homed().await? {
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
/// intents and complete every one whose steps can be applied or shipped
/// now — the S3.5 recovery over the shipped applier, run after the
/// holders' endpoints are bound (an initiator's own crash residue at the
/// next mount, a holder that was down while the op ran). Returns how many
/// retired; an intent whose holder is still unreachable stays open and,
/// past the grace window, counts on `xv_cross_owner_intents_stuck`.
pub async fn roll_forward_open_intents(routed: &RoutedMetaBackend) -> Result<usize> {
    let open = scan_open(routed).await?;
    let mut rolled = 0usize;
    for o in open {
        if recover_one(routed, &o).await? {
            rolled += 1;
        }
    }
    Ok(rolled)
}

/// **Ownership-scoped recovery** (per-volume claim admission §5.4 sweep
/// row 3): [`recover_open_intents`] for a mount that appends to only PART
/// of the set. `owned[v]` is `true` for a volume this mount holds the D0
/// claim on.
///
/// The three arms are §5.4a's, unchanged in substance:
///
/// * an intent wholly inside volumes THIS node owns → rolled forward;
/// * wholly inside a peer's → **skipped and logged once**, because
///   rolling it forward is a WRITE to trees this mount has no authority
///   over; its owner's own mount recovers it;
/// * spanning two owners → **refuse the mount loud**, and count it.
///   `xv_cross_owner_intents` is a must-stay-0 tripwire: the M1 pre-check
///   is what keeps it 0, so a nonzero value means a cross-owner mutation
///   escaped that check and half-committed (case (c) is reachable-by-bug,
///   not unreachable).
pub async fn recover_open_intents_scoped(
    routed: &RoutedMetaBackend,
    owned: &[bool],
) -> Result<usize> {
    let open = scan_open(routed).await?;
    if open.is_empty() {
        return Ok(0);
    }
    let mine = |v: usize| owned.get(v).copied().unwrap_or(false);
    let mut rolled = 0usize;
    for o in open {
        let mut vols: Vec<usize> = o.rec.steps.iter().map(|s| localise(routed, s).0).collect();
        vols.push(o.host);
        vols.sort_unstable();
        vols.dedup();
        let ours = vols.iter().filter(|v| mine(**v)).count();
        if ours == vols.len() {
            if recover_one(routed, &o).await? {
                rolled += 1;
            }
        } else if ours == 0 {
            log::warn!(
                "cross-volume transaction {:016x} is wholly inside volumes {vols:?}, which a \
                 PEER authority of this set appends to — SKIPPED at this mount. Rolling it \
                 forward would be a write to trees this node has no authority over; its \
                 owner's own mount recovers it",
                o.rec.tx_id
            );
        } else {
            crate::fuse_client::METRICS
                .xv_cross_owner_intents
                .fetch_add(1, Ordering::Relaxed);
            return Err(SqueezefsError::InvalidOperation(format!(
                "refusing to mount: open cross-volume transaction {:016x} ({:?}) spans two \
                 metadata OWNERS (volumes {vols:?}) — no process in this fleet can roll it \
                 forward, because each half needs the D0 claim of a different node. This is \
                 reachable only by a bug in the M1 cross-owner pre-check \
                 (xv_cross_owner_intents, a must-stay-0 counter): the remedy is the offline \
                 whole-set pass — unmount every owner and run `squeezefs fsck` — never a \
                 partial roll-forward",
                o.rec.tx_id, o.rec.op
            )));
        }
    }
    Ok(rolled)
}

/// Roll ONE intent forward. `Ok(true)` = retired; `Ok(false)` = a shipped
/// step could not reach its holder and the intent stays open (armed
/// plane); a LOCAL failure propagates as the mount refusal it always was.
async fn recover_one(routed: &RoutedMetaBackend, o: &OpenIntent) -> Result<bool> {
    let rec = &o.rec;
    note_intent_open(rec.tx_id);
    let armed = routed.volumes[o.host].slot_lease_armed();
    let localised: Vec<(usize, XvLocalStep)> =
        rec.steps.iter().map(|s| localise(routed, s)).collect();
    let guards = Arc::from(acquire_plan_guards(routed, &localised).await);

    for (i, (v_idx, local)) in localised.iter().enumerate() {
        let out = match apply_or_ship_step(
            routed,
            rec.tx_id,
            i,
            *v_idx,
            &rec.steps[i],
            local,
            None,
            Arc::clone(&guards),
        )
        .await
        {
            Ok(out) => out,
            Err(e) if armed && is_ship_failure(routed, *v_idx, local) => {
                log::warn!(
                    "cross-owner transaction {:016x} ({:?}): step {i} could not be shipped to \
                     its holder ({e}) — left open for the next roll-forward",
                    rec.tx_id,
                    rec.op
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

/// Acquire a recovered plan's LOCAL 4a guard set in the ONE canonical
/// order the DLM defines: ascending volume index, and inside each volume
/// `lock_many`'s canonical I-before-D, stripe-deduped, ascending-index
/// discipline. Every guard is taken before the first step commits, so a
/// recovery pass introduces no acquisition edge the live ops do not
/// already have. A step homed on a slot another appender leases takes no
/// guard here — its guard is the holder's, around the served apply.
async fn acquire_plan_guards(
    routed: &RoutedMetaBackend,
    localised: &[(usize, XvLocalStep)],
) -> Vec<dlm::DlmGuard> {
    let mut per_vol: Vec<(Vec<(Ino, dlm::LockMode)>, Vec<(Ino, String)>)> =
        (0..routed.volumes.len())
            .map(|_| (Vec::new(), Vec::new()))
            .collect();
    for (v_idx, step) in localised {
        if !matches!(
            step_home(routed, *v_idx, step.local_home()),
            StepHome::Local
        ) {
            continue;
        }
        let (inos, dentries) = &mut per_vol[*v_idx];
        match step {
            XvLocalStep::RemoveDentry {
                local_parent, name, ..
            }
            | XvLocalStep::InsertDentry {
                local_parent, name, ..
            } => {
                inos.push((*local_parent, dlm::LockMode::Exclusive));
                dentries.push((*local_parent, name.clone()));
            }
            XvLocalStep::SetNlink { local_ino, .. }
            | XvLocalStep::TouchCtime { local_ino, .. }
            | XvLocalStep::MintInode { local_ino, .. }
            | XvLocalStep::CreateInode { local_ino, .. } => {
                inos.push((*local_ino, dlm::LockMode::Exclusive));
            }
        }
    }
    let mut guards = Vec::new();
    for (v_idx, (inos, dentries)) in per_vol.iter().enumerate() {
        if inos.is_empty() && dentries.is_empty() {
            continue;
        }
        let d: Vec<(Ino, &str, dlm::LockMode)> = dentries
            .iter()
            .map(|(p, n)| (*p, n.as_str(), dlm::LockMode::Exclusive))
            .collect();
        guards.extend(routed.volumes[v_idx].dlm().lock_many(inos, &d).await);
    }
    guards
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
/// flight task ticking at the checkpoint LANDING ceiling — an intent a
/// live op left open (its holder down, its endpoint unbound at the mount's
/// own recovery) is completed within a few ceilings of the holder
/// returning, never only at the next mount. Ends when the set is dropped.
pub fn spawn_roll_forward_cadence(routed: std::sync::Weak<RoutedMetaBackend>, tick_ms: u64) {
    crate::meta_exec::spawn_meta("xv_roll_forward_cadence", async move {
        let tick = std::time::Duration::from_millis(tick_ms.max(1));
        loop {
            squeezefs_ipc::sqz_time::sleep(tick).await;
            let Some(routed) = routed.upgrade() else {
                return;
            };
            if OPEN_INTENTS.lock().is_empty() {
                continue;
            }
            if let Err(e) = roll_forward_open_intents(&routed).await {
                log::warn!("cross-owner roll-forward cadence: {e}");
            }
        }
    });
}
