//! The S8 **verb vocabulary** — what a function-shipped metadata
//! operation looks like on the S3 wire (spec §6.7 decision 1, §6.9 S8).
//!
//! # The census this vocabulary covers
//!
//! `crate::meta_backend::Metadata` has **13 required members** plus one
//! *provided* member:
//!
//! | Trait member | Wire verb |
//! |---|---|
//! | `lookup` | [`MetaVerb::LookupDentry`] ⊕ [`MetaVerb::Getattr`] (see below) |
//! | `create` (provided) | none of its own — it delegates to `create_with_rdev` |
//! | `create_with_rdev` | [`MetaVerb::CreateWithRdev`] |
//! | `unlink` | [`MetaVerb::Unlink`] |
//! | `link` | [`MetaVerb::Link`] |
//! | `rename` | [`MetaVerb::Rename`] |
//! | `readdir` | [`MetaVerb::Readdir`] |
//! | `getattr` | [`MetaVerb::Getattr`] |
//! | `setattr` | [`MetaVerb::Setattr`] |
//! | `getxattr` / `setxattr` / `removexattr` / `listxattr` | the four xattr verbs |
//! | `destroy_inode` | [`MetaVerb::DestroyInode`] |
//!
//! There is **no `statfs` member** on the trait (the FUSE `statfs` handler
//! aggregates per-volume capacity, not a `Metadata` call), so none ships.
//! The non-trait capability surface — `create_with_rdev_size`,
//! `readdir_stream`, `xattr_value_cap`, `set_layout_and_size`,
//! `merge_layout_and_size`, `commit_block_refs`, `park_write_times`,
//! `destroy_inodes` — is deliberately **not** shipped here: those are the
//! data plane's publish path (S9) and the daemon cannot be switched onto a
//! shipping router until they ship too. Stated plainly rather than stubbed.
//!
//! # Why `lookup` is two verbs
//!
//! `RoutedMetaBackend::lookup` reads the parent's dentry and then does a
//! `getattr` on the child — and the child's inode can live on a volume
//! **another node owns**, which the parent's owner may not read (one node
//! cache per volume). So the router composes the trait call from two
//! independently-routed verbs. This is not a semantic change: the shipped
//! implementation performs exactly those two steps, and the code says in
//! so many words that "lookup→getattr was never atomic".
//!
//! The decomposition is **not paid when it is not needed**: an owner that
//! holds the child's volume too resolves it inline and answers with the
//! inode, so the common shape (every single-volume set, every set whose
//! volumes share an owner) costs ONE round trip. Paying two
//! unconditionally would double the latency of the hottest metadata verb
//! — every path walk — for a shape that cannot occur on the set it runs
//! against.
//!
//! # Frame shape, and why each field is on it
//!
//! * `schema` — the vocabulary's own version, independent of the
//!   transport's (`CLUSTER_WIRE_SCHEMA`). A mismatch is refused loud,
//!   never guessed: a custody-bearing protocol has no safe guess.
//! * `client_epoch` + per-op `id` — the **idempotency key** (§6.7's
//!   at-most-once requirement made concrete): the owner's dedup window is
//!   keyed on the pair, so a retry after a lost reply returns the
//!   original outcome instead of re-applying.
//! * `owner_term` — the client's belief about the owner's durable era. A
//!   successor bumps `term` durably before arming (spec §6.7 "Recovery"),
//!   so an old-term frame is stale **by construction** and is refused
//!   whole rather than partly applied.
//! * per-result `grant` — the **intent-lock piggyback** (§6.7 decision
//!   3): the object's fencing generation as the OWNER knows it rides the
//!   reply of the metadata RPC the operation was going to issue anyway, so
//!   acquisition is never a separate round trip and a foreign-home fencing
//!   read has a sound answer.
//!
//! Bodies are bincode, encoded unbounded (we build them) and decoded
//! **bounded** (untrusted) — the same discipline `cluster_wire`'s codec
//! documents: a length in a frame is a claim, never an allocation
//! authority.

use crate::error::{Result, SqueezefsError};
use crate::meta_backend::{DirEntry, Inode};
use bincode::Options as _;
use serde::{Deserialize, Serialize};

/// The verb vocabulary's schema. Bumped when a verb's payload changes;
/// independent of the transport's framing version.
///
/// **2** (rung 12 — S10 LOOKUP-class delegations, design §11 "schema +1"):
/// request frames carry the client's KD-MW-2 identity, per-op results
/// carry piggybacked [`DelegGrant`]s and reply-ridden revocations, and the
/// vocabulary gains the [`VERB_DELEG_RECALL`] / [`VERB_DELEG_REASSERT`]
/// verbs. A schema-1 peer refuses loud (KD-MW-11: wire schema versions
/// carry compatibility; no incompat bit — delegations are RAM).
///
/// **3** (rung 13 — KD-MW-13 per-directory EXCLUSIVE UPDATE grants +
/// asynchronous create-intent batches): per-op results gain the optional
/// [`IntentGrant`] (the UPDATE authority: dentry census + ino supply,
/// riding the reply of a shipped create — the intent-lock law applied to
/// mint authority), and the vocabulary gains [`VERB_DELEG_INTENT`] — the
/// batched intent-apply verb, era-gated on the custody `lease_epoch` and
/// witnessed on `(lease_epoch, request_id)` FROM BIRTH (the rung-9
/// finding-#6 law: a custody-bearing mutation verb never ships un-gated).
pub const META_SHIP_SCHEMA: u32 = 3;

/// `cluster_wire` RPC verb carrying a batch of metadata calls. S3 reserved
/// 0 for its ping and S4's lock verbs take the low numbers; the metadata
/// plane starts at 16 so the two vocabularies can grow without collision.
pub const VERB_META_BATCH: u16 = 16;

/// `cluster_wire` RPC verb carrying a grace-window **reclaim** (spec §6.7
/// "Recovery": a successor's grace window admits only these).
pub const VERB_RECLAIM: u16 = 17;

/// Frame status: the frame was admitted and every op has its own outcome.
pub const STATUS_OK: u16 = crate::cluster_wire::RPC_OK;
/// Frame status: the peer speaks another vocabulary version.
pub const STATUS_SCHEMA: u16 = 32;
/// Frame status: the frame names an era the owner has moved past — refused
/// WHOLE, nothing executed.
pub const STATUS_STALE_TERM: u16 = 33;
/// Frame status: the owner is inside its failover grace window and the
/// frame carried a fresh mutation.
pub const STATUS_IN_GRACE: u16 = 34;
/// Frame status: undecodable body (bounded, refused loud).
pub const STATUS_MALFORMED: u16 = 35;
/// Frame status: the owner does not own the target volume — the client's
/// ownership map is stale.
pub const STATUS_NOT_OWNER: u16 = 36;
/// Frame status: the owner-side execution PANICKED (must-stay-0).
pub const STATUS_PANIC: u16 = 37;
/// Frame status: the caller is a FENCED delegation holder — its recall
/// deadline expired and it was escalated to membership eviction (rung-11
/// law: a timed-out grant is DEAD). Its poll and re-assert refuse whole;
/// re-admission is by remount, the documented posture.
pub const STATUS_DELEG_FENCED: u16 = 38;

/// The delegation verb block: its own range so the S8 metadata block
/// (16/17), S9 custody (`0x0200`) and publish (`0x0300`) can all grow
/// without collision.
pub const VERB_DELEG_BASE: u16 = 0x0400;
/// The holder's standing **recall channel** (the DelegRecall verb): the
/// client's call IS the channel — the owner parks it until recalls are
/// pending (or its park bound elapses) and answers with the batched
/// [`WireRecallFrame`]s; the client's NEXT call on the channel carries the
/// acks. Owner-initiated push over a dial-only wire, without a second
/// listener.
pub const VERB_DELEG_RECALL: u16 = VERB_DELEG_BASE;
/// Grace re-assertion (the DelegReassert verb): delegations are RAM
/// (KD-MW-5), so a holder re-asserts them to a successor authority inside
/// its grace window and receives fresh-era grants; un-reasserted grants
/// are gone (the NFSv4 law; the `VERB_RECLAIM` shape one plane up).
pub const VERB_DELEG_REASSERT: u16 = VERB_DELEG_BASE + 1;
/// The **intent batch** (rung 13, KD-MW-13): the UPDATE holder's flush —
/// an ordered batch of locally-acked child mutations (creates at
/// pre-supplied inos + deferred setattrs on pending inos) applied by the
/// owner. Era-gated (owner term + custody lease epoch) and witnessed
/// (`(lease_epoch, request_id)` dedup) FROM BIRTH.
pub const VERB_DELEG_INTENT: u16 = VERB_DELEG_BASE + 2;
/// Last verb of the delegation block.
pub const VERB_DELEG_LAST: u16 = 0x04FF;

/// Frame status (rung 13): the intent batch presented a custody lease
/// epoch the authority cannot verify as live custody — the batch dies
/// with the custody fence (the publish path's `PUBLISH_STALE_LEASE` law
/// on this verb). Refused BEFORE the witness window: a dead era's replay
/// must never be answered from cache.
pub const STATUS_INTENT_LEASE: u16 = 39;

/// The LOOKUP capability bit (design §8.2's mode table): dentry + attr
/// reads of the delegated object serve from the holder's
/// reader-revalidation view under the coherence promise. `PERM`/`XATTR`
/// are later rows' and deliberately not defined yet — an undefined bit
/// cannot be granted by accident.
pub const DELEG_CLASS_LOOKUP: u8 = 1;
/// The UPDATE capability bit (rung 13, KD-MW-13): child-entry MINT
/// authority over one directory — EXCLUSIVE per directory (recall-on-
/// conflict), carried as [`IntentGrant`] beside the LOOKUP-class
/// [`DelegGrant`]s (a bitmask so one entry can carry both classes).
pub const DELEG_CLASS_UPDATE: u8 = 2;

/// The verbs on the wire — the trait census above, one code each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MetaVerb {
    /// The dentry half of `lookup`. Answers with the resolved **inode**
    /// when the owner also holds the child's volume (the common shape —
    /// so a shipped `lookup` is ONE round trip), and with the child's
    /// **ino** when the child is owned elsewhere, for the client to route
    /// the `getattr` itself.
    LookupDentry = 1,
    /// `create_with_rdev` (and therefore the provided `create`).
    CreateWithRdev = 2,
    Unlink = 3,
    Link = 4,
    Rename = 5,
    Readdir = 6,
    Getattr = 7,
    Setattr = 8,
    Getxattr = 9,
    Setxattr = 10,
    Removexattr = 11,
    Listxattr = 12,
    DestroyInode = 13,
    // PR 6 (symmetric cross-owner transactions): the block 0x60–0x6F.
    /// ONE step of a cross-owner intent, applied by the step's slot
    /// holder through `xv_apply_step` (design-symmetric-metadata §5.6).
    XvStep = 0x60,
    /// An EXACT `(parent, name)` resolution served from the parent's
    /// slot holder's RAM-authoritative tree — the set-wide directory-
    /// rename lock's ancestor check reads each link through it (§5.6.4).
    LookupExact = 0x61,
    /// The initiator's 4a guards on objects in the holder's slots, parked
    /// at the holder under a scope for the op's duration — §5.6 line 1,
    /// "foreign-home guards travel" (the served step then applies without
    /// taking a guard, so it can never park behind the initiator).
    XvGuards = 0x62,
    /// Release a scope's parked guards (idempotent — `Unit` for a scope
    /// the holder no longer has).
    XvRelease = 0x63,
}

impl MetaVerb {
    /// Every shipped verb — the census, in code order.
    pub const ALL: &'static [MetaVerb] = &[
        MetaVerb::LookupDentry,
        MetaVerb::CreateWithRdev,
        MetaVerb::Unlink,
        MetaVerb::Link,
        MetaVerb::Rename,
        MetaVerb::Readdir,
        MetaVerb::Getattr,
        MetaVerb::Setattr,
        MetaVerb::Getxattr,
        MetaVerb::Setxattr,
        MetaVerb::Removexattr,
        MetaVerb::Listxattr,
        MetaVerb::DestroyInode,
        MetaVerb::XvStep,
        MetaVerb::LookupExact,
        MetaVerb::XvGuards,
        MetaVerb::XvRelease,
    ];

    /// The verb's wire code.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Decode a wire code.
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.code() == code)
    }

    /// The verb's name (log lines, refusals, the phase tables).
    pub fn name(self) -> &'static str {
        match self {
            MetaVerb::LookupDentry => "lookup_dentry",
            MetaVerb::CreateWithRdev => "create_with_rdev",
            MetaVerb::Unlink => "unlink",
            MetaVerb::Link => "link",
            MetaVerb::Rename => "rename",
            MetaVerb::Readdir => "readdir",
            MetaVerb::Getattr => "getattr",
            MetaVerb::Setattr => "setattr",
            MetaVerb::Getxattr => "getxattr",
            MetaVerb::Setxattr => "setxattr",
            MetaVerb::Removexattr => "removexattr",
            MetaVerb::Listxattr => "listxattr",
            MetaVerb::DestroyInode => "destroy_inode",
            MetaVerb::XvStep => "xv_step",
            MetaVerb::LookupExact => "lookup_exact",
            MetaVerb::XvGuards => "xv_guards",
            MetaVerb::XvRelease => "xv_release",
        }
    }

    /// Does the verb MUTATE durable metadata?
    ///
    /// Two mechanisms key on this, which is why it lives in one place:
    /// the **dedup window** (a read is naturally idempotent, so it never
    /// consumes a window entry) and the **grace gate** (a reader takes no
    /// grant, so a grace window need not refuse it).
    pub fn mutating(self) -> bool {
        match self {
            MetaVerb::LookupDentry
            | MetaVerb::Readdir
            | MetaVerb::Getattr
            | MetaVerb::Getxattr
            | MetaVerb::Listxattr
            | MetaVerb::LookupExact => false,
            MetaVerb::CreateWithRdev
            | MetaVerb::Unlink
            | MetaVerb::Link
            | MetaVerb::Rename
            | MetaVerb::Setattr
            | MetaVerb::Setxattr
            | MetaVerb::Removexattr
            | MetaVerb::DestroyInode
            | MetaVerb::XvStep
            // The guard verbs write no record, but a RESEND must be
            // answered from the winner's outcome (a second `lock_many`
            // under a scope already parked would wait behind itself), so
            // they ride the dedup window like every mutating verb.
            | MetaVerb::XvGuards
            | MetaVerb::XvRelease => true,
        }
    }
}

/// One metadata call, with its arguments verbatim from the trait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetaCall {
    LookupDentry {
        parent: u64,
        name: String,
    },
    CreateWithRdev {
        parent: u64,
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    },
    Unlink {
        parent: u64,
        name: String,
    },
    Link {
        ino: u64,
        new_parent: u64,
        new_name: String,
    },
    Rename {
        old_parent: u64,
        old_name: String,
        new_parent: u64,
        new_name: String,
        flags: u32,
    },
    Readdir {
        dir: u64,
        offset: u64,
        max: u32,
    },
    Getattr {
        ino: u64,
    },
    Setattr {
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    },
    Getxattr {
        ino: u64,
        name: String,
    },
    Setxattr {
        ino: u64,
        name: String,
        value: Vec<u8>,
    },
    Removexattr {
        ino: u64,
        name: String,
    },
    Listxattr {
        ino: u64,
    },
    DestroyInode {
        ino: u64,
    },
    // PR 6 — appended (the enum's serde index grows at the end; the
    // explicit discriminants live on `MetaVerb`, 0x60–0x6F).
    /// One step of cross-owner intent `tx_id` (its `step_idx`-th), applied
    /// by the holder of the step's slot. Idempotent under the step's
    /// `(pre, post)` witness — a resend, a successor holder's first sight
    /// of it and a roll-forward all answer the same outcome.
    XvStep {
        tx_id: u64,
        step_idx: u32,
        step: crate::meta_backend::crossvol_tx::XvStep,
        /// The guard scope the step applies under (`XvGuards`); 0 = none
        /// travelled (a roll-forward without a scope) — the holder takes
        /// the step's guards for the apply.
        scope: u64,
    },
    /// The exact `(parent, name)` resolution (`MetaReply::DentryExact`).
    LookupExact {
        parent: u64,
        name: String,
    },
    /// Park the initiator's 4a guards at the holder under `scope`:
    /// `inodes` = `(global ino, exclusive)`, `dentries` = `(global parent,
    /// name, exclusive)` — one canonical `lock_many` at the holder, held
    /// until `XvRelease` or the initiator's lease expiry.
    XvGuards {
        scope: u64,
        inodes: Vec<(u64, bool)>,
        dentries: Vec<(u64, String, bool)>,
    },
    /// Release scope `scope`'s parked guards; `ino` names one of its
    /// objects (the verb's routing/authority ino).
    XvRelease {
        scope: u64,
        ino: u64,
    },
}

impl MetaCall {
    /// The verb this call is.
    pub fn verb(&self) -> MetaVerb {
        match self {
            MetaCall::LookupDentry { .. } => MetaVerb::LookupDentry,
            MetaCall::CreateWithRdev { .. } => MetaVerb::CreateWithRdev,
            MetaCall::Unlink { .. } => MetaVerb::Unlink,
            MetaCall::Link { .. } => MetaVerb::Link,
            MetaCall::Rename { .. } => MetaVerb::Rename,
            MetaCall::Readdir { .. } => MetaVerb::Readdir,
            MetaCall::Getattr { .. } => MetaVerb::Getattr,
            MetaCall::Setattr { .. } => MetaVerb::Setattr,
            MetaCall::Getxattr { .. } => MetaVerb::Getxattr,
            MetaCall::Setxattr { .. } => MetaVerb::Setxattr,
            MetaCall::Removexattr { .. } => MetaVerb::Removexattr,
            MetaCall::Listxattr { .. } => MetaVerb::Listxattr,
            MetaCall::DestroyInode { .. } => MetaVerb::DestroyInode,
            MetaCall::XvStep { .. } => MetaVerb::XvStep,
            MetaCall::LookupExact { .. } => MetaVerb::LookupExact,
            MetaCall::XvGuards { .. } => MetaVerb::XvGuards,
            MetaCall::XvRelease { .. } => MetaVerb::XvRelease,
        }
    }

    /// Does this call mutate durable metadata?
    pub fn mutating(&self) -> bool {
        self.verb().mutating()
    }

    /// Does this call PARK at the holder by design — PR 6's travelling
    /// guard (`XvGuards` waits on the stripe the previous scope still
    /// holds) and the `XvRelease` that unparks one? Such a call never
    /// rides the stop-and-wait lane, where it would head the line in
    /// front of the very release it waits for (`ShipLane::mux`).
    pub fn parks_at_holder(&self) -> bool {
        matches!(self, MetaCall::XvGuards { .. } | MetaCall::XvRelease { .. })
    }

    /// The **primary object**: the inode whose owner executes the call,
    /// and whose grant rides the reply.
    ///
    /// For the two-participant verbs it is deliberately the one whose
    /// *dentry* the operation writes (the parent), because that is the
    /// object whose 4a `D{parent:name}` guard serializes the op.
    pub fn primary_ino(&self) -> u64 {
        match self {
            MetaCall::LookupDentry { parent, .. }
            | MetaCall::CreateWithRdev { parent, .. }
            | MetaCall::Unlink { parent, .. } => *parent,
            MetaCall::Link { new_parent, .. } => *new_parent,
            MetaCall::Rename { old_parent, .. } => *old_parent,
            MetaCall::Readdir { dir, .. } => *dir,
            MetaCall::Getattr { ino }
            | MetaCall::Setattr { ino, .. }
            | MetaCall::Getxattr { ino, .. }
            | MetaCall::Setxattr { ino, .. }
            | MetaCall::Removexattr { ino, .. }
            | MetaCall::Listxattr { ino }
            | MetaCall::DestroyInode { ino } => *ino,
            MetaCall::XvStep { step, .. } => step.home_ino(),
            MetaCall::LookupExact { parent, .. } => *parent,
            MetaCall::XvGuards {
                inodes, dentries, ..
            } => inodes
                .first()
                .map(|(i, _)| *i)
                .or_else(|| dentries.first().map(|(p, _, _)| *p))
                .unwrap_or(0),
            MetaCall::XvRelease { ino, .. } => *ino,
        }
    }

    /// Every inode the call **names** — the routing + cross-owner
    /// detection set.
    ///
    /// "Names", not "touches": `unlink`'s child and `rename`'s moved and
    /// overwritten inodes are *discovered* under guards, so the client
    /// cannot see them and the owner checks them after discovery.
    pub fn named_inos(&self) -> Vec<u64> {
        match self {
            MetaCall::Link {
                ino, new_parent, ..
            } => vec![*new_parent, *ino],
            MetaCall::Rename {
                old_parent,
                new_parent,
                ..
            } => vec![*old_parent, *new_parent],
            MetaCall::XvGuards {
                inodes, dentries, ..
            } => inodes
                .iter()
                .map(|(i, _)| *i)
                .chain(dentries.iter().map(|(p, _, _)| *p))
                .collect(),
            other => vec![other.primary_ino()],
        }
    }
}

/// An inode as it crosses the wire — a mirror of
/// [`crate::meta_backend::Inode`] on purpose: the internal struct is free
/// to gain fields without silently changing a wire format, and the
/// vocabulary's schema is what versions this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireInode {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub flags: u32,
    pub rdev: u32,
}

impl From<&Inode> for WireInode {
    fn from(i: &Inode) -> Self {
        Self {
            ino: i.ino,
            mode: i.mode,
            uid: i.uid,
            gid: i.gid,
            size: i.size,
            nlink: i.nlink,
            atime: i.atime,
            mtime: i.mtime,
            ctime: i.ctime,
            flags: i.flags,
            rdev: i.rdev,
        }
    }
}

impl From<WireInode> for Inode {
    fn from(w: WireInode) -> Self {
        Inode {
            ino: w.ino,
            mode: w.mode,
            uid: w.uid,
            gid: w.gid,
            size: w.size,
            nlink: w.nlink,
            atime: w.atime,
            mtime: w.mtime,
            ctime: w.ctime,
            flags: w.flags,
            rdev: w.rdev,
        }
    }
}

/// A directory entry as it crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireDirEntry {
    pub ino: u64,
    pub name: String,
    pub file_type: u32,
}

impl From<&DirEntry> for WireDirEntry {
    fn from(e: &DirEntry) -> Self {
        Self {
            ino: e.ino,
            name: e.name.clone(),
            file_type: e.file_type,
        }
    }
}

impl From<WireDirEntry> for DirEntry {
    fn from(w: WireDirEntry) -> Self {
        DirEntry {
            ino: w.ino,
            name: w.name,
            file_type: w.file_type,
        }
    }
}

/// One call's successful payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetaReply {
    /// `create_with_rdev`, `link`, `getattr`, `setattr`, and
    /// `lookup_dentry` when the owner resolved the child itself.
    Inode(WireInode),
    /// `lookup_dentry` when the child is owned ELSEWHERE (its global
    /// ino, for the client to route), and `unlink` (the child it
    /// removed).
    Ino(u64),
    /// `rename`, `setxattr`, `removexattr`, `destroy_inode`.
    Unit,
    /// `readdir`.
    Dir(Vec<WireDirEntry>),
    /// `getxattr` — `None` = the attribute is absent.
    Xattr(Option<Vec<u8>>),
    /// `listxattr`.
    Names(Vec<String>),
    // PR 6 — appended.
    /// `xv_step`: the applier's verdict (`crossvol_tx::status_code` — 0
    /// applied, 1 already applied, 2 the object moved under the plan) and
    /// the post-image where the step produced one.
    XvStep {
        status: u8,
        inode: Option<WireInode>,
    },
    /// `lookup_exact`: `(child ino, S_IFMT bits)` or absent — exact as the
    /// holder's tree stands.
    DentryExact(Option<(u64, u32)>),
}

/// A refusal as it crosses the wire.
///
/// **The errno is the payload** (POSIX-6): `SqueezefsError::to_errno` is
/// structural and total, so shipping the number and reconstructing a
/// `Refused { errno }` preserves what userspace sees byte for byte. The
/// message is prose for the operator and is never parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub errno: i32,
    pub msg: String,
}

impl WireError {
    /// Render a local error for the wire.
    pub fn from_error(e: &SqueezefsError) -> Self {
        Self {
            errno: e.to_errno(),
            msg: e.to_string(),
        }
    }

    /// Rebuild a local error that presents the SAME errno.
    pub fn into_error(self) -> SqueezefsError {
        SqueezefsError::refused(self.errno, self.msg)
    }
}

/// The intent-lock piggyback: an object's fencing generation as the owner
/// knows it, riding the reply of the verb that named the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenGrant {
    /// The object.
    pub ino: u64,
    /// Its current fencing generation (composed `(term << 40) | seq`).
    pub token: u64,
    /// The owner's durable era, so a client can tell a fresh grant from
    /// one minted before a failover.
    pub term: u64,
}

/// One op in a batch: the idempotency id plus the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaOp {
    /// Client-chosen, monotone per client epoch. The dedup window's key.
    pub id: u64,
    pub call: MetaCall,
}

/// One op's outcome, id-correlated with its request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaOpResult {
    pub id: u64,
    pub outcome: std::result::Result<MetaReply, WireError>,
    /// The piggybacked grant (absent when the op's object does not exist
    /// or the outcome was a refusal).
    pub grant: Option<TokenGrant>,
    /// Piggybacked **delegation grants** (S10, schema 2): the intent-lock
    /// law applied to delegations — acquisition rides the reply of the
    /// metadata RPC the client was already issuing, never its own round
    /// trip. A LOOKUP-class verb over-issues here (the parent AND the
    /// resolved child — Ceph's move); empty on mutations, refusals, and
    /// lever-off owners.
    pub delegs: Vec<DelegGrant>,
    /// Reply-ridden **revocations** (S10): the mutating holder's OWN
    /// grants on the objects this op invalidated. They die with this
    /// reply — the client drops them before the caller's await returns
    /// (read-your-own-writes by construction) — never through a wire
    /// recall, so a serial mutate-then-lookup stream pays zero added
    /// round trips (the tar-x shape the design's R1 recovery is for).
    pub revokes: Vec<u64>,
    /// The owner's delegation-sequence fence at revoke time: any in-flight
    /// grant on a revoked ino with `seq <= revoke_fence` is dead on
    /// arrival (the cross-session grant/recall ordering law — see
    /// [`DelegGrant::seq`]). `0` when `revokes` is empty.
    pub revoke_fence: u64,
    /// The piggybacked **EXCLUSIVE UPDATE grant** (rung 13, schema 3):
    /// rides the reply of a successful shipped CREATE into a directory
    /// this client may hold mint authority over (the parent, and the
    /// created directory itself). `None` on every other verb, on
    /// refusals, on lever-off owners, and when the exclusivity/valve/
    /// census gates decline.
    pub intent_grant: Option<IntentGrant>,
}

/// The stamp a delegation grant carries: the object's volume's **commit
/// watermark** (journal reservation frontier) at grant time. The holder
/// serves locally only while its own view's covered journal prefix
/// reaches the stamp — which closes the warming window (a view that has
/// not caught up serves nothing) without tightening the reader staleness
/// bound. Soundness: the v3 KV's whole-tx atomicity + checkpoint-prefix
/// visibility mean a view whose tail covers the watermark includes every
/// transaction at or below the grant's mint — the dentry set is exactly
/// as current as the grant.
///
/// **Why a journal position and not the inode's `(ctime, mtime, size)`**
/// (rung-12 live finding #4): the attr triple ALIASES — two creates
/// inside one clock tick leave the parent's attrs equal while its dentry
/// set differs, and the first fleet `tar -x` served a stale
/// authoritative NEGATIVE (`utime: No such file or directory` on a
/// just-created directory) from exactly that window. A reservation
/// frontier cannot alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegStamp {
    /// The object's volume's commit watermark at grant time.
    pub watermark: u64,
}

/// A **delegation grant** (S10 lever 1): a capability token over one
/// object, granted by the owning authority and cached client-side. RAM
/// only (KD-MW-5) — reconstructed by re-assertion after failover, never
/// durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegGrant {
    /// The delegated object.
    pub ino: u64,
    /// Capability bits ([`DELEG_CLASS_LOOKUP`] is the only defined one).
    pub class: u8,
    /// Is the object a directory (whether dentry reads are servable)?
    pub dir: bool,
    /// Owner-minted monotone sequence. Grants and recalls travel on
    /// DIFFERENT sessions (the batch lane vs the recall channel), so a
    /// recall carries the fence `seq` at issue and the client drops any
    /// later-arriving grant at or below it — the reordering race closed
    /// without cross-session ordering.
    pub seq: u64,
    /// The owner's durable era (a failover's fresh grants carry the
    /// successor's term; stale-era entries never serve).
    pub term: u64,
    /// The view-currency stamp (see [`DelegStamp`]).
    pub stamp: DelegStamp,
}

/// A **pre-reserved ino supply** (rung 13): the mint authority's number
/// half. The owner reserves `count` fresh locals from ONE (volume, slot)
/// cursor by advancing it — so the numbers can never be re-minted by the
/// owner within its incarnation — and the holder mints
/// `first_global + k × stride` locally. Unused inos BURN at grant death
/// (§4.8's monotonic-allocation law: a burned ino is free), and a supply
/// never survives an owner era: the flush frame presents the supply's own
/// `owner_term`, so a successor's era gate refuses every stale-supply
/// apply before its recovered cursor could collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InoSupply {
    /// The first reserved GLOBAL ino.
    pub first_global: u64,
    /// Global stride between consecutive reserved locals (the routing
    /// width's slot encoding is affine in the local — asserted at
    /// reservation).
    pub stride: u64,
    /// Reserved count.
    pub count: u32,
}

/// The **EXCLUSIVE UPDATE grant** (rung 13, KD-MW-13): per-directory
/// child-entry mint authority, riding the reply of the shipped create the
/// client was already issuing (the intent-lock law).
///
/// The `census` is the §8.2 "dentry-version revalidation at grant",
/// discharged as the ENTRY SET itself: D's names snapshotted at grant
/// time, bounded by the wire's CONTROL budget (an over-budget directory
/// DECLINES the grant — the design's priced fallback, never a correctness
/// fork). Exclusivity keeps it exact thereafter — every foreign mutation
/// of D recalls this grant first, and the holder folds its OWN mutations
/// in — so a local negative lookup IS authoritative and `O_EXCL` is
/// decidable locally. A volume-wide watermark deliberately does NOT gate
/// mints: it never settles under a create storm (the rung-12 measured
/// tar-x disengagement), which is exactly the shape this grant exists for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentGrant {
    /// The delegated directory.
    pub dir: u64,
    /// Owner-minted monotone sequence (the [`DelegGrant::seq`] space —
    /// recalls fence both classes with one number).
    pub seq: u64,
    /// The owner's durable era.
    pub term: u64,
    /// The directory's FULL attributes at grant time — two consumers:
    /// the setgid-inheritance inputs (mode/gid, computed at mint AND
    /// re-computed at apply; exclusivity keeps the two equal), and the
    /// holder's LOCAL parent-attr serve. The second is a measured live
    /// finding: the FUSE create handler's D2.c parent refresh shipped ONE
    /// getattr per minted create — the exact round trip the grant exists
    /// to delete. Exclusivity keeps the image exact (every foreign
    /// mutation of D recalls this grant first) and the holder folds its
    /// own mints in (Δtimes, mkdir Δnlink).
    pub dir_attrs: WireInode,
    /// D's complete name set at grant time (≤ the census budget).
    pub census: Vec<String>,
    /// The mint supply riding this grant (`None` when the reply already
    /// carried one — one supply chunk per reply).
    pub supply: Option<InoSupply>,
}

/// One locally-acked child mutation inside an intent batch (rung 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentCall {
    /// A create at a PRE-SUPPLIED global ino (the client minted the
    /// number from its [`InoSupply`]; the owner applies the record AT
    /// that ino). `ts_ns` is the client's mint instant — the applied
    /// record's times, so a `stat` answers the same times before and
    /// after the flush.
    CreateAt {
        parent: u64,
        name: String,
        ino: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        ts_ns: u64,
    },
    /// A deferred setattr on a PENDING ino (the tar `utimensat` shape) —
    /// ordered strictly after the create it names inside the batch.
    /// Deliberately sizeless: a size change is a data-plane act and
    /// barriers + ships instead.
    SetattrAt {
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    },
}

impl IntentCall {
    /// The directory whose deferred-refusal latch a failure of this op
    /// lands on.
    pub fn latch_dir(&self, pending_dir_of: impl Fn(u64) -> Option<u64>) -> Option<u64> {
        match self {
            IntentCall::CreateAt { parent, .. } => Some(*parent),
            IntentCall::SetattrAt { ino, .. } => pending_dir_of(*ino),
        }
    }
}

/// One intent op: the witness id plus the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentOp {
    /// Client-chosen, monotone per process — the `(lease_epoch,
    /// request_id)` witness's other half.
    pub request_id: u64,
    pub call: IntentCall,
}

/// The intent batch (the **VERB_DELEG_INTENT** request): one flush of the
/// holder's pending intents, executed IN ORDER on the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentBatchFrame {
    pub schema: u32,
    pub client_epoch: u64,
    /// The holder's KD-MW-2 identity (the grant bookkeeping's key AND the
    /// custody-lease identity the era gate verifies).
    pub client_id: String,
    /// The owner era the batch's supply + grants were minted under. A
    /// mismatch refuses the frame WHOLE — which is what makes a dead
    /// era's supply structurally un-appliable (see [`InoSupply`]).
    pub owner_term: u64,
    /// The custody lease epoch (the era gate's input — intents die with
    /// the custody fence).
    pub lease_epoch: u64,
    /// Refill request: how many supply inos the holder wants back on the
    /// reply (0 = none).
    pub supply_request: u32,
    /// Executed in ORDER: in-batch causality is submission order (a
    /// create and the setattr that names it ride one batch, ordered).
    pub ops: Vec<IntentOp>,
}

/// One intent op's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentResult {
    pub request_id: u64,
    /// `Err` = the deferred refusal (§8.2 law 2): the client latches the
    /// errno onto the directory and DESTROYS the local mint.
    pub outcome: std::result::Result<(), WireError>,
    /// Reply-ridden revocations of the FLUSHING holder's own LOOKUP-class
    /// grants that this op's apply invalidated (the self-conflict
    /// surrender law, on this verb's reply).
    pub revokes: Vec<u64>,
    /// The fence for `revokes` (see [`MetaOpResult::revoke_fence`]).
    pub revoke_fence: u64,
}

/// The intent batch's reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentBatchReply {
    pub schema: u32,
    pub owner_term: u64,
    pub results: Vec<IntentResult>,
    /// The requested supply refill (`None` when none was requested or the
    /// reservation could not be made).
    pub supply: Option<InoSupply>,
}

/// A batch of calls against ONE owner: the **pipelining unit**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaRequestFrame {
    pub schema: u32,
    /// The client's incarnation — half of the dedup key, and what makes a
    /// restarted client's reused ids un-confusable with its old ones.
    pub client_epoch: u64,
    /// The client's KD-MW-2 identity (`node_{16hex}.m{8hex}` in
    /// production) — the key delegation grants and recalls are
    /// bookkept under (schema 2). Within storage trust: whoever holds the
    /// enrollment secret is inside the trust domain, so the id is
    /// correlation, not authentication (design §Security: S10 adds no new
    /// trust class). Empty = the client wants no delegations.
    pub client_id: String,
    /// The era the client believes the owner is in; `0` = "unknown, tell
    /// me" (the first frame of a session).
    pub owner_term: u64,
    /// Executed in ORDER on the owner: in-batch causality is submission
    /// order.
    pub ops: Vec<MetaOp>,
}

/// The batch's results, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaReplyFrame {
    pub schema: u32,
    /// The owner's era at execution time — how a client (re)learns it.
    pub owner_term: u64,
    pub results: Vec<MetaOpResult>,
}

/// A grace-window reclaim: the objects this client held before the
/// failover (spec §6.7 "Recovery" — the NFSv4-style re-assertion).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimFrame {
    pub schema: u32,
    pub client_epoch: u64,
    pub inos: Vec<u64>,
}

/// The reclaim's answer: fresh-era grants for the objects the owner
/// admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimReplyFrame {
    pub schema: u32,
    pub owner_term: u64,
    pub grants: Vec<TokenGrant>,
}

/// The holder's recall-channel round (the **DelegRecall** verb's request):
/// one standing call per (holder, owner). Carries the acks for the frames
/// the holder finished draining — so ack latency after delivery is one
/// round trip, and the owner's `ack_frame` correlation fires before this
/// round parks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegPollFrame {
    pub schema: u32,
    pub client_epoch: u64,
    /// The holder's KD-MW-2 identity (must match the grants' bookkeeping).
    pub client_id: String,
    /// Frame ids (rung-11 `RecallFrame::frame_id`) the holder has fully
    /// drained: every named object's local serves completed and the
    /// entries dropped BEFORE the ack was queued (the never-serve-after-
    /// ack law).
    pub acks: Vec<u64>,
}

/// One batched recall as it crosses the wire — the rung-11
/// `RecallFrame`'s payload half (`client` is the channel's own identity,
/// so it does not travel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRecallFrame {
    /// Ack correlation id (rung-11 lane).
    pub frame_id: u64,
    /// The recalled objects (≤ the lane's derived `batch_max`).
    pub inos: Vec<u64>,
}

/// The recall channel's answer: pending recalls (possibly none — a park
/// bound elapsed), plus the numbers the holder's own validity arithmetic
/// consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegPollReply {
    pub schema: u32,
    /// The owner's era at reply time (the same relearn every reply
    /// carries).
    pub owner_term: u64,
    /// The recalls pending for THIS holder.
    pub frames: Vec<WireRecallFrame>,
    /// The delegation-sequence fence at frame-build time: any in-flight
    /// grant on a recalled ino with `seq <= fence_seq` is dead on arrival
    /// (see [`DelegGrant::seq`]).
    pub fence_seq: u64,
    /// The owner's park bound for this channel, ms — the holder's
    /// channel-freshness input (serves suspend when the last completed
    /// round ages past the derived window).
    pub park_ms: u64,
    /// The owner's live recall deadline, ms (published so the two ends
    /// cannot disagree about the arithmetic in force — the
    /// `free_grace_bound` publish-the-derivation pattern).
    pub deadline_ms: u64,
}

/// Grace re-assertion (the **DelegReassert** verb's request): the
/// delegations this holder held before the failover/disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegReassertFrame {
    pub schema: u32,
    pub client_epoch: u64,
    pub client_id: String,
    /// The held objects. Bounded by the CONTROL frame cap like everything
    /// else on this wire.
    pub inos: Vec<u64>,
}

/// The re-assertion's answer: fresh-era grants for the objects the
/// successor re-admitted (fresh stamps — the predecessor may have applied
/// mutations the holder never saw, and the stamp check makes the holder's
/// serves wait for its view to catch up). An ino absent from `grants` is
/// GONE (the valve refused it, the authority moved, or the object died).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegReassertReply {
    pub schema: u32,
    pub owner_term: u64,
    pub grants: Vec<DelegGrant>,
    /// The fence at re-admission (same law as the poll reply's).
    pub fence_seq: u64,
}

/// Decode-side allocation bound: a metadata batch is a CONTROL-class
/// frame, so the transport already refuses anything larger — this is the
/// second bound, inside the body, so a lying in-body length cannot make
/// the decoder allocate either.
fn decode_limit() -> u64 {
    u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES)
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(value)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("S8 {what} encode failed: {e}")))?;
    if body.len() as u64 > decode_limit() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S8 {what} of {} B exceeds the cluster wire's CONTROL class cap ({} B) — batch \
             smaller (SQUEEZEFS_META_SHIP_BATCH_MAX)",
            body.len(),
            decode_limit()
        )));
    }
    Ok(body)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    bincode::DefaultOptions::new()
        .with_limit(decode_limit())
        .deserialize(bytes)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "S8 {what}: undecodable frame body ({} B): {e}",
                bytes.len()
            ))
        })
}

/// Encode a request frame (trusted input — we built it).
pub fn encode_request(frame: &MetaRequestFrame) -> Result<Vec<u8>> {
    encode(frame, "request")
}

/// Decode a request frame (**untrusted** — bounded).
pub fn decode_request(bytes: &[u8]) -> Result<MetaRequestFrame> {
    decode(bytes, "request")
}

/// Encode a reply frame.
pub fn encode_reply(frame: &MetaReplyFrame) -> Result<Vec<u8>> {
    encode(frame, "reply")
}

/// Decode a reply frame (**untrusted** — bounded).
pub fn decode_reply(bytes: &[u8]) -> Result<MetaReplyFrame> {
    decode(bytes, "reply")
}

/// Encode a reclaim frame.
pub fn encode_reclaim(frame: &ReclaimFrame) -> Result<Vec<u8>> {
    encode(frame, "reclaim")
}

/// Decode a reclaim frame (**untrusted** — bounded).
pub fn decode_reclaim(bytes: &[u8]) -> Result<ReclaimFrame> {
    decode(bytes, "reclaim")
}

/// Encode a reclaim reply.
pub fn encode_reclaim_reply(frame: &ReclaimReplyFrame) -> Result<Vec<u8>> {
    encode(frame, "reclaim reply")
}

/// Decode a reclaim reply (**untrusted** — bounded).
pub fn decode_reclaim_reply(bytes: &[u8]) -> Result<ReclaimReplyFrame> {
    decode(bytes, "reclaim reply")
}

/// Encode a recall-channel round.
pub fn encode_deleg_poll(frame: &DelegPollFrame) -> Result<Vec<u8>> {
    encode(frame, "deleg poll")
}

/// Decode a recall-channel round (**untrusted** — bounded).
pub fn decode_deleg_poll(bytes: &[u8]) -> Result<DelegPollFrame> {
    decode(bytes, "deleg poll")
}

/// Encode a recall-channel reply.
pub fn encode_deleg_poll_reply(frame: &DelegPollReply) -> Result<Vec<u8>> {
    encode(frame, "deleg poll reply")
}

/// Decode a recall-channel reply (**untrusted** — bounded).
pub fn decode_deleg_poll_reply(bytes: &[u8]) -> Result<DelegPollReply> {
    decode(bytes, "deleg poll reply")
}

/// Encode a re-assertion.
pub fn encode_deleg_reassert(frame: &DelegReassertFrame) -> Result<Vec<u8>> {
    encode(frame, "deleg reassert")
}

/// Decode a re-assertion (**untrusted** — bounded).
pub fn decode_deleg_reassert(bytes: &[u8]) -> Result<DelegReassertFrame> {
    decode(bytes, "deleg reassert")
}

/// Encode a re-assertion reply.
pub fn encode_deleg_reassert_reply(frame: &DelegReassertReply) -> Result<Vec<u8>> {
    encode(frame, "deleg reassert reply")
}

/// Decode a re-assertion reply (**untrusted** — bounded).
pub fn decode_deleg_reassert_reply(bytes: &[u8]) -> Result<DelegReassertReply> {
    decode(bytes, "deleg reassert reply")
}

/// Encode an intent batch.
pub fn encode_intent_batch(frame: &IntentBatchFrame) -> Result<Vec<u8>> {
    encode(frame, "intent batch")
}

/// Decode an intent batch (**untrusted** — bounded).
pub fn decode_intent_batch(bytes: &[u8]) -> Result<IntentBatchFrame> {
    decode(bytes, "intent batch")
}

/// Encode an intent batch reply.
pub fn encode_intent_batch_reply(frame: &IntentBatchReply) -> Result<Vec<u8>> {
    encode(frame, "intent batch reply")
}

/// Decode an intent batch reply (**untrusted** — bounded).
pub fn decode_intent_batch_reply(bytes: &[u8]) -> Result<IntentBatchReply> {
    decode(bytes, "intent batch reply")
}
