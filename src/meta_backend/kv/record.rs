//! Key/value encodings per tree and the `Put`/`Delta`/`Delete` fold algebra
//! (design §4.2), plus the seeded dentry/xattr hashes, the `coll_seq`
//! collision scheme, and the readdir cookie contract (§5.1).
//!
//! Keys are **big-endian composites** so raw byte order equals logical order:
//! bset binary search and node routing compare keys with `memcmp` and never
//! decode. Values are little-endian (no ordering requirement) and, for inode
//! records, carry a leading varint version tag for future extension.

use super::KvError;
use std::borrow::Cow;

// ---------------------------------------------------------------------------
// Tree ids (design §4.2 table) — on-disk format constants.
// ---------------------------------------------------------------------------

/// Inode tree: key = `ino: u64` (BE), value = versioned packed inode record.
pub const TREE_INODES: u8 = 1;
/// Dentry tree: key = `(parent_ino, name_hash54, coll_seq)`.
pub const TREE_DENTRIES: u8 = 2;
/// Xattr tree: key = `(ino, name_hash56, coll_seq)`.
pub const TREE_XATTRS: u8 = 3;
/// Reserved: future refcounted-extent tree (snapshots, design §4.11).
pub const TREE_ALLOC_RESERVED: u8 = 4;
/// Reserved: future data-block backpointer tree (roadmap step 2 remainder).
pub const TREE_BACKPTR_RESERVED: u8 = 5;

// ---------------------------------------------------------------------------
// Key builders — memcmp-ordered big-endian composites (design §4.2).
// ---------------------------------------------------------------------------

/// Inode tree key length: `ino: u64`.
pub const INODE_KEY_LEN: usize = 8;
/// Dentry tree key length: `parent_ino: u64` + packed `(hash54, coll_seq)`.
pub const DENTRY_KEY_LEN: usize = 16;
/// Xattr tree key length: `ino: u64` + packed `(hash56, coll_seq)`.
pub const XATTR_KEY_LEN: usize = 16;

/// Largest 54-bit dentry name hash (54 bits chosen for readdir cookie
/// headroom — design §4.2 / §5.1).
pub const HASH54_MAX: u64 = (1 << 54) - 1;
/// Largest 56-bit xattr name hash (no cookie constraint — design §4.2).
pub const HASH56_MAX: u64 = (1 << 56) - 1;

/// Build the inode-tree key for `ino` (big-endian, memcmp-ordered).
pub fn inode_key(_ino: u64) -> [u8; INODE_KEY_LEN] {
    todo!()
}

/// Decode an inode-tree key; rejects wrong-length input.
pub fn decode_inode_key(_key: &[u8]) -> Result<u64, KvError> {
    todo!()
}

/// The packed 62-bit dentry key suffix `(hash54 << 8) | coll_seq` — the same
/// value the readdir cookie biases by 3 (design §5.1 resume rule), which is
/// what makes cookies stable: they *are* the key.
pub fn dentry_key_suffix(_hash54: u64, _coll_seq: u8) -> u64 {
    todo!()
}

/// Build the dentry-tree key `(parent_ino, hash54, coll_seq)` — parent is the
/// primary dimension, the packed suffix secondary, both big-endian.
pub fn dentry_key(_parent_ino: u64, _hash54: u64, _coll_seq: u8) -> [u8; DENTRY_KEY_LEN] {
    todo!()
}

/// Decode a dentry-tree key into `(parent_ino, hash54, coll_seq)`; rejects
/// wrong-length input and suffixes with the two structurally-zero top bits
/// set (never produced by [`dentry_key`]).
pub fn decode_dentry_key(_key: &[u8]) -> Result<(u64, u64, u8), KvError> {
    todo!()
}

/// Build the xattr-tree key `(ino, hash56, coll_seq)`, big-endian.
pub fn xattr_key(_ino: u64, _hash56: u64, _coll_seq: u8) -> [u8; XATTR_KEY_LEN] {
    todo!()
}

/// Decode an xattr-tree key into `(ino, hash56, coll_seq)`.
pub fn decode_xattr_key(_key: &[u8]) -> Result<(u64, u64, u8), KvError> {
    todo!()
}

// ---------------------------------------------------------------------------
// Seeded name hashes (design §4.2): per-volume secret `hash_seed` so
// filenames cannot be chosen off-line to collide (bcachefs seeded-str_hash
// precedent).
// ---------------------------------------------------------------------------

/// 54-bit seeded dentry name hash: `xxh3_64_with_seed(name, hash_seed) >> 10`.
pub fn dentry_name_hash54(_name: &[u8], _hash_seed: u64) -> u64 {
    todo!()
}

/// 56-bit seeded xattr name hash: `xxh3_64_with_seed(name, hash_seed) >> 8`.
pub fn xattr_name_hash56(_name: &[u8], _hash_seed: u64) -> u64 {
    todo!()
}

// ---------------------------------------------------------------------------
// coll_seq collision scheme (design §4.2): same-hash names probe coll_seq
// 0..=255 comparing full names; a 257th same-hash name fails clean.
// ---------------------------------------------------------------------------

/// Lowest `coll_seq` not present in `occupied` (the chain's existing
/// same-hash entries, any order), or `None` when all 256 slots are taken.
/// Bounded by construction — the u8 domain is the probe space.
pub fn first_free_coll_seq<I: IntoIterator<Item = u8>>(_occupied: I) -> Option<u8> {
    todo!()
}

/// Assign the `coll_seq` for inserting a new same-hash dentry. Callers must
/// have already probed the chain by full-name comparison (an existing name
/// is a replace, not a new slot). Chain exhaustion — a 257th same-hash name —
/// returns [`KvError::DentryChainOverflow`] and bumps
/// [`super::META_KV_DENTRY_COLLISION_OVERFLOWS`]; it never panics.
pub fn assign_dentry_coll_seq<I: IntoIterator<Item = u8>>(_occupied: I) -> Result<u8, KvError> {
    todo!()
}

// ---------------------------------------------------------------------------
// Inode record value (design §4.2 table): the DiskInode fields minus the pad,
// plus flags2; leading varint version tag for future extension.
// ---------------------------------------------------------------------------

/// `flags2` bit 0: migrated quarantined ino whose v2 xattr block was presumed
/// corrupt and not read (design §6.2).
pub const FLAGS2_QUARANTINE_CONTENT_LOST: u32 = 1;

/// Current inode value encoding version (the leading varint tag).
pub const INODE_VALUE_VERSION: u64 = 1;

/// Packed inode record value for `TREE_INODES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InodeValue {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub flags: u32,
    pub flags2: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl InodeValue {
    /// Encode as `varint(version=1)` + fixed little-endian fields.
    pub fn encode(&self) -> Vec<u8> {
        todo!()
    }

    /// Decode; rejects unknown versions, truncation, and trailing bytes.
    pub fn decode(_bytes: &[u8]) -> Result<Self, KvError> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Dentry / xattr record values (design §4.2 table).
// ---------------------------------------------------------------------------

/// Dentry record value: `{child_ino: u64, file_type: u8, name_len: u8, name}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DentryValue {
    pub child_ino: u64,
    /// Directory-entry type byte (`DT_*`).
    pub file_type: u8,
    /// The full name — collision chains resolve by comparing it (§4.2).
    pub name: Vec<u8>,
}

impl DentryValue {
    /// Encode; names longer than 255 bytes are a clean [`KvError::NameTooLong`].
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        todo!()
    }

    /// Decode; rejects truncation, lying `name_len`, and trailing bytes.
    pub fn decode(_bytes: &[u8]) -> Result<Self, KvError> {
        todo!()
    }
}

/// Xattr record value: `{name_len: u8, name, value}` (value = remainder; the
/// record framing carries the total length). The per-volume value cap
/// (`min(65,536, node_size/4)`, §4.2) is enforced by the node layer in K2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrValue {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl XattrValue {
    /// Encode; names longer than 255 bytes are a clean [`KvError::NameTooLong`].
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        todo!()
    }

    /// Decode; rejects truncation and lying `name_len`.
    pub fn decode(_bytes: &[u8]) -> Result<Self, KvError> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Inode delta records (design §4.4 pt 6): partial updates folding into the
// newest base Put. v1's only user is the Δtime record.
// ---------------------------------------------------------------------------

pub const DELTA_MODE: u16 = 1 << 0;
pub const DELTA_UID: u16 = 1 << 1;
pub const DELTA_GID: u16 = 1 << 2;
pub const DELTA_NLINK: u16 = 1 << 3;
pub const DELTA_FLAGS: u16 = 1 << 4;
pub const DELTA_FLAGS2: u16 = 1 << 5;
pub const DELTA_SIZE: u16 = 1 << 6;
pub const DELTA_ATIME: u16 = 1 << 7;
pub const DELTA_MTIME: u16 = 1 << 8;
pub const DELTA_CTIME: u16 = 1 << 9;
/// All defined mask bits; anything else is a decode error.
pub const DELTA_MASK_ALL: u16 = 0x3FF;
/// The Δtime mask: mtime + ctime only, never value fields (design §4.4 pt 6).
pub const DELTA_TIMES: u16 = DELTA_MTIME | DELTA_CTIME;

/// A field-masked partial inode update. Unmasked `fields` entries are
/// ignored by [`InodeDelta::apply`] and never encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeDelta {
    pub mask: u16,
    pub fields: InodeValue,
}

impl InodeDelta {
    /// The Δtime record: overwrite `mtime`/`ctime` only, so concurrent
    /// shared-parent-lock creates never clobber value fields (§4.4 pt 6).
    pub fn times(_mtime: u64, _ctime: u64) -> Self {
        todo!()
    }

    /// Encode as `mask: u16 LE` + the masked fields in canonical field order.
    pub fn encode(&self) -> Vec<u8> {
        todo!()
    }

    /// Decode; rejects unknown mask bits, truncation, and trailing bytes.
    pub fn decode(_bytes: &[u8]) -> Result<Self, KvError> {
        todo!()
    }

    /// Overwrite the masked fields of `base` (field overwrite — idempotent,
    /// so replaying a delta over an already-folded base is a no-op).
    pub fn apply(&self, _base: &mut InodeValue) {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Record kinds & framing (design §4.2 "Record kinds & merge rules").
// ---------------------------------------------------------------------------

/// Record kinds. Encodings start at 1 so a zeroed byte is never a valid kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// Full value.
    Put = 1,
    /// Partial update folding into the newest base `Put` (§4.4 pt 6).
    Delta = 2,
    /// Per-key tombstone.
    Delete = 3,
}

/// Fixed framing-header length: `key_len: u16 | kind: u8 | seq: u64 |
/// val_len: u32`, all little-endian, followed by key then value bytes.
pub const RECORD_HEADER_LEN: usize = 15;

/// An owned record — the staging/build-side twin of [`RecordRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: Vec<u8>,
    pub seq: u64,
    pub kind: RecordKind,
    /// Empty for `Delete`; the delta payload for `Delta`.
    pub value: Vec<u8>,
}

impl Record {
    pub fn put(_key: Vec<u8>, _seq: u64, _value: Vec<u8>) -> Self {
        todo!()
    }

    pub fn delta(_key: Vec<u8>, _seq: u64, _delta: &InodeDelta) -> Self {
        todo!()
    }

    pub fn delete(_key: Vec<u8>, _seq: u64) -> Self {
        todo!()
    }

    /// Borrow as the zero-copy view the fold and bset layers consume.
    pub fn record_ref(&self) -> RecordRef<'_> {
        todo!()
    }
}

/// A borrowed, zero-copy record view into a bset (or an owned [`Record`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordRef<'a> {
    pub key: &'a [u8],
    pub seq: u64,
    pub kind: RecordKind,
    pub value: &'a [u8],
}

impl<'a> RecordRef<'a> {
    /// Exact encoded size (header + key + value).
    pub fn encoded_len(&self) -> usize {
        todo!()
    }

    /// Append the framing to `out`.
    pub fn encode_into(&self, _out: &mut Vec<u8>) {
        todo!()
    }

    /// Decode one record from the front of `buf`, returning the view and the
    /// bytes consumed. Every length field is bounds-checked against the
    /// container before use (design §9); tombstones must carry no value;
    /// unknown kind bytes and empty keys are rejected.
    pub fn decode(_buf: &'a [u8]) -> Result<(Self, usize), KvError> {
        todo!()
    }

    /// Copy into an owned [`Record`].
    pub fn to_record(&self) -> Record {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// THE fold algebra (design §4.2) — shared byte-identically by point lookup,
// bset n-way merge, compaction, and journal replay. "Replay reproduces RAM"
// is a single theorem about this one function.
// ---------------------------------------------------------------------------

/// Outcome of folding one key's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Folded<'a> {
    /// The key resolves to a live value; `seq` is the newest contributing
    /// record's seq (so a folded `Put` shadows everything it folded away).
    /// Borrowed (zero-copy) when no deltas had to be applied.
    Put { value: Cow<'a, [u8]>, seq: u64 },
    /// The key is absent because of a tombstone with this seq — compaction
    /// needs the seq for the elision rule (§4.2).
    Tombstone { seq: u64 },
    /// No records, or only orphaned deltas (counted no-op, §4.2).
    Absent,
}

impl Folded<'_> {
    /// The user-visible projection (§4.2 digest-walk framing): live value
    /// bytes, or `None` for tombstones/absent keys alike.
    pub fn live_value(&self) -> Option<&[u8]> {
        todo!()
    }
}

/// Fold one key's records, presented **newest-seq-first** (ties across
/// sources keep input order and are legal only for identical-effect records,
/// e.g. a node bset overlapping the journal replay window).
///
/// Scan collecting `Delta`s until the first `Put` or `Delete`:
/// - `Delete` ⇒ [`Folded::Tombstone`] (collected deltas discarded — the
///   tombstone is the underlying truth, not an orphan case);
/// - `Put` ⇒ apply collected deltas onto it in **ascending seq order**;
/// - exhausted with deltas pending ⇒ [`Folded::Absent`] and each orphaned
///   delta record is counted in [`super::META_KV_DELTA_ORPHANS`] (§4.2).
///
/// Values are opaque to the fold unless deltas force a base decode.
pub fn fold_newest_first<'a, I>(_records: I) -> Result<Folded<'a>, KvError>
where
    I: IntoIterator<Item = RecordRef<'a>>,
{
    todo!()
}

/// Compaction fold (§4.2): identical algebra; the output contains one folded
/// `Put` (or nothing) per key. **Tombstone elision rule**: a `Delete` may be
/// dropped only if its seq is **strictly below** `durable_tail` (the durable
/// checkpoint tail at compaction time) — a tombstone still inside the replay
/// window must survive, or replay could resurrect the key from an older
/// journal `Put` it was shadowing.
///
/// `group_newest_first` must hold one key's records, newest-seq-first.
pub fn compact_fold(
    _group_newest_first: &[RecordRef<'_>],
    _durable_tail: u64,
) -> Result<Option<Record>, KvError> {
    todo!()
}

// ---------------------------------------------------------------------------
// Readdir cookie contract (design §5.1).
// ---------------------------------------------------------------------------

/// Synthetic `.` is emitted with offset 1.
pub const READDIR_OFFSET_DOT: u64 = 1;
/// Synthetic `..` is emitted with offset 2.
pub const READDIR_OFFSET_DOTDOT: u64 = 2;
/// Real entries are biased past the reserved offsets 0/1/2.
pub const READDIR_COOKIE_BIAS: u64 = 3;

/// Decoded readdir resume position (design §5.1 resume rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaddirPos {
    /// Offset 0: emit `.`, `..`, then every entry.
    Start,
    /// Offset 1: emit `..`, then every entry.
    AfterDot,
    /// Offset 2: every entry.
    AfterDotDot,
    /// Offset ≥ 3: entries whose key suffix is **strictly greater** than
    /// `(hash54, coll_seq)`.
    AfterEntry { hash54: u64, coll_seq: u8 },
}

/// Encode a real entry's readdir cookie: `3 + ((hash54 << 8) | coll_seq)`.
/// The payload is ≤ 62 bits, so the bias can never overflow and the sign bit
/// is always clear when the vendored fuse3 surfaces the offset as `i64` —
/// the classic FUSE readdir bug class this encoding forecloses (§5.1).
pub fn encode_readdir_cookie(_hash54: u64, _coll_seq: u8) -> u64 {
    todo!()
}

/// Decode a readdir offset. Cookies whose biased payload exceeds the 62-bit
/// key-suffix space were never issued by this filesystem and are rejected
/// with [`KvError::InvalidReaddirCookie`].
pub fn decode_readdir_cookie(_cookie: u64) -> Result<ReaddirPos, KvError> {
    todo!()
}

// ---------------------------------------------------------------------------
// Tests — these pin the §4.2 / §5.1 contracts (PR K1, tests-first).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta_backend::kv::{META_KV_DELTA_ORPHANS, META_KV_DENTRY_COLLISION_OVERFLOWS};
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;

    /// Deterministic, field-distinct inode value for a test seed.
    fn iv(seed: u64) -> InodeValue {
        InodeValue {
            mode: 0o100644 ^ (seed as u32),
            uid: 1000 + seed as u32,
            gid: 2000 + (seed >> 8) as u32,
            nlink: 1 + (seed as u32 & 3),
            flags: (seed as u32).rotate_left(7),
            flags2: 0,
            size: seed.wrapping_mul(4096),
            atime: seed.wrapping_add(1),
            mtime: seed.wrapping_add(2),
            ctime: seed.wrapping_add(3),
        }
    }

    /// Fold-test key: one inode-tree key shared by a record group.
    fn k() -> Vec<u8> {
        inode_key(77).to_vec()
    }

    fn put(seq: u64, v: &InodeValue) -> Record {
        Record::put(k(), seq, v.encode())
    }

    fn dtimes(seq: u64, mtime: u64, ctime: u64) -> Record {
        Record::delta(k(), seq, &InodeDelta::times(mtime, ctime))
    }

    fn del(seq: u64) -> Record {
        Record::delete(k(), seq)
    }

    /// Fold a newest-first slice of owned records.
    fn fold(recs: &[Record]) -> Result<Folded<'_>, KvError> {
        fold_newest_first(recs.iter().map(|r| r.record_ref()))
    }

    // -- key encodings (§4.2) ------------------------------------------------

    #[test]
    fn tree_ids_pin_the_on_disk_format() {
        assert_eq!(TREE_INODES, 1);
        assert_eq!(TREE_DENTRIES, 2);
        assert_eq!(TREE_XATTRS, 3);
        assert_eq!(TREE_ALLOC_RESERVED, 4);
        assert_eq!(TREE_BACKPTR_RESERVED, 5);
    }

    #[test]
    fn inode_key_is_big_endian_and_memcmp_ordered() {
        assert_eq!(inode_key(1), [0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            decode_inode_key(&inode_key(0xDEAD_BEEF_F00D)).expect("roundtrip"),
            0xDEAD_BEEF_F00D
        );
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        for _ in 0..1000 {
            let (a, b): (u64, u64) = (rng.gen(), rng.gen());
            assert_eq!(
                inode_key(a).cmp(&inode_key(b)),
                a.cmp(&b),
                "byte order must equal logical order for {a} vs {b}"
            );
        }
        assert!(matches!(
            decode_inode_key(&[0u8; 7]),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            decode_inode_key(&[0u8; 9]),
            Err(KvError::Corrupt(_))
        ));
    }

    #[test]
    fn dentry_key_orders_parent_then_hash_then_coll_seq() {
        let key = dentry_key(2, 0x2A5, 7);
        assert_eq!(&key[..8], &2u64.to_be_bytes());
        assert_eq!(&key[8..], &dentry_key_suffix(0x2A5, 7).to_be_bytes());
        assert_eq!(dentry_key_suffix(0x2A5, 7), (0x2A5 << 8) | 7);
        // The suffix's top two bits are structurally clear (54 + 8 = 62 bits).
        assert_eq!(dentry_key_suffix(HASH54_MAX, 0xFF) >> 62, 0);
        assert_eq!(
            decode_dentry_key(&key).expect("roundtrip"),
            (2, 0x2A5, 7),
            "decode must invert the builder"
        );

        let mut rng = ChaCha8Rng::seed_from_u64(2);
        for _ in 0..1000 {
            let t1 = (
                rng.gen::<u64>(),
                rng.gen_range(0..=HASH54_MAX),
                rng.gen::<u8>(),
            );
            let t2 = (
                rng.gen::<u64>(),
                rng.gen_range(0..=HASH54_MAX),
                rng.gen::<u8>(),
            );
            assert_eq!(
                dentry_key(t1.0, t1.1, t1.2).cmp(&dentry_key(t2.0, t2.1, t2.2)),
                t1.cmp(&t2),
                "memcmp order must equal (parent, hash54, coll_seq) order: {t1:?} vs {t2:?}"
            );
        }

        assert!(matches!(
            decode_dentry_key(&key[..15]),
            Err(KvError::Corrupt(_))
        ));
        // A suffix with the structurally-zero top bits set was never built by us.
        let mut bad = key;
        bad[8] |= 0x80;
        assert!(matches!(decode_dentry_key(&bad), Err(KvError::Corrupt(_))));
    }

    #[test]
    fn xattr_key_orders_ino_then_hash_then_coll_seq() {
        let key = xattr_key(9, 0xBEEF, 3);
        assert_eq!(&key[..8], &9u64.to_be_bytes());
        assert_eq!(&key[8..], &((0xBEEF_u64 << 8) | 3).to_be_bytes());
        assert_eq!(decode_xattr_key(&key).expect("roundtrip"), (9, 0xBEEF, 3));

        let mut rng = ChaCha8Rng::seed_from_u64(3);
        for _ in 0..1000 {
            let t1 = (
                rng.gen::<u64>(),
                rng.gen_range(0..=HASH56_MAX),
                rng.gen::<u8>(),
            );
            let t2 = (
                rng.gen::<u64>(),
                rng.gen_range(0..=HASH56_MAX),
                rng.gen::<u8>(),
            );
            assert_eq!(
                xattr_key(t1.0, t1.1, t1.2).cmp(&xattr_key(t2.0, t2.1, t2.2)),
                t1.cmp(&t2),
                "memcmp order must equal (ino, hash56, coll_seq) order: {t1:?} vs {t2:?}"
            );
        }
        assert!(matches!(
            decode_xattr_key(&key[..12]),
            Err(KvError::Corrupt(_))
        ));
    }

    // -- seeded hashes (§4.2) ------------------------------------------------

    #[test]
    fn dentry_hash54_is_the_seeded_xxh3_top_54_bits() {
        use xxhash_rust::xxh3::xxh3_64_with_seed;
        // The exact formula is an on-disk format contract: seeded xxh3 >> 10.
        assert_eq!(
            dentry_name_hash54(b"hello", 42),
            xxh3_64_with_seed(b"hello", 42) >> 10
        );
        assert_eq!(
            dentry_name_hash54(b"hello", 42),
            dentry_name_hash54(b"hello", 42)
        );
        assert_ne!(
            dentry_name_hash54(b"hello", 1),
            dentry_name_hash54(b"hello", 2),
            "the per-volume seed must key the hash (anti hash-flooding, §9)"
        );
        for i in 0..4096u32 {
            let h = dentry_name_hash54(format!("name-{i}").as_bytes(), 0xFEED);
            assert!(h <= HASH54_MAX, "hash54 must fit 54 bits, got {h:#x}");
        }
    }

    #[test]
    fn xattr_hash56_is_the_seeded_xxh3_top_56_bits() {
        use xxhash_rust::xxh3::xxh3_64_with_seed;
        assert_eq!(
            xattr_name_hash56(b"user.layout", 42),
            xxh3_64_with_seed(b"user.layout", 42) >> 8
        );
        assert_ne!(
            xattr_name_hash56(b"user.layout", 1),
            xattr_name_hash56(b"user.layout", 2)
        );
        for i in 0..4096u32 {
            let h = xattr_name_hash56(format!("user.attr-{i}").as_bytes(), 0xFEED);
            assert!(h <= HASH56_MAX, "hash56 must fit 56 bits, got {h:#x}");
        }
    }

    // -- coll_seq scheme (§4.2) ----------------------------------------------

    #[test]
    fn first_free_coll_seq_fills_the_lowest_hole() {
        assert_eq!(first_free_coll_seq(std::iter::empty()), Some(0));
        assert_eq!(first_free_coll_seq([0u8, 1]), Some(2));
        assert_eq!(first_free_coll_seq([0u8, 2]), Some(1), "holes are refilled");
        assert_eq!(first_free_coll_seq([5u8]), Some(0));
        assert_eq!(
            first_free_coll_seq([3u8, 1, 0, 2]),
            Some(4),
            "input order must not matter"
        );
        assert_eq!(first_free_coll_seq(0..=254u8), Some(255));
        assert_eq!(first_free_coll_seq(0..=255u8), None);
    }

    #[test]
    fn dentry_chain_overflow_at_256_is_a_clean_counted_error() {
        let before = META_KV_DENTRY_COLLISION_OVERFLOWS.load(Ordering::Relaxed);
        assert_eq!(
            assign_dentry_coll_seq(0..=254u8).expect("256th entry still fits"),
            255
        );
        assert_eq!(
            META_KV_DENTRY_COLLISION_OVERFLOWS.load(Ordering::Relaxed),
            before,
            "successful assignment must not count as an overflow"
        );
        let res = assign_dentry_coll_seq(0..=255u8);
        assert!(
            matches!(res, Err(KvError::DentryChainOverflow)),
            "the 257th same-hash name must fail clean (never panic), got {res:?}"
        );
        assert_eq!(
            META_KV_DENTRY_COLLISION_OVERFLOWS.load(Ordering::Relaxed),
            before + 1,
            "chain exhaustion must bump meta_kv_dentry_collision_overflows"
        );
    }

    // -- record values (§4.2) ------------------------------------------------

    #[test]
    fn inode_value_roundtrips_with_leading_version_varint() {
        let v = InodeValue {
            flags2: FLAGS2_QUARANTINE_CONTENT_LOST,
            ..iv(7)
        };
        let bytes = v.encode();
        assert_eq!(bytes[0], 1, "leading varint version tag must be 1");
        assert_eq!(bytes.len(), 57, "varint(1) + 6×u32 + 4×u64");
        assert_eq!(InodeValue::decode(&bytes).expect("roundtrip"), v);
        assert_eq!(
            FLAGS2_QUARANTINE_CONTENT_LOST, 1,
            "§6.2 quarantine flag is bit 0"
        );

        // Unknown future version — refused, not misparsed.
        let mut future = bytes.clone();
        future[0] = 2;
        assert!(matches!(
            InodeValue::decode(&future),
            Err(KvError::Corrupt(_))
        ));
        // Multi-byte varint version (129) — refused, not misparsed.
        let mut big = vec![0x81, 0x01];
        big.extend_from_slice(&bytes[1..]);
        assert!(matches!(InodeValue::decode(&big), Err(KvError::Corrupt(_))));
        // Truncation and trailing garbage — refused.
        assert!(matches!(
            InodeValue::decode(&bytes[..bytes.len() - 1]),
            Err(KvError::Corrupt(_))
        ));
        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            InodeValue::decode(&trailing),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(InodeValue::decode(&[]), Err(KvError::Corrupt(_))));
    }

    #[test]
    fn dentry_value_roundtrips_and_caps_name_at_255() {
        let d = DentryValue {
            child_ino: 42,
            file_type: 8, // DT_REG
            name: b"hello".to_vec(),
        };
        let bytes = d.encode().expect("encode");
        assert_eq!(bytes.len(), 8 + 1 + 1 + 5);
        assert_eq!(DentryValue::decode(&bytes).expect("roundtrip"), d);

        let max = DentryValue {
            child_ino: 1,
            file_type: 4, // DT_DIR
            name: vec![b'x'; 255],
        };
        assert_eq!(
            DentryValue::decode(&max.encode().expect("255-byte name fits")).expect("roundtrip"),
            max
        );

        let over = DentryValue {
            child_ino: 1,
            file_type: 8,
            name: vec![b'x'; 256],
        };
        assert!(matches!(
            over.encode(),
            Err(KvError::NameTooLong { len: 256 })
        ));

        // Lying name_len: shorter and longer than the actual remainder.
        let mut lying = d.encode().expect("encode");
        lying[9] = 200;
        assert!(matches!(
            DentryValue::decode(&lying),
            Err(KvError::Corrupt(_))
        ));
        lying[9] = 2;
        assert!(matches!(
            DentryValue::decode(&lying),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            DentryValue::decode(&[0u8; 9]),
            Err(KvError::Corrupt(_))
        ));
    }

    #[test]
    fn xattr_value_roundtrips_and_caps_name_at_255() {
        let x = XattrValue {
            name: b"user.layout".to_vec(),
            value: vec![0xAB; 300],
        };
        let bytes = x.encode().expect("encode");
        assert_eq!(bytes.len(), 1 + 11 + 300);
        assert_eq!(XattrValue::decode(&bytes).expect("roundtrip"), x);

        let empty_value = XattrValue {
            name: b"user.empty".to_vec(),
            value: vec![],
        };
        assert_eq!(
            XattrValue::decode(&empty_value.encode().expect("encode")).expect("roundtrip"),
            empty_value
        );

        let over = XattrValue {
            name: vec![b'n'; 256],
            value: vec![],
        };
        assert!(matches!(
            over.encode(),
            Err(KvError::NameTooLong { len: 256 })
        ));

        // Lying name_len beyond the buffer.
        let mut lying = x.encode().expect("encode");
        lying[0] = 255;
        if lying.len() < 256 {
            assert!(matches!(
                XattrValue::decode(&lying),
                Err(KvError::Corrupt(_))
            ));
        }
        assert!(matches!(XattrValue::decode(&[]), Err(KvError::Corrupt(_))));
    }

    // -- inode deltas (§4.4 pt 6) --------------------------------------------

    #[test]
    fn inode_delta_times_applies_only_time_fields() {
        let d = InodeDelta::times(111, 222);
        assert_eq!(d.mask, DELTA_TIMES);
        let bytes = d.encode();
        assert_eq!(bytes.len(), 2 + 8 + 8, "mask + two u64 time fields");
        assert_eq!(InodeDelta::decode(&bytes).expect("roundtrip"), d);

        let mut base = iv(3);
        let orig = base;
        d.apply(&mut base);
        assert_eq!(base.mtime, 111);
        assert_eq!(base.ctime, 222);
        assert_eq!(
            (
                base.mode,
                base.uid,
                base.gid,
                base.nlink,
                base.flags,
                base.flags2,
                base.size,
                base.atime
            ),
            (
                orig.mode,
                orig.uid,
                orig.gid,
                orig.nlink,
                orig.flags,
                orig.flags2,
                orig.size,
                orig.atime
            ),
            "Δtime must never carry or clobber value fields (§4.4 pt 6)"
        );
    }

    #[test]
    fn inode_delta_rejects_unknown_mask_bits_and_bad_lengths() {
        let full = InodeDelta {
            mask: DELTA_MASK_ALL,
            fields: iv(9),
        };
        let bytes = full.encode();
        assert_eq!(bytes.len(), 2 + 6 * 4 + 4 * 8);
        assert_eq!(InodeDelta::decode(&bytes).expect("roundtrip"), full);
        let mut base = InodeValue::default();
        full.apply(&mut base);
        assert_eq!(base, iv(9), "a full-mask delta overwrites every field");

        // Unknown mask bit 10.
        let mut unknown = InodeDelta::times(1, 2).encode();
        let bad_mask = (DELTA_MASK_ALL as u32 + 1) as u16;
        unknown[..2].copy_from_slice(&bad_mask.to_le_bytes());
        assert!(matches!(
            InodeDelta::decode(&unknown),
            Err(KvError::Corrupt(_))
        ));

        // Truncated fields and trailing bytes.
        let ok = InodeDelta::times(1, 2).encode();
        assert!(matches!(
            InodeDelta::decode(&ok[..ok.len() - 1]),
            Err(KvError::Corrupt(_))
        ));
        let mut trailing = ok;
        trailing.push(0);
        assert!(matches!(
            InodeDelta::decode(&trailing),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            InodeDelta::decode(&[0u8; 1]),
            Err(KvError::Corrupt(_))
        ));
    }

    // -- record framing (§4.2) -----------------------------------------------

    #[test]
    fn record_framing_roundtrips_all_kinds() {
        let records = [
            Record::put(inode_key(5).to_vec(), 9, iv(1).encode()),
            Record::delta(inode_key(5).to_vec(), 10, &InodeDelta::times(4, 5)),
            Record::delete(inode_key(5).to_vec(), 11),
        ];
        assert_eq!(records[1].kind, RecordKind::Delta);
        assert!(records[2].value.is_empty(), "tombstones carry no value");

        let mut buf = Vec::new();
        for rec in &records {
            let r = rec.record_ref();
            let before = buf.len();
            r.encode_into(&mut buf);
            assert_eq!(buf.len() - before, r.encoded_len());
        }
        // Sequential decode walks all three back out.
        let mut cursor = 0usize;
        for rec in &records {
            let (dec, used) = RecordRef::decode(&buf[cursor..]).expect("decode");
            assert_eq!(dec.to_record(), *rec);
            cursor += used;
        }
        assert_eq!(cursor, buf.len(), "no trailing bytes after the last record");
    }

    #[test]
    fn record_decode_rejects_malformed_framing() {
        let rec = Record::put(inode_key(5).to_vec(), 9, iv(1).encode());
        let mut buf = Vec::new();
        rec.record_ref().encode_into(&mut buf);

        // Truncated header / key / value.
        assert!(matches!(
            RecordRef::decode(&buf[..RECORD_HEADER_LEN - 1]),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            RecordRef::decode(&buf[..RECORD_HEADER_LEN + 3]),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            RecordRef::decode(&buf[..buf.len() - 1]),
            Err(KvError::Corrupt(_))
        ));

        // key_len lying beyond the container (§9 bounds rule).
        let mut lying = buf.clone();
        lying[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            RecordRef::decode(&lying),
            Err(KvError::Corrupt(_))
        ));

        // val_len lying beyond the container.
        let mut lying = buf.clone();
        lying[11..15].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            RecordRef::decode(&lying),
            Err(KvError::Corrupt(_))
        ));

        // Unknown kind bytes (0 and 4).
        for bad_kind in [0u8, 4] {
            let mut bad = buf.clone();
            bad[2] = bad_kind;
            assert!(matches!(RecordRef::decode(&bad), Err(KvError::Corrupt(_))));
        }

        // Empty keys are illegal.
        let mut empty_key = Vec::new();
        Record::delete(inode_key(5).to_vec(), 1)
            .record_ref()
            .encode_into(&mut empty_key);
        empty_key[0..2].copy_from_slice(&0u16.to_le_bytes());
        assert!(matches!(
            RecordRef::decode(&empty_key),
            Err(KvError::Corrupt(_))
        ));

        // A tombstone carrying a value is illegal.
        let mut del_with_value = Vec::new();
        Record::delete(inode_key(5).to_vec(), 1)
            .record_ref()
            .encode_into(&mut del_with_value);
        del_with_value[11..15].copy_from_slice(&1u32.to_le_bytes());
        del_with_value.push(0xFF);
        assert!(matches!(
            RecordRef::decode(&del_with_value),
            Err(KvError::Corrupt(_))
        ));
    }

    // -- the fold algebra (§4.2 "Record kinds & merge rules") -----------------

    #[test]
    fn fold_lww_newest_put_wins() {
        let old = put(1, &iv(1));
        let new = put(5, &iv(2));
        let folded = fold_newest_first([new.record_ref(), old.record_ref()]).expect("fold");
        match folded {
            Folded::Put { value, seq } => {
                assert_eq!(seq, 5);
                assert_eq!(value.as_ref(), iv(2).encode().as_slice());
                assert!(
                    matches!(value, Cow::Borrowed(_)),
                    "a plain Put lookup must be zero-copy (no delta ⇒ borrowed)"
                );
            }
            other => panic!("expected the newest Put to win, got {other:?}"),
        }
        assert_eq!(
            fold_newest_first(std::iter::empty()).expect("fold"),
            Folded::Absent
        );
    }

    #[test]
    fn fold_tombstone_shadows_older_records_in_both_scan_shapes() {
        let orphans_before = META_KV_DELTA_ORPHANS.load(Ordering::Relaxed);

        // Tombstone newest: shadows the older Put.
        let recs = [del(7), put(3, &iv(1))];
        let folded = fold(&recs).expect("fold");
        assert_eq!(folded, Folded::Tombstone { seq: 7 });

        // Deltas above the tombstone: collected then discarded — and NOT
        // counted as orphans (the tombstone is the underlying truth).
        let recs = [dtimes(9, 90, 99), del(7), put(3, &iv(1))];
        let folded = fold(&recs).expect("fold");
        assert_eq!(
            folded,
            Folded::Tombstone { seq: 7 },
            "the tombstone's own seq is the fold seq — safe because deltas \
             above it fold to absent as orphans on replay"
        );
        assert_eq!(
            META_KV_DELTA_ORPHANS.load(Ordering::Relaxed),
            orphans_before,
            "delete-discarded deltas are not delta orphans"
        );
    }

    #[test]
    fn fold_newer_put_revives_key_after_tombstone() {
        let recs = [put(9, &iv(4)), del(7), put(3, &iv(1))];
        let folded = fold(&recs).expect("fold");
        match folded {
            Folded::Put { value, seq } => {
                assert_eq!(seq, 9);
                assert_eq!(value.as_ref(), iv(4).encode().as_slice());
            }
            other => panic!("a Put newer than the tombstone must revive the key, got {other:?}"),
        }
    }

    #[test]
    fn fold_applies_deltas_ascending_onto_newest_put() {
        let base = iv(1);
        // Overlapping masks: ascending application means the NEWEST delta's
        // fields land last and win.
        let recs = [
            dtimes(9, 90, 99),
            dtimes(5, 50, 55),
            put(2, &base),
            put(1, &iv(0)), // shadowed; the scan stops at seq 2
        ];
        let folded = fold(&recs).expect("fold");
        match folded {
            Folded::Put { value, seq } => {
                assert_eq!(seq, 9, "the folded Put carries the newest contributing seq");
                assert!(
                    matches!(value, Cow::Owned(_)),
                    "delta application is the one merge copy"
                );
                let out = InodeValue::decode(&value).expect("folded value decodes");
                assert_eq!(
                    (out.mtime, out.ctime),
                    (90, 99),
                    "ascending seq order: newest delta wins"
                );
                assert_eq!(
                    (out.mode, out.uid, out.gid, out.nlink, out.flags, out.size, out.atime),
                    (
                        base.mode, base.uid, base.gid, base.nlink, base.flags, base.size,
                        base.atime
                    ),
                    "value fields come from the base Put untouched"
                );
            }
            other => panic!("expected a folded Put, got {other:?}"),
        }
    }

    #[test]
    fn fold_delta_without_base_is_a_counted_absent() {
        let before = META_KV_DELTA_ORPHANS.load(Ordering::Relaxed);
        let recs = [dtimes(9, 90, 99), dtimes(5, 50, 55)];
        let folded = fold(&recs).expect("Δ-without-base folds to absent, not an error (§4.2)");
        assert_eq!(folded, Folded::Absent);
        assert_eq!(
            META_KV_DELTA_ORPHANS.load(Ordering::Relaxed),
            before + 2,
            "each orphaned delta record counts in meta_kv_delta_orphans"
        );
    }

    #[test]
    fn fold_values_stay_opaque_unless_a_delta_forces_a_decode() {
        // A garbage Put value folds through untouched when no delta needs it.
        let garbage = Record::put(k(), 4, b"not-an-inode-value".to_vec());
        let folded = fold_newest_first([garbage.record_ref()]).expect("fold");
        assert_eq!(
            folded,
            Folded::Put {
                value: Cow::Borrowed(b"not-an-inode-value".as_slice()),
                seq: 4
            }
        );

        // With a delta on top, the base must decode — corrupt bases surface
        // as a clean error, never a panic.
        let recs = [dtimes(9, 1, 2), garbage];
        let res = fold(&recs);
        assert!(matches!(res, Err(KvError::Corrupt(_))));
    }

    // -- compaction fold + tombstone elision (§4.2) ---------------------------

    #[test]
    fn compact_fold_tombstone_elision_respects_the_durable_tail_boundary() {
        let d = del(7);
        let group = [d.record_ref()];

        // seq < durable_tail ⇒ elide (checkpoint-covered, replay can't resurrect).
        assert_eq!(compact_fold(&group, 8).expect("fold"), None);
        // seq == durable_tail ⇒ still inside the replay window ⇒ MUST survive.
        let kept = compact_fold(&group, 7)
            .expect("fold")
            .expect("tombstone kept");
        assert_eq!(kept.kind, RecordKind::Delete);
        assert_eq!(kept.seq, 7);
        assert_eq!(kept.key, k());
        assert!(kept.value.is_empty());
        // Anything older than the tombstone is inside the window too.
        assert!(compact_fold(&group, 0).expect("fold").is_some());
    }

    #[test]
    fn compact_fold_emits_one_folded_put_or_nothing_per_key() {
        // Delta + base ⇒ one folded Put carrying the newest seq.
        let recs = [dtimes(5, 50, 55), put(2, &iv(1))];
        let group: Vec<RecordRef<'_>> = recs.iter().map(|r| r.record_ref()).collect();
        let out = compact_fold(&group, 0).expect("fold").expect("live key");
        assert_eq!(out.kind, RecordKind::Put);
        assert_eq!(out.seq, 5);
        let folded = InodeValue::decode(&out.value).expect("decodes");
        assert_eq!((folded.mtime, folded.ctime), (50, 55));

        // Shadowed Puts collapse to the newest.
        let recs = [put(5, &iv(2)), put(2, &iv(1))];
        let group: Vec<RecordRef<'_>> = recs.iter().map(|r| r.record_ref()).collect();
        let out = compact_fold(&group, 0).expect("fold").expect("live key");
        assert_eq!((out.seq, out.value), (5, iv(2).encode()));

        // Orphan deltas compact to nothing (counted).
        let before = META_KV_DELTA_ORPHANS.load(Ordering::Relaxed);
        let recs = [dtimes(5, 50, 55)];
        let group: Vec<RecordRef<'_>> = recs.iter().map(|r| r.record_ref()).collect();
        assert_eq!(compact_fold(&group, 0).expect("fold"), None);
        assert_eq!(META_KV_DELTA_ORPHANS.load(Ordering::Relaxed), before + 1);

        // Tombstone below the tail shadows the Put AND elides: key vanishes.
        let recs = [del(7), put(3, &iv(1))];
        let group: Vec<RecordRef<'_>> = recs.iter().map(|r| r.record_ref()).collect();
        assert_eq!(compact_fold(&group, 8).expect("fold"), None);

        // Empty group is a no-op.
        assert_eq!(compact_fold(&[], 0).expect("fold"), None);
    }

    // -- the replay-reproduces-RAM theorem (§4.2: one fold, tested once) ------

    /// Post-fold user-visible state: key → live value bytes.
    fn model_live_state(records: &[Record]) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut groups: BTreeMap<&[u8], Vec<RecordRef<'_>>> = BTreeMap::new();
        for r in records {
            groups
                .entry(r.key.as_slice())
                .or_default()
                .push(r.record_ref());
        }
        let mut out = BTreeMap::new();
        for (key, mut refs) in groups {
            // Newest first; stable sort keeps push order for equal seqs
            // (identical-effect records, per the fold input contract).
            refs.sort_by(|a, b| b.seq.cmp(&a.seq));
            let folded = fold_newest_first(refs.iter().copied()).expect("fold");
            if let Some(v) = folded.live_value() {
                out.insert(key.to_vec(), v.to_vec());
            }
        }
        out
    }

    /// Compact every key of `records` at `durable_tail` (the §4.2 compaction
    /// fold), returning the node's replacement record set in key order.
    fn compact_history(records: &[Record], durable_tail: u64) -> Vec<Record> {
        let mut groups: BTreeMap<&[u8], Vec<RecordRef<'_>>> = BTreeMap::new();
        for r in records {
            groups
                .entry(r.key.as_slice())
                .or_default()
                .push(r.record_ref());
        }
        let mut out = Vec::new();
        for refs in groups.values_mut() {
            refs.sort_by(|a, b| b.seq.cmp(&a.seq));
            if let Some(rec) = compact_fold(refs, durable_tail).expect("compaction fold") {
                out.push(rec);
            }
        }
        out
    }

    #[test]
    fn replay_reproduces_ram_is_one_fold_theorem() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x5EED);
        for round in 0..50u32 {
            // A random history over a small keyspace: Puts, Δtimes, Deletes —
            // including Δ-without-base and Δ-after-delete replay shapes.
            let n_seq = 120u64;
            let mut history: Vec<Record> = Vec::new();
            for seq in 1..=n_seq {
                let key = inode_key(rng.gen_range(1..=6)).to_vec();
                let rec = match rng.gen_range(0..10) {
                    0..=4 => Record::put(key, seq, iv(seq).encode()),
                    5..=7 => Record::delta(key, seq, &InodeDelta::times(seq * 10, seq * 10 + 1)),
                    _ => Record::delete(key, seq),
                };
                history.push(rec);
            }
            let ram = model_live_state(&history);

            for tail in [0, rng.gen_range(1..=n_seq), n_seq + 1] {
                // Node state after compaction at `tail` + the journal replay
                // window (every record with seq ≥ tail) must fold to the
                // exact RAM state — the single theorem lookup/compaction/
                // replay all lean on (§4.2).
                let compacted = compact_history(&history, tail);
                let mut replay_input = compacted.clone();
                replay_input.extend(history.iter().filter(|r| r.seq >= tail).cloned());
                assert_eq!(
                    model_live_state(&replay_input),
                    ram,
                    "round {round}, tail {tail}: replay over (compacted node ∪ journal window) \
                     must reproduce RAM"
                );

                // The compaction fold is idempotent: re-compacting the output
                // at the same tail is a fixed point.
                assert_eq!(
                    compact_history(&compacted, tail),
                    compacted,
                    "round {round}, tail {tail}: compaction must be idempotent"
                );
            }
        }
    }

    // -- readdir cookies (§5.1) ------------------------------------------------

    #[test]
    fn readdir_cookie_reserved_offsets_and_bias() {
        assert_eq!(READDIR_COOKIE_BIAS, 3);
        assert_eq!(READDIR_OFFSET_DOT, 1);
        assert_eq!(READDIR_OFFSET_DOTDOT, 2);
        assert_eq!(
            encode_readdir_cookie(0, 0),
            3,
            "the smallest real-entry cookie sits just past the reserved offsets"
        );
        assert_eq!(decode_readdir_cookie(0).expect("decode"), ReaddirPos::Start);
        assert_eq!(
            decode_readdir_cookie(READDIR_OFFSET_DOT).expect("decode"),
            ReaddirPos::AfterDot
        );
        assert_eq!(
            decode_readdir_cookie(READDIR_OFFSET_DOTDOT).expect("decode"),
            ReaddirPos::AfterDotDot
        );
        assert_eq!(
            decode_readdir_cookie(3).expect("decode"),
            ReaddirPos::AfterEntry {
                hash54: 0,
                coll_seq: 0
            },
            "a forced hash54==0, coll_seq==0 name still gets a valid, distinct cookie"
        );
    }

    #[test]
    fn readdir_cookie_sign_bit_is_always_clear() {
        let max = encode_readdir_cookie(HASH54_MAX, 0xFF);
        assert_eq!(max, (1u64 << 62) + 2, "top-of-range cookie is 2^62 + 2");
        assert!(
            (max as i64) > 0,
            "a set sign bit would break i64 FUSE directory offsets (§5.1)"
        );
        let mut rng = ChaCha8Rng::seed_from_u64(4);
        for _ in 0..10_000 {
            let cookie = encode_readdir_cookie(rng.gen_range(0..=HASH54_MAX), rng.gen::<u8>());
            assert!(cookie >= READDIR_COOKIE_BIAS);
            assert!(
                (cookie as i64) > 0,
                "sign bit must be clear, got {cookie:#x}"
            );
        }
    }

    #[test]
    fn readdir_cookie_roundtrips_and_matches_the_key_suffix_resume_rule() {
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        for _ in 0..1000 {
            let (h, c) = (rng.gen_range(0..=HASH54_MAX), rng.gen::<u8>());
            let cookie = encode_readdir_cookie(h, c);
            assert_eq!(
                decode_readdir_cookie(cookie).expect("roundtrip"),
                ReaddirPos::AfterEntry {
                    hash54: h,
                    coll_seq: c
                }
            );
            // §5.1 resume rule: "entries whose key suffix is strictly greater
            // than c − 3" — the biased payload IS the §4.2 dentry key suffix.
            assert_eq!(cookie - READDIR_COOKIE_BIAS, dentry_key_suffix(h, c));
            let key = dentry_key(9, h, c);
            assert_eq!(
                u64::from_be_bytes(key[8..16].try_into().expect("8 bytes")),
                cookie - READDIR_COOKIE_BIAS,
                "cookies are the key itself — stable across concurrent inserts/removals"
            );
        }
        // Order-preserving: cookie order == (hash54, coll_seq) == key order.
        for _ in 0..1000 {
            let t1 = (rng.gen_range(0..=HASH54_MAX), rng.gen::<u8>());
            let t2 = (rng.gen_range(0..=HASH54_MAX), rng.gen::<u8>());
            assert_eq!(
                encode_readdir_cookie(t1.0, t1.1).cmp(&encode_readdir_cookie(t2.0, t2.1)),
                t1.cmp(&t2),
                "resume-by-strictly-greater needs cookie order == key-suffix order"
            );
        }
    }

    #[test]
    fn readdir_cookie_decode_rejects_payloads_we_never_issue() {
        assert!(matches!(
            decode_readdir_cookie(u64::MAX),
            Err(KvError::InvalidReaddirCookie(_))
        ));
        let first_bad = READDIR_COOKIE_BIAS + (1u64 << 62);
        assert!(matches!(
            decode_readdir_cookie(first_bad),
            Err(KvError::InvalidReaddirCookie(_))
        ));
        assert!(
            decode_readdir_cookie(first_bad - 1).is_ok(),
            "the largest issued cookie must decode"
        );
    }
}
