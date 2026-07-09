//! CoW KV metadata node layer (MetaLV v3) — pure in-memory building blocks.
//!
//! PR K1 of `docs/design-cow-kv-metadata.md`: memcmp-ordered key encodings
//! for the three logical trees, record framing with the `Put`/`Delta`/`Delete`
//! kinds and the **single fold algebra** shared byte-identically by point
//! lookup, bset n-way merge, compaction, and journal replay (design §4.2),
//! xxh3-checksummed bset build/verify/binary-search/merge (§4.3), the seeded
//! dentry/xattr hash + `coll_seq` collision scheme, and the readdir cookie
//! contract (§5.1).
//!
//! PR K2 adds [`node`]: the on-disk CoW btree node format — header page,
//! bset-frame appends, the §4.5 positional torn-tail classifier, and
//! compact/split as pure functions over caller-provided extents, with all
//! extent I/O through `crate::uring_fs` (io_uring-only).
//!
//! PR K3 adds the journal ring: the pure lock-free reservation/admission
//! core ([`journal_core`], loom-modeled), page/entry framing + replay scan
//! over `crate::uring_fs` ([`journal`], §4.1/§4.4), and the A/B root-ledger
//! records ([`checkpoint`], §4.1 — records only; scheduling is PR K6b).
//!
//! The record/bset layer is pure and in-memory; nothing here is mount-wired
//! yet. Per the design's liveness convention, K1–K5 code is
//! production-unreachable until PR K6a wires the mount path; it is kept
//! alive by its own unit/integration tests, the crash harness, the loom
//! models, and the `meta_lv_bench` criterion micro-benches.

pub mod bset;
pub mod checkpoint;
pub mod journal;
pub mod journal_core;
pub mod node;
pub mod record;

use std::sync::atomic::AtomicU64;

/// Dentry hash-collision chain exhaustion events (design §4.2): a 257th
/// same-`(parent, hash54)` name found every `coll_seq` occupied, so the
/// insert was rejected with [`KvError::DentryChainOverflow`]. Surfaced on the
/// stats inode as `meta_kv_dentry_collision_overflows` when K6a wires the
/// mount path; until then it is read by this module's tests.
pub static META_KV_DENTRY_COLLISION_OVERFLOWS: AtomicU64 = AtomicU64::new(0);

/// `Delta`-without-base folds (design §4.2): delta records discarded because
/// the scan exhausted all sources without finding an underlying `Put` or a
/// shadowing `Delete` — a counted no-op reachable only through replay
/// orderings. Surfaced as `meta_kv_delta_orphans` when K6a wires the mount
/// path; until then it is read by this module's tests.
pub static META_KV_DELTA_ORPHANS: AtomicU64 = AtomicU64::new(0);

/// Torn/garbage tail bsets dropped by the §4.5 positional node-load
/// classifier — the expected un-checkpointed-tail artifact (every truncated
/// record has seq > the durable tail; replay re-supplies it). Nonzero after
/// a **clean** unmount is the corruption alert, mirroring
/// `meta_kv_replay_dropped_torn` (design §10). Surfaced on the stats inode
/// as `meta_kv_node_dropped_tail_bsets` when K6a wires the mount path; until
/// then it is read by the node-layer tests and the crash harness.
pub static META_KV_NODE_DROPPED_TAIL_BSETS: AtomicU64 = AtomicU64::new(0);

/// Errors from the pure KV encoding / fold layer.
///
/// Kept separate from [`crate::error::SqueezefsError`] so the contracts stay
/// typed for the K2+ layers (node, journal, backend) that map them onto the
/// crate error surface.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// A 257th same-hash name: every `coll_seq` 0..=255 in the dentry chain
    /// is occupied (design §4.2). A clean insert rejection — never a panic —
    /// counted in [`META_KV_DENTRY_COLLISION_OVERFLOWS`].
    #[error("dentry hash-collision chain full (coll_seq 0..=255 all occupied); insert rejected")]
    DentryChainOverflow,

    /// A bset's stored xxh3 checksum does not match the bytes (design §4.3).
    #[error("bset checksum mismatch: stored {stored:#018x}, computed {computed:#018x}")]
    ChecksumMismatch { stored: u64, computed: u64 },

    /// Structurally invalid or truncated encoding: bad magic/version, a
    /// length field exceeding its container (design §9 bounds rule), record
    /// ordering violations, unknown record kinds or delta mask bits.
    #[error("corrupt KV encoding: {0}")]
    Corrupt(String),

    /// A readdir cookie whose biased payload exceeds the 62-bit dentry key
    /// suffix space — this filesystem never issued it (design §5.1).
    #[error("invalid readdir cookie {0:#x}: payload exceeds the 62-bit key-suffix space")]
    InvalidReaddirCookie(u64),

    /// A dentry or xattr name longer than the 255-byte `name_len: u8` record
    /// limit (design §4.2 value layouts).
    #[error("name length {len} exceeds the 255-byte record limit")]
    NameTooLong { len: usize },

    /// A record value larger than the per-volume cap
    /// `min(65,536, node_size/4)` (design §4.2) — enforced at the node layer
    /// on every write path (PR K2). A clean op rejection, never a panic.
    #[error(
        "record value length {len} exceeds the per-volume cap {cap} (min(65536, node_size/4))"
    )]
    ValueTooLarge { len: usize, cap: usize },

    /// A bset append that does not fit the node's unwritten tail — the
    /// signal for the caller (the K5/K6b writeback task) to compact
    /// (design §4.6 pt 1: "on-disk log area full ⇒ compact").
    #[error("node log area full: append needs {needed} bytes, tail has {available}")]
    NodeFull { needed: usize, available: usize },

    /// The §4.5 positional torn-tail classifier's **loud** outcome: a valid
    /// same-incarnation bset beyond the tear whose `journal_seq_horizon` is
    /// ≤ the durable journal tail. Checkpoint-covered data was barriered
    /// before its ledger record (§4.6 ordering), so a durable-required bset
    /// cannot legitimately follow a torn one — this is real corruption, not
    /// a power-loss artifact, and the node must fail loud.
    #[error(
        "corrupt node at {node_addr:#x}: checkpoint-covered bset at node offset {bset_offset} \
         (journal_seq_horizon {horizon} ≤ durable tail {durable_tail}) follows a torn bset"
    )]
    CheckpointCoveredBsetAfterTear {
        node_addr: u64,
        bset_offset: usize,
        horizon: u64,
        durable_tail: u64,
    },

    /// Node extent I/O failed (the `crate::uring_fs` paths — io_uring-only
    /// per AGENTS.md; a poisoned/torn device surfaces here as `EIO`).
    #[error("node extent I/O failed: {0}")]
    Io(#[from] crate::error::SqueezefsError),
}
