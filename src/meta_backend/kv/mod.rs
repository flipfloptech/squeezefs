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
//! Everything in this module is pure and in-memory: no I/O, no mount wiring.
//! Per the design's liveness convention, K1–K5 code is production-unreachable
//! until PR K6a wires the mount path; it is kept alive by its own unit tests
//! and the `meta_lv_bench` criterion micro-benches.

pub mod bset;
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
}
