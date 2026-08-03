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
//! # Reuse (S8)
//!
//! [`execute`] is the whole entry point: build an [`XvPlan`] of steps over
//! GLOBAL inos, hand it the op's guard set, done. Function-shipped metadata
//! (stage S8) reuses it verbatim for the cross-volume verbs; what it still
//! owes is a **remote** participant — the applier is a per-volume method
//! called in-process, so a step homed on a volume this node does not own
//! is not expressible today. That is S8's wire, not this stage's: when
//! `cluster_wire` can carry `Metadata` verbs, the remote leg replaces the
//! per-volume call inside `apply_step` and the record format does not
//! change (steps carry global inos, so the participant is resolved by
//! routing at apply time — a slot migration between crash and recovery is
//! handled for free).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::kv::backend::{KvMetaBackend, RoutedParentUpdate};
use super::kv::record::{xattr_key, InodeValue, HASH56_MAX, XATTR_KEY_LEN};
use super::{dlm, Ino, RoutedMetaBackend};
use crate::error::{Result, SqueezefsError};

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
    xattr_key(XV_INTENT_INO, tx_id & HASH56_MAX, (tx_id >> 56) as u8)
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
            _ => return None,
        })
    }
}

/// One step of a cross-volume plan, over **global** inos: the participant
/// is resolved by routing at apply time, so global-ino stability (the VL5a
/// law) makes a plan survive a slot migration between crash and recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl XvStep {
    /// The global ino whose volume HOMES this step (dentry steps home on
    /// the parent; the rest on their own inode).
    pub fn home_ino(&self) -> Ino {
        match self {
            Self::RemoveDentry { parent, .. } | Self::InsertDentry { parent, .. } => *parent,
            Self::SetNlink { ino, .. }
            | Self::TouchCtime { ino, .. }
            | Self::MintInode { ino, .. } => *ino,
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Self::RemoveDentry { .. } => 1,
            Self::InsertDentry { .. } => 2,
            Self::SetNlink { .. } => 3,
            Self::TouchCtime { .. } => 4,
            Self::MintInode { .. } => 5,
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
}

/// The intent record's rider on a step's transaction: the `Put` rides step
/// 0 (intent and first effect are then ONE checksummed entry — atomic by
/// construction, and free), the `Delete` is the retirement.
#[derive(Debug, Clone)]
pub enum XvRider {
    Put { tx_id: u64, image: Vec<u8> },
    Delete { tx_id: u64 },
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
    }
}

/// Apply one localised step on its volume. **The S8 seam**: a remote
/// participant replaces this call (and only this call) once `cluster_wire`
/// carries `Metadata` verbs — the plan, the record and the witnesses are
/// already node-agnostic.
async fn apply_step(
    routed: &RoutedMetaBackend,
    v_idx: usize,
    step: &XvLocalStep,
    rider: Option<&XvRider>,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<XvStepOutcome> {
    let out = routed.volumes[v_idx]
        .xv_apply_step(step, rider, guards)
        .await;
    if out.is_err() {
        routed.mirror_volume_failure(v_idx);
    }
    let out = out?;
    out.count();
    Ok(out)
}

/// Execute a cross-volume transaction under the op's ALREADY-HELD 4a
/// guard set (see §4.10a's acquisition-order rule: this machinery acquires
/// nothing, which is exactly why it adds no wait-for edge).
pub async fn execute(
    routed: &RoutedMetaBackend,
    plan: &XvPlan,
    guards: Arc<[dlm::DlmGuard]>,
) -> Result<XvExecution> {
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

    let mut outcomes = Vec::with_capacity(localised.len());
    for (i, (v_idx, step)) in localised.iter().enumerate() {
        if i >= allowed {
            return Err(seam_error());
        }
        let rider = (i == 0).then(|| XvRider::Put {
            tx_id,
            image: image.clone(),
        });
        match apply_step(routed, *v_idx, step, rider.as_ref(), guards.clone()).await {
            Ok(o) => {
                if i == 0 {
                    // Counted only once the intent record is DURABLE (it
                    // rode this commit): `started` therefore means "an
                    // intent exists", which is what makes
                    // `started == completed` the steady-state law.
                    XV_TX_STARTED.fetch_add(1, Ordering::Relaxed);
                }
                outcomes.push(o)
            }
            Err(e) => {
                if i == 0 {
                    // Step 0 and the intent are ONE entry: a failure here
                    // committed neither, so there is nothing to complete
                    // and nothing to escalate.
                    return Err(e);
                }
                escalate_midplan(routed, &localised, tx_id, i, &e);
                return Err(e);
            }
        }
        if i == 0 {
            // Cross-DEVICE ordering: the intent must be durable before any
            // later participant's entry is submitted, or a power cut could
            // keep a later effect and lose the intent that explains it. A
            // barrier that FAILS leaves exactly the durable-but-incomplete
            // shape a mid-plan step failure leaves, so it escalates the
            // same way.
            if let Err(e) = barrier(routed, *v_idx).await {
                escalate_midplan(routed, &localised, tx_id, i, &e);
                return Err(e);
            }
        }
    }

    // Every participant durable BEFORE the retirement: otherwise a power
    // cut could keep the retirement and lose a step.
    let mut participants: Vec<usize> = localised.iter().skip(1).map(|(v, _)| *v).collect();
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
    // Retirement is SYNCHRONOUS — before the op releases its guards. An
    // intent that outlived its transaction could meet a later, legitimate
    // mutation of the same objects, and recovery's witnesses assume no
    // such interleaving exists (§4.10a).
    if let Err(e) = routed.volumes[coord].xv_retire_intent(tx_id, guards).await {
        routed.mirror_volume_failure(coord);
        escalate_midplan(routed, &localised, tx_id, localised.len(), &e);
        return Err(e);
    }
    XV_TX_COMPLETED.fetch_add(1, Ordering::Relaxed);
    Ok(XvExecution { tx_id, outcomes })
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
// Recovery (mount, before the mount serves)
// ---------------------------------------------------------------------------

/// Roll every open cross-volume intent forward. Called from
/// `open_routed_meta_set` — i.e. on WRITE opens only (a read-only mount
/// never mints and never recovers, DLM S5), after each volume's journal
/// replay has made its records RAM-authoritative, and before the mount
/// serves anything.
///
/// Returns the number of transactions completed. The scan is bounded (one
/// 16-byte-prefix range per volume) and finds nothing on a healthy set, so
/// this is not a mount-time cost anybody pays twice.
pub async fn recover_open_intents(routed: &RoutedMetaBackend) -> Result<usize> {
    let mut open: Vec<(usize, IntentRecord)> = Vec::new();
    for (idx, vol) in routed.volumes.iter().enumerate() {
        for (tx_id, image) in vol.xv_scan_intents().await? {
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
            open.push((idx, rec));
        }
    }
    if open.is_empty() {
        return Ok(0);
    }
    // Deterministic order (ids are monotone per mount, so this is also
    // submission order for anything a single crash left behind).
    open.sort_by_key(|(_, r)| r.tx_id);
    let n = open.len();
    log::warn!(
        "cross-volume transaction recovery: {n} open intent(s) found at mount — rolling \
         forward (a crash interrupted them; this is the DUR-7 machinery working)"
    );
    for (host, rec) in open {
        recover_one(routed, host, &rec).await?;
    }
    Ok(n)
}

async fn recover_one(routed: &RoutedMetaBackend, host: usize, rec: &IntentRecord) -> Result<()> {
    let localised: Vec<(usize, XvLocalStep)> =
        rec.steps.iter().map(|s| localise(routed, s)).collect();
    let guards = Arc::from(acquire_plan_guards(routed, &localised).await);

    for (v_idx, step) in &localised {
        let out = apply_step(routed, *v_idx, step, None, Arc::clone(&guards)).await?;
        if out.status == XvStepStatus::ForeignSkipped {
            log::error!(
                "cross-volume transaction {:016x}: a step's object moved under the intent — \
                 skipped rather than clobbered (crossvol_tx_steps_foreign; run \
                 `squeezefs fsck`)",
                rec.tx_id
            );
        }
    }
    // The completed steps must be durable before the intent that explains
    // them is retired (the live path's rule, verbatim).
    let mut vols: Vec<usize> = localised.iter().map(|(v, _)| *v).collect();
    vols.push(host);
    vols.sort_unstable();
    vols.dedup();
    for v in vols {
        barrier(routed, v).await?;
    }
    routed.volumes[host]
        .xv_retire_intent(rec.tx_id, guards)
        .await?;
    XV_TX_RECOVERED.fetch_add(1, Ordering::Relaxed);
    log::info!(
        "cross-volume transaction {:016x} ({:?}, {} step(s)) rolled forward and retired",
        rec.tx_id,
        rec.op,
        rec.steps.len()
    );
    Ok(())
}

/// Acquire a recovered plan's whole 4a guard set in the ONE canonical
/// order the DLM defines: ascending volume index, and inside each volume
/// `lock_many`'s canonical I-before-D, stripe-deduped, ascending-index
/// discipline. Every guard is taken before the first step commits, so a
/// recovery pass introduces no acquisition edge the live ops do not
/// already have.
async fn acquire_plan_guards(
    routed: &RoutedMetaBackend,
    localised: &[(usize, XvLocalStep)],
) -> Vec<dlm::DlmGuard> {
    let mut per_vol: Vec<(Vec<(Ino, dlm::LockMode)>, Vec<(Ino, String)>)> =
        (0..routed.volumes.len())
            .map(|_| (Vec::new(), Vec::new()))
            .collect();
    for (v_idx, step) in localised {
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
            | XvLocalStep::MintInode { local_ino, .. } => {
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
