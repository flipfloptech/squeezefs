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
pub const META_SHIP_SCHEMA: u32 = 1;

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
            | MetaVerb::Listxattr => false,
            MetaVerb::CreateWithRdev
            | MetaVerb::Unlink
            | MetaVerb::Link
            | MetaVerb::Rename
            | MetaVerb::Setattr
            | MetaVerb::Setxattr
            | MetaVerb::Removexattr
            | MetaVerb::DestroyInode => true,
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
        }
    }

    /// Does this call mutate durable metadata?
    pub fn mutating(&self) -> bool {
        self.verb().mutating()
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
}

/// A batch of calls against ONE owner: the **pipelining unit**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaRequestFrame {
    pub schema: u32,
    /// The client's incarnation — half of the dedup key, and what makes a
    /// restarted client's reused ids un-confusable with its old ones.
    pub client_epoch: u64,
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
