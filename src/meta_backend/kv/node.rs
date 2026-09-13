//! CoW btree node format: load, append, compact, split (PR K2).
//!
//! A node is one heap extent (`node_size`, 256 KiB default), copy-on-write,
//! internally log-structured (design §4.1):
//!
//! ```text
//! Node = | node header (4 KiB page) | bset frame 0 (base) | frame 1 | … | unwritten tail |
//! ```
//!
//! - The **node header** page carries `{magic, node_addr (self), node_seq,
//!   tree_id, level, min_key, max_key, format_version, xxh3}` — checksummed
//!   as a unit (§4.3), self-addressed so a misdirected read/write fails loud.
//! - Each append wraps a K1 bset image in a 32 B **bset frame**
//!   `{magic, node_seq_at_write, padded_len (4 KiB multiple), bset_len, xxh3}`
//!   and lands at the current 4 KiB-aligned tail. `node_seq_at_write` is the
//!   §4.1 incarnation stamp: recycled extents legitimately hold stale frames
//!   from a previous node life, and only frames stamped with **this** header's
//!   `node_seq` belong to the log.
//! - Appends mutate only **never-written bytes** (the unwritten tail), so a
//!   torn append can only damage data that was never live; rewrites
//!   (compact/split) go to **freshly allocated extents, never in place**
//!   (§4.1). That is the whole §4.10 node crash contract.
//!
//! **Torn-tail classification is positional, using only trustworthy bytes**
//! (§4.5): walk frames in append order and truncate the view at the first
//! failure; a torn unit's own bytes (frame fields, bset header — including
//! its `journal_seq_horizon`) are garbage and are never branched on. A
//! diagnosis pass then scans the remainder at 4 KiB strides for
//! same-incarnation bsets that *do* verify: one with
//! `journal_seq_horizon ≤ durable_tail` means checkpoint-covered data
//! follows a tear — impossible under the §4.6 barrier ordering — and the
//! node fails **loud** ([`KvError::CheckpointCoveredBsetAfterTear`]);
//! otherwise the tear is the expected un-checkpointed tail, dropped silently
//! and counted ([`super::META_KV_NODE_DROPPED_TAIL_BSETS`]).
//!
//! All extent I/O goes through `crate::uring_fs::{read_at, write_at}` —
//! io_uring only, per AGENTS.md. Everything here is a **pure function over
//! caller-provided extents**: the allocator arrives in PR K4, the node cache
//! and SMO scheduling in K5; tests inject extent offsets. Per the design's
//! liveness convention, K1–K5 code is production-unreachable until K6a wires
//! the mount path; this layer is kept alive by `tests/kv_node_tests.rs` and
//! the K2 crash cases in `tests/crash_contract_tests.rs`.

use super::bset::{build_bset, compact, BsetView, BSET_HEADER_LEN};
use super::record::Record;
use super::KvError;
use crate::uring_fs;
use bytes::Bytes;
use std::ops::Range;
use std::path::Path;

/// Debug-build write-side audit (Finding A hardening): every record value
/// about to be encoded into a frame must round-trip its tree's typed
/// decoder — a malformed encode (or an in-RAM corruption of the staged
/// records) fails HERE, on the writer with a backtrace, instead of
/// surfacing as a reader-side "corrupt KV encoding" on a checksum-valid
/// frame days later. Debug builds only: zero release-path cost; the
/// debug/test tiers (cargo gate, QUICK, soaks) all run with it armed.
///
/// Armed at the TYPED boundary — the backend's commit staging
/// (`commit_tx` / `commit_compensation`), the only layer whose contract
/// says values are inode/dentry/xattr encodings. The tree and node
/// layers are deliberately contents-agnostic containers (their tests
/// forge synthetic payloads to attack cap / overflow / torn-frame /
/// SMO mechanics), so the audit does not run there; every record a
/// production freeze/compact/split re-folds passed the audit when it
/// was first staged.
#[cfg(debug_assertions)]
pub(crate) fn debug_audit_records(tree_id: u8, level: u8, records: &[Record]) {
    use super::record::{
        DentryValue, InodeDelta, InodeValue, RecordKind, XattrValue, TREE_BLOCK_MAP,
        TREE_BLOCK_REFS, TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
    };
    for r in records {
        // `tree_id` is the record KIND and `r.key` its LEGACY key on both
        // layouts: the backend audits BEFORE it frames a forest key (and
        // un-frames a re-staged one first), so the per-kind decoders read
        // exactly the bytes they were written for.
        let kind = tree_id;
        let legacy: &[u8] = &r.key;
        let ok = match (level, r.kind) {
            (_, RecordKind::Delete) => true, // tombstones carry no value
            (l, RecordKind::Put) if l > 0 => r.value.len() == 16,
            (l, _) if l > 0 => false, // interior nodes hold Put/Delete pointers only
            (_, RecordKind::Put) => match kind {
                TREE_INODES => InodeValue::decode(&r.value).is_ok(),
                TREE_DENTRIES => DentryValue::decode(&r.value).is_ok(),
                TREE_XATTRS => XattrValue::decode(&r.value).is_ok(),
                // Spec §6.2 item 1: an accounting record whose key or
                // value does not decode under its own type would
                // mis-count shared ownership at recovery — audited here,
                // write-side, before a byte is persisted.
                TREE_BLOCK_REFS => {
                    super::block_refs::decode_block_ref_key(&legacy).is_ok()
                        && super::block_refs::decode_block_ref_value(&r.value).is_ok()
                }
                // PB-class files, PR 1: a map record whose key or value
                // does not decode under its own type would mis-resolve a
                // block at read time — audited write-side, before a byte
                // is persisted.
                TREE_BLOCK_MAP => {
                    super::block_map::decode_block_map_key(&legacy).is_ok()
                        && super::block_map::decode_block_map_value(&r.value).is_ok()
                }
                _ => true, // foreign trees (test harnesses) are not audited
            },
            (_, RecordKind::Delta) => kind != TREE_INODES || InodeDelta::decode(&r.value).is_ok(),
        };
        assert!(
            ok,
            "write-side encode audit: record (tree {tree_id}, level {level}, kind {:?}, \
             seq {}, key len {}, value len {}) does not decode under its own type — \
             refusing to persist a malformed record",
            r.kind,
            r.seq,
            r.key.len(),
            r.value.len()
        );
    }
}

/// Node page size: the header page, and the append granularity (§4.1).
pub const NODE_PAGE: usize = 4096;

/// Node header magic (`"KVND"`).
pub const NODE_MAGIC: u32 = u32::from_le_bytes(*b"KVND");
/// Current node format version (§4.1 `format_version`).
pub const NODE_FORMAT_VERSION: u16 = 1;
/// Fixed node-header length before the `min_key`/`max_key` bytes:
/// `magic: u32 | version: u16 | tree_id: u8 | level: u8 | node_addr: u64 |
/// node_seq: u64 | node_size: u32 | min_key_len: u16 | max_key_len: u16 |
/// checksum: u64`, little-endian. The checksum is xxh3_64 over the whole
/// 4 KiB header page with the checksum field zeroed (§4.3).
pub const NODE_HEADER_FIXED_LEN: usize = 40;

/// Bset frame magic (`"KBSF"`).
pub const BSET_FRAME_MAGIC: u32 = u32::from_le_bytes(*b"KBSF");
/// Current bset frame version.
pub const BSET_FRAME_VERSION: u16 = 1;
/// Fixed bset-frame header length: `magic: u32 | version: u16 |
/// reserved: u16 | node_seq_at_write: u64 | padded_len: u32 | bset_len: u32 |
/// checksum: u64`, little-endian. The checksum is xxh3_64 over the first
/// 24 bytes; the embedded bset image carries its own checksum (§4.3), and
/// the zero padding to `padded_len` is deliberately **not** covered — a torn
/// append may legitimately truncate padding without damaging the bset.
pub const BSET_FRAME_LEN: usize = 32;

/// Format-knob floor: 64 KiB (design §5.1 `--meta-node-kib`).
pub const MIN_NODE_SIZE: usize = 64 * 1024;
/// Format-knob ceiling: 1 MiB (design §4.4 / §5.1).
pub const MAX_NODE_SIZE: usize = 1024 * 1024;
/// Default node size (§4.1; bcachefs's shipped default).
pub const DEFAULT_NODE_SIZE: usize = 256 * 1024;
/// The user-facing record-value ceiling: Linux `XATTR_SIZE_MAX` (§4.2).
pub const RECORD_VALUE_CAP_CEILING: usize = 65_536;

/// Worst-case xattr record envelope OVER the user value: the
/// `XattrValue` framing is a 1-byte `name_len` + the name (≤ 255,
/// `XATTR_NAME_MAX`).
pub const XATTR_RECORD_ENVELOPE_MAX: usize = 256;

/// The user-facing xattr VALUE cap `min(65,536, node_size/4)` (§4.2):
/// exactly Linux `XATTR_SIZE_MAX` at the 256 KiB default node size.
pub fn xattr_value_cap(node_size: usize) -> usize {
    RECORD_VALUE_CAP_CEILING.min(node_size / 4)
}

/// The per-volume record-value cap — the node layer's budget for one
/// ENCODED record value, enforced at this layer on every write path
/// (PR K2). It is the user VALUE cap plus the xattr envelope allowance,
/// so a full `XATTR_SIZE_MAX` user value with a maximal name still fits
/// (fstests generic/020, VL10 release gate — the pre-fix formula charged
/// the envelope against the value cap, making the advertised
/// "value ≤ min(65536, node_size/4)" contract unreachable).
pub fn record_value_cap(node_size: usize) -> usize {
    xattr_value_cap(node_size) + XATTR_RECORD_ENVELOPE_MAX
}

/// A validated node-size knob (§5.1: 64 KiB–1 MiB, 4 KiB-page aligned).
/// The knob *plumbing* (superblock field, `--meta-node-kib`, the sub-256 KiB
/// format-time warning) is PR K6a's; here the size arrives as a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeLayout {
    node_size: usize,
}

impl NodeLayout {
    /// Validate `node_size`: within `[MIN_NODE_SIZE, MAX_NODE_SIZE]` and a
    /// multiple of [`NODE_PAGE`]; anything else is a format bug, rejected
    /// with [`KvError::Corrupt`].
    pub fn new(node_size: usize) -> Result<Self, KvError> {
        if !(MIN_NODE_SIZE..=MAX_NODE_SIZE).contains(&node_size) || node_size % NODE_PAGE != 0 {
            return Err(KvError::Corrupt(format!(
                "invalid node_size {node_size}: must be a 4 KiB multiple in \
                 [{MIN_NODE_SIZE}, {MAX_NODE_SIZE}]"
            )));
        }
        Ok(Self { node_size })
    }

    /// The validated node size in bytes.
    pub fn node_size(&self) -> usize {
        self.node_size
    }

    /// This layout's record-value cap: [`record_value_cap`]`(node_size)`.
    pub fn record_value_cap(&self) -> usize {
        record_value_cap(self.node_size)
    }

    /// This layout's user-facing xattr VALUE cap:
    /// [`xattr_value_cap`]`(node_size)`.
    pub fn xattr_value_cap(&self) -> usize {
        xattr_value_cap(self.node_size)
    }

    /// Encoded record bytes ONE node image can hold after its header page
    /// and one bset frame: the SMO's compaction-vs-split line (§4.4 fill
    /// accounting — a fold at-or-under this rewrites 1:1, above it splits)
    /// and the heap admission's projection of the same decision.
    pub fn fold_capacity(&self) -> usize {
        self.node_size - NODE_PAGE - BSET_FRAME_LEN - super::bset::BSET_HEADER_LEN
    }

    /// The SMO's per-part fill target on a split: parts fill to ¾ of
    /// [`Self::fold_capacity`] so appends have headroom (§4.4 fill
    /// accounting) — the divisor the heap admission's part-count
    /// projection shares with `KvTree::smo_replace`.
    pub fn split_part_capacity(&self) -> usize {
        self.fold_capacity() * 3 / 4
    }

    /// §4.6a (a): the encoded bytes two adjacent siblings' folds may sum
    /// to and still merge — exactly [`Self::split_part_capacity`], the
    /// split's ¾ fill target read backwards, so a merge successor is as
    /// full as a fresh split part and carries a part's append headroom.
    pub fn merge_pair_capacity(&self) -> usize {
        self.split_part_capacity()
    }

    /// §4.6a (a): the UNDERFULL bound — a node whose fold is at-or-under
    /// it is a merge candidate: the pair fill target less the split's
    /// balance point (`split_node` cuts a fold `> C` at its byte
    /// midpoint, so every fresh half holds more than `C/2`), i.e. `¾C −
    /// ½C = ¼C`. An underfull node merges with ANY sibling up to half
    /// full, and a fresh half never completes a pair with one — the
    /// structural hysteresis that keeps a split from being undone by the
    /// next merge.
    pub fn merge_candidate_capacity(&self) -> usize {
        self.split_part_capacity() - self.fold_capacity() / 2
    }

    /// Extents the SMO of a leaf whose fold measures `fold_bytes` in
    /// `parts` greedy ¾-fill parts claims (§4.7 heap admission's
    /// promise): one for a compaction (the fold fits one node), else the
    /// part count plus two — one for the packer's own rounding against a
    /// key-ordered estimate that over-sizes records, one for the CASCADE
    /// growth below [`Self::split_growth_window`] bytes of later
    /// admissions can add (bytes landing mid-order push each part's last
    /// group into the next, which is at most one extra part at the end).
    /// Over-promising by an extent only refuses growth an extent early
    /// (the surplus releases at the SMO); under-promising is what lets a
    /// flush pass run the heap to zero. A ROOT leaf's split also mints a
    /// new root (+1, the caller's).
    pub fn smo_extents_for_parts(&self, fold_bytes: usize, parts: usize) -> u64 {
        if fold_bytes <= self.fold_capacity() {
            1
        } else {
            parts as u64 + 2
        }
    }

    /// Bytes a promised leaf may absorb after its exact packing without a
    /// re-walk: every greedy part but the last closes because the next
    /// record did not fit, so a NEW part needs more than `budget − max
    /// record` bytes — below that only the cascade extent the promise
    /// already carries can be needed.
    pub fn split_growth_window(&self) -> usize {
        self.split_part_capacity()
            .saturating_sub(self.record_value_cap())
            .max(1)
    }

    /// On-disk length of the append frame `bytes` of encoded records
    /// produce (`encode_bset_frame`'s geometry: frame + bset headers,
    /// padded to the 4 KiB append granularity); 0 for no records.
    pub fn append_frame_len(&self, bytes: usize) -> usize {
        if bytes == 0 {
            0
        } else {
            page_align(BSET_FRAME_LEN + super::bset::BSET_HEADER_LEN + bytes)
        }
    }
}

/// Decoded node header (§4.1). `min_key`/`max_key` are the node's inclusive
/// key-space bounds — owned by the caller's tree logic (K5 revalidation);
/// this layer stores and returns them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHeader {
    /// Self extent byte offset — a mismatch at load is a misdirected
    /// read/write, failed loud.
    pub node_addr: u64,
    /// Node incarnation stamp; bset frames must match it to belong.
    pub node_seq: u64,
    pub tree_id: u8,
    /// 0 = leaf; interior nodes use the same format with `level > 0` (§4.2).
    pub level: u8,
    /// The extent size this node was written for (self-description).
    pub node_size: u32,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
}

/// xxh3 over a header page with the checksum field (bytes 32..40) zeroed.
fn header_page_checksum(page: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&page[..NODE_HEADER_FIXED_LEN - 8]);
    h.update(&[0u8; 8]);
    h.update(&page[NODE_HEADER_FIXED_LEN..]);
    h.digest()
}

impl NodeHeader {
    /// Serialize into a checksummed 4 KiB header page.
    fn encode_page(&self) -> Result<Vec<u8>, KvError> {
        let keys_len = self.min_key.len() + self.max_key.len();
        if self.min_key.len() > usize::from(u16::MAX)
            || self.max_key.len() > usize::from(u16::MAX)
            || NODE_HEADER_FIXED_LEN + keys_len > NODE_PAGE
        {
            return Err(KvError::Corrupt(format!(
                "node header keys too large for the header page: {} + {} bytes",
                self.min_key.len(),
                self.max_key.len()
            )));
        }
        let mut page = vec![0u8; NODE_PAGE];
        page[0..4].copy_from_slice(&NODE_MAGIC.to_le_bytes());
        page[4..6].copy_from_slice(&NODE_FORMAT_VERSION.to_le_bytes());
        page[6] = self.tree_id;
        page[7] = self.level;
        page[8..16].copy_from_slice(&self.node_addr.to_le_bytes());
        page[16..24].copy_from_slice(&self.node_seq.to_le_bytes());
        page[24..28].copy_from_slice(&self.node_size.to_le_bytes());
        page[28..30].copy_from_slice(&(self.min_key.len() as u16).to_le_bytes());
        page[30..32].copy_from_slice(&(self.max_key.len() as u16).to_le_bytes());
        let keys_start = NODE_HEADER_FIXED_LEN;
        page[keys_start..keys_start + self.min_key.len()].copy_from_slice(&self.min_key);
        page[keys_start + self.min_key.len()..keys_start + keys_len].copy_from_slice(&self.max_key);
        let sum = header_page_checksum(&page);
        page[32..40].copy_from_slice(&sum.to_le_bytes());
        Ok(page)
    }

    /// Parse and verify a 4 KiB header page (§4.3: magic/version gates
    /// first, whole-page checksum, then bounds — every length checked
    /// against its container before use, §9).
    fn decode_page(page: &[u8]) -> Result<Self, KvError> {
        if page.len() != NODE_PAGE {
            return Err(KvError::Corrupt(format!(
                "node header page must be {NODE_PAGE} bytes, got {}",
                page.len()
            )));
        }
        let magic = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        if magic != NODE_MAGIC {
            return Err(KvError::Corrupt(format!(
                "bad node magic {magic:#010x} (expected {NODE_MAGIC:#010x})"
            )));
        }
        let version = u16::from_le_bytes([page[4], page[5]]);
        if version != NODE_FORMAT_VERSION {
            return Err(KvError::Corrupt(format!(
                "unsupported node format version {version} \
                 (this binary understands {NODE_FORMAT_VERSION})"
            )));
        }
        let stored = u64::from_le_bytes(page[32..40].try_into().expect("8-byte slice"));
        let computed = header_page_checksum(page);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        let min_key_len = usize::from(u16::from_le_bytes([page[28], page[29]]));
        let max_key_len = usize::from(u16::from_le_bytes([page[30], page[31]]));
        if NODE_HEADER_FIXED_LEN + min_key_len + max_key_len > NODE_PAGE {
            return Err(KvError::Corrupt(format!(
                "node header key lengths overrun the header page: {min_key_len} + {max_key_len}"
            )));
        }
        let keys_start = NODE_HEADER_FIXED_LEN;
        Ok(Self {
            node_addr: u64::from_le_bytes(page[8..16].try_into().expect("8-byte slice")),
            node_seq: u64::from_le_bytes(page[16..24].try_into().expect("8-byte slice")),
            tree_id: page[6],
            level: page[7],
            node_size: u32::from_le_bytes([page[24], page[25], page[26], page[27]]),
            min_key: page[keys_start..keys_start + min_key_len].to_vec(),
            max_key: page[keys_start + min_key_len..keys_start + min_key_len + max_key_len]
                .to_vec(),
        })
    }
}

/// Identity + placement parameters for writing a fresh node image.
#[derive(Debug, Clone, Copy)]
pub struct NodeWriteParams<'a> {
    /// Destination extent byte offset (4 KiB aligned; caller-provided — the
    /// allocator arrives in K4).
    pub node_addr: u64,
    pub node_seq: u64,
    pub tree_id: u8,
    pub level: u8,
    pub min_key: &'a [u8],
    pub max_key: &'a [u8],
}

/// Where the next append lands: the triple a caller tracks between appends
/// (from [`LoadedNode::append_dest`] after a load, advancing `tail_offset`
/// with each [`append_bset`] return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendDest {
    /// Extent byte offset of the node.
    pub node_addr: u64,
    /// The node incarnation the frame is stamped with.
    pub node_seq: u64,
    /// Node-relative offset of the unwritten tail (4 KiB aligned,
    /// ≥ [`NODE_PAGE`]).
    pub tail_offset: usize,
}

/// Summary of a freshly written node image ([`write_node`] /
/// [`compact_node`] / [`split_node`] outputs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenNode {
    pub node_addr: u64,
    pub node_seq: u64,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    /// Records in the base bset (0 ⇒ header-only node).
    pub record_count: usize,
    /// Header page + padded base frame — also the loaded node's tail offset.
    pub bytes_written: usize,
    /// The base bset's `journal_seq_horizon` (compact/split derive it as the
    /// max over source bset horizons; 0 for a header-only node).
    pub journal_seq_horizon: u64,
}

/// A loaded, verified node: header + the surviving bset log (§4.5).
pub struct LoadedNode {
    header: NodeHeader,
    buf: Bytes,
    bset_ranges: Vec<Range<usize>>,
    tail_offset: usize,
    dropped_tail_bsets: u64,
}

impl std::fmt::Debug for LoadedNode {
    /// Summarizes instead of deriving: dumping the whole extent buffer into
    /// a failed assertion message helps nobody.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedNode")
            .field("header", &self.header)
            .field("extent_len", &self.buf.len())
            .field("bsets", &self.bset_ranges.len())
            .field("tail_offset", &self.tail_offset)
            .field("dropped_tail_bsets", &self.dropped_tail_bsets)
            .finish()
    }
}

impl LoadedNode {
    /// The verified node header.
    pub fn header(&self) -> &NodeHeader {
        &self.header
    }

    /// Number of live bsets, in append order (base first).
    pub fn bset_count(&self) -> usize {
        self.bset_ranges.len()
    }

    /// Parse the `i`-th bset (append order). Every range was verified at
    /// load, so re-parsing cannot fail; the `Result` only propagates the
    /// impossible for callers that must not panic. Panics on out-of-range
    /// `i`, like slice indexing.
    pub fn bset(&self, i: usize) -> Result<BsetView<'_>, KvError> {
        BsetView::parse(&self.buf[self.bset_ranges[i].clone()])
    }

    /// All bsets **newest-first** (reverse append order) — the source order
    /// the K1 fold/merge layer expects (`sources[0]` newest).
    pub fn bset_views_newest_first(&self) -> Result<Vec<BsetView<'_>>, KvError> {
        self.bset_ranges
            .iter()
            .rev()
            .map(|r| BsetView::parse(&self.buf[r.clone()]))
            .collect()
    }

    /// Point lookup across this node's bsets with the single fold algebra
    /// (§4.2): newest-first sources into `bset::lookup`.
    pub fn lookup(&self, key: &[u8]) -> Result<super::record::Folded<'_>, KvError> {
        let views = self.bset_views_newest_first()?;
        super::bset::lookup(&views, key)
    }

    /// Node-relative offset of the unwritten tail (where the next append
    /// lands), 4 KiB aligned.
    pub fn tail_offset(&self) -> usize {
        self.tail_offset
    }

    /// Torn/garbage tail bsets dropped by THIS load's §4.5 classifier
    /// (also accumulated in [`super::META_KV_NODE_DROPPED_TAIL_BSETS`]).
    pub fn dropped_tail_bsets(&self) -> u64 {
        self.dropped_tail_bsets
    }

    /// The append destination continuing this node's log.
    pub fn append_dest(&self) -> AppendDest {
        AppendDest {
            node_addr: self.header.node_addr,
            node_seq: self.header.node_seq,
            tail_offset: self.tail_offset,
        }
    }

    /// Decompose into `(header, extent buffer, verified bset ranges, tail
    /// offset)` — the K5 node cache takes ownership of the verified extent
    /// bytes zero-copy (its arc-swap snapshots slice this `Bytes`; §4.5
    /// "readers hand out Bytes/slice views into the snapshot").
    pub fn into_parts(self) -> (NodeHeader, Bytes, Vec<Range<usize>>, usize) {
        (self.header, self.buf, self.bset_ranges, self.tail_offset)
    }
}

/// The smallest key strictly greater than `key` in memcmp order:
/// `key ⧺ 0x00`. Sibling nodes partition the key space with it
/// (`right.min = key_successor(left.max)`, [`split_node`]) so the §4.6
/// revalidation predicate `min_key ≤ key ≤ max_key` admits every key the
/// interior separators can route to the node — no unroutable gaps.
pub fn key_successor(key: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(key.len() + 1);
    s.extend_from_slice(key);
    s.push(0);
    s
}

/// Round `len` up to the next [`NODE_PAGE`] multiple.
fn page_align(len: usize) -> usize {
    len.div_ceil(NODE_PAGE) * NODE_PAGE
}

/// Require a 4 KiB-aligned extent address.
fn check_addr_alignment(node_addr: u64) -> Result<(), KvError> {
    if node_addr % NODE_PAGE as u64 != 0 {
        return Err(KvError::Corrupt(format!(
            "node extent address {node_addr:#x} is not 4 KiB aligned"
        )));
    }
    Ok(())
}

/// Build one padded bset frame image (frame header + K1 bset + zero padding
/// to a 4 KiB multiple), enforcing the §4.2 record-value cap. `records` must
/// be non-empty and strictly `(key, seq)` ascending (bset build rules).
///
/// Public because it *is* the append/write wire format: the crash harness
/// forges frames with it to attack the §4.5 classifier from outside.
pub fn encode_bset_frame(
    layout: &NodeLayout,
    node_seq_at_write: u64,
    records: &[Record],
    journal_seq_horizon: u64,
) -> Result<Vec<u8>, KvError> {
    let cap = layout.record_value_cap();
    for r in records {
        if r.value.len() > cap {
            return Err(KvError::ValueTooLarge {
                len: r.value.len(),
                cap,
            });
        }
    }
    let bset = build_bset(records, journal_seq_horizon)?;
    let bset_len = u32::try_from(bset.len())
        .map_err(|_| KvError::Corrupt(format!("bset image length {} exceeds u32", bset.len())))?;
    let padded_len = page_align(BSET_FRAME_LEN + bset.len());
    let padded_len_u32 = u32::try_from(padded_len)
        .map_err(|_| KvError::Corrupt(format!("padded frame length {padded_len} exceeds u32")))?;

    let mut out = vec![0u8; padded_len];
    out[0..4].copy_from_slice(&BSET_FRAME_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&BSET_FRAME_VERSION.to_le_bytes());
    // bytes 6..8: reserved, zero.
    out[8..16].copy_from_slice(&node_seq_at_write.to_le_bytes());
    out[16..20].copy_from_slice(&padded_len_u32.to_le_bytes());
    out[20..24].copy_from_slice(&bset_len.to_le_bytes());
    let sum = xxhash_rust::xxh3::xxh3_64(&out[..24]);
    out[24..32].copy_from_slice(&sum.to_le_bytes());
    out[BSET_FRAME_LEN..BSET_FRAME_LEN + bset.len()].copy_from_slice(&bset);
    Ok(out)
}

/// A decoded, checksum-verified frame header (geometry not yet validated).
struct FrameHeader {
    node_seq_at_write: u64,
    padded_len: usize,
    bset_len: usize,
}

/// Outcome of inspecting one 4 KiB-aligned frame slot.
enum FrameProbe {
    /// No identifiable frame start: not this node's log (unwritten tail,
    /// stale garbage, or a tear that kept less than the magic).
    CleanEnd,
    /// Frame magic present but the header does not verify, or its version is
    /// foreign: a torn/garbage unit — the counted stop class.
    Garbage,
    /// A checksum-verified frame stamped by a DIFFERENT node incarnation:
    /// recycled-extent residue, a clean end (§4.1 `node_seq_at_write`).
    ///
    /// Direction note (Finding A): within one volume generation the
    /// ledger's `node_seq_watermark` makes mints strictly monotonic, so
    /// same-generation residue is always OLDER than the live node. A
    /// numerically HIGHER stamp still classifies here — quick-reformat
    /// residue from a dead generation carries stamps from a foreign
    /// uuid-derived base (`builder::node_seq_base`) that is above the
    /// live base on a coin flip, and must stay silently buried
    /// (`v3_quick_reformat_buries_previous_generation_records`). A loud
    /// higher-stamp tripwire would need a generation tag in the frame
    /// header (the reserved u16 is the candidate slot) to be sound;
    /// rejected for now — the watermark invariant is pinned directly in
    /// `tests/kv_finding_a_tests.rs` instead.
    StaleIncarnation,
    /// A checksum-verified frame stamped by THIS incarnation.
    Frame(FrameHeader),
}

/// Inspect the frame slot at `buf[pos..]` (caller guarantees at least
/// [`BSET_FRAME_LEN`] readable bytes). Branches only on checksum-verified
/// bytes (§4.5).
fn probe_frame(buf: &[u8], pos: usize, node_seq: u64) -> FrameProbe {
    let hdr = &buf[pos..pos + BSET_FRAME_LEN];
    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != BSET_FRAME_MAGIC {
        return FrameProbe::CleanEnd;
    }
    let stored = u64::from_le_bytes(hdr[24..32].try_into().expect("8-byte slice"));
    if stored != xxhash_rust::xxh3::xxh3_64(&hdr[..24]) {
        return FrameProbe::Garbage;
    }
    let version = u16::from_le_bytes([hdr[4], hdr[5]]);
    if version != BSET_FRAME_VERSION {
        return FrameProbe::Garbage;
    }
    let node_seq_at_write = u64::from_le_bytes(hdr[8..16].try_into().expect("8-byte slice"));
    if node_seq_at_write != node_seq {
        return FrameProbe::StaleIncarnation;
    }
    FrameProbe::Frame(FrameHeader {
        node_seq_at_write,
        padded_len: u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]) as usize,
        bset_len: u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]) as usize,
    })
}

/// Whether `page` (≥ [`BSET_FRAME_LEN`] bytes of a node's log area at a
/// 4 KiB boundary) already holds a **checksum-verified frame of this node
/// incarnation** — the multi-appender foreign-append probe (pre-RC
/// engineering spec §6.2 closing / §6.3): a peer that appended into a node
/// whose log tail we remember would otherwise be silently overwritten by
/// our next append, which writes at that remembered offset and only checks
/// the node incarnation. Branches only on verified bytes, exactly like the
/// §4.5 classifier: garbage, a clean end, and a previous incarnation's
/// residue are all "not a peer's frame".
///
/// Only [`super::node_cache::NodeCache::append_frozen`] calls it, and only
/// on a partitioned volume — a solo volume has no peers by construction and
/// pays no extra device read.
pub fn page_holds_live_frame(page: &[u8], node_seq: u64) -> bool {
    if page.len() < BSET_FRAME_LEN {
        return false;
    }
    matches!(probe_frame(page, 0, node_seq), FrameProbe::Frame(_))
}

/// Validate a same-incarnation frame's geometry against its container
/// (§9 bounds rule): 4 KiB-multiple `padded_len` within the extent, a bset
/// no smaller than its header and no larger than the frame.
fn frame_geometry_ok(f: &FrameHeader, pos: usize, node_size: usize) -> bool {
    f.padded_len >= NODE_PAGE
        && f.padded_len % NODE_PAGE == 0
        && f.padded_len <= node_size - pos
        && f.bset_len >= BSET_HEADER_LEN
        && BSET_FRAME_LEN + f.bset_len <= f.padded_len
}

/// Write a fresh node image — header page plus, when `base_records` is
/// non-empty, one base bset frame — to the caller-provided extent with a
/// single `uring_fs::write_at`. This is the rewrite primitive: it must only
/// ever target extents holding no live data (CoW, §4.1); enforcing that is
/// the K4 allocator's and K5 SMO task's job.
///
/// Typed failures: [`KvError::ValueTooLarge`] (a value over
/// `min(65,536, node_size/4)`), [`KvError::NodeFull`] (image exceeds the
/// extent), [`KvError::Io`] (uring path).
pub async fn write_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    params: &NodeWriteParams<'_>,
    base_records: &[Record],
    journal_seq_horizon: u64,
) -> Result<WrittenNode, KvError> {
    check_addr_alignment(params.node_addr)?;
    // Finding 41 defense-in-depth: a record outside its node's key bounds
    // is unroutable forever (revalidation rejects the key the parent
    // separators route here) — refuse LOUD at the write instead of landing
    // durable stranded state. Records are (key, seq)-ascending (bset law),
    // so the first/last keys bound the set.
    if let (Some(first), Some(last)) = (base_records.first(), base_records.last()) {
        if first.key[..] < *params.min_key || last.key[..] > *params.max_key {
            return Err(KvError::Corrupt(format!(
                "node write at {:#x} carries records outside its key bounds \
                 (first {:02x?}, last {:02x?} vs [{:02x?}, {:02x?}]) — a \
                 stranded-record hazard (finding 41)",
                params.node_addr, first.key, last.key, params.min_key, params.max_key
            )));
        }
    }
    let header = NodeHeader {
        node_addr: params.node_addr,
        node_seq: params.node_seq,
        tree_id: params.tree_id,
        level: params.level,
        node_size: layout.node_size() as u32,
        min_key: params.min_key.to_vec(),
        max_key: params.max_key.to_vec(),
    };
    let mut image = header.encode_page()?;
    let horizon = if base_records.is_empty() {
        0
    } else {
        let frame = encode_bset_frame(layout, params.node_seq, base_records, journal_seq_horizon)?;
        if NODE_PAGE + frame.len() > layout.node_size() {
            return Err(KvError::NodeFull {
                needed: NODE_PAGE + frame.len(),
                available: layout.node_size(),
            });
        }
        image.extend_from_slice(&frame);
        journal_seq_horizon
    };
    let bytes_written = image.len();
    uring_fs::write_at(path, params.node_addr, image).await?;
    // §10 / §8 row 7: whole-node CoW rewrite bytes (compaction / split
    // successors and builder/format output alike — every fresh image).
    super::META_KV_NODE_REWRITE_BYTES
        .fetch_add(bytes_written as u64, std::sync::atomic::Ordering::Relaxed);
    Ok(WrittenNode {
        node_addr: params.node_addr,
        node_seq: params.node_seq,
        min_key: header.min_key,
        max_key: header.max_key,
        record_count: base_records.len(),
        bytes_written,
        journal_seq_horizon: horizon,
    })
}

/// Append one frozen bset to a node's unwritten tail (4 KiB granularity,
/// §4.1) with a single `uring_fs::write_at`; returns the new tail offset.
/// Appends never touch previously written bytes — the §4.10 torn-append
/// immunity argument rests on exactly this.
///
/// Typed failures: [`KvError::ValueTooLarge`], [`KvError::NodeFull`] (the
/// caller's signal to compact — §4.6 pt 1), [`KvError::Io`].
pub async fn append_bset(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    dest: &AppendDest,
    records: &[Record],
    journal_seq_horizon: u64,
) -> Result<usize, KvError> {
    check_addr_alignment(dest.node_addr)?;
    if dest.tail_offset < NODE_PAGE
        || dest.tail_offset % NODE_PAGE != 0
        || dest.tail_offset > layout.node_size()
    {
        return Err(KvError::Corrupt(format!(
            "append tail offset {} is not a 4 KiB-aligned log position within the node",
            dest.tail_offset
        )));
    }
    let frame = encode_bset_frame(layout, dest.node_seq, records, journal_seq_horizon)?;
    let available = layout.node_size() - dest.tail_offset;
    if frame.len() > available {
        return Err(KvError::NodeFull {
            needed: frame.len(),
            available,
        });
    }
    let new_tail = dest.tail_offset + frame.len();
    let frame_len = frame.len() as u64;
    uring_fs::write_at(path, dest.node_addr + dest.tail_offset as u64, frame).await?;
    // §10 / §8 row 7: writeback append accounting (4 KiB-padded frames).
    super::META_KV_NODE_APPENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::META_KV_NODE_APPEND_BYTES.fetch_add(frame_len, std::sync::atomic::Ordering::Relaxed);
    Ok(new_tail)
}

/// Load a node: one `uring_fs::read_at` of the whole extent, header + bset
/// verification (§4.3, once at load), and the §4.5 positional torn-tail
/// classification against `durable_tail` (the durable journal tail seq —
/// caller-provided; the root ledger arrives in K3).
///
/// Loud failures: header corruption / self-address mismatch
/// ([`KvError::Corrupt`] / [`KvError::ChecksumMismatch`]) and the §4.5
/// checkpoint-violation tear ([`KvError::CheckpointCoveredBsetAfterTear`]).
/// Expected power-loss artifacts (a torn tail append) are dropped silently
/// and counted instead ([`LoadedNode::dropped_tail_bsets`]).
pub async fn load_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    node_addr: u64,
    durable_tail: u64,
) -> Result<LoadedNode, KvError> {
    check_addr_alignment(node_addr)?;
    let node_size = layout.node_size();
    let buf = uring_fs::read_at(path, node_addr, node_size).await?;
    if buf.len() != node_size {
        return Err(KvError::Corrupt(format!(
            "short extent read at {node_addr:#x}: {} of {node_size} bytes \
             (extent beyond the volume?)",
            buf.len()
        )));
    }
    verify_node_extent(buf, layout, node_addr, durable_tail)
}

/// The **pure** half of [`load_node`]: everything after the device read —
/// header decode, self-address/geometry checks, the §4.5 append walk, and
/// the torn-tail diagnosis pass. Split out so the whole on-disk node
/// grammar is reachable without I/O: `fuzz/fuzz_targets/kv_node.rs`
/// (spec §11 TEST-4) drives arbitrary bytes through exactly this, and it
/// is the same code path a real mount takes.
///
/// `buf.len()` must equal `layout.node_size()` — the caller's read already
/// established that.
pub fn verify_node_extent(
    buf: Bytes,
    layout: &NodeLayout,
    node_addr: u64,
    durable_tail: u64,
) -> Result<LoadedNode, KvError> {
    let node_size = layout.node_size();
    if buf.len() != node_size {
        return Err(KvError::Corrupt(format!(
            "node extent length {} != node_size {node_size}",
            buf.len()
        )));
    }
    if buf.len() < NODE_PAGE {
        return Err(KvError::Corrupt(format!(
            "node extent {} shorter than one header page",
            buf.len()
        )));
    }
    let header = NodeHeader::decode_page(&buf[..NODE_PAGE])?;
    if header.node_addr != node_addr {
        return Err(KvError::Corrupt(format!(
            "node self-address mismatch: header says {:#x}, read from {node_addr:#x} \
             (misdirected read/write)",
            header.node_addr
        )));
    }
    if header.node_size as usize != node_size {
        return Err(KvError::Corrupt(format!(
            "node_size mismatch: header says {}, volume layout says {node_size}",
            header.node_size
        )));
    }

    // Main walk (§4.5): frames in append order; truncate at the first unit
    // that does not verify. Only checksum-verified bytes are branched on.
    let mut bset_ranges: Vec<Range<usize>> = Vec::new();
    let mut pos = NODE_PAGE;
    let mut torn_stop = false; // an identifiable-but-invalid unit at `pos`
    while pos < node_size {
        match probe_frame(&buf, pos, header.node_seq) {
            FrameProbe::CleanEnd | FrameProbe::StaleIncarnation => break,
            FrameProbe::Garbage => {
                torn_stop = true;
                break;
            }
            FrameProbe::Frame(f) => {
                if !frame_geometry_ok(&f, pos, node_size) {
                    // A checksum-verified same-incarnation frame lying about
                    // its geometry cannot be a tear (a torn prefix breaks
                    // the checksum): writer bug or real corruption.
                    return Err(KvError::Corrupt(format!(
                        "node {node_addr:#x}: bset frame at offset {pos} (incarnation \
                         {}) has impossible geometry: padded_len {}, bset_len {}",
                        f.node_seq_at_write, f.padded_len, f.bset_len
                    )));
                }
                let bset_start = pos + BSET_FRAME_LEN;
                match BsetView::parse(&buf[bset_start..bset_start + f.bset_len]) {
                    Ok(_) => {
                        bset_ranges.push(bset_start..bset_start + f.bset_len);
                        pos += f.padded_len;
                    }
                    Err(_) => {
                        // The §4.5 torn tail bset: the frame header landed,
                        // the records did not. Never branch on its bytes.
                        torn_stop = true;
                        break;
                    }
                }
            }
        }
    }

    // Diagnosis pass (§4.5): scan the remainder at 4 KiB strides for
    // same-incarnation bsets that DO verify. horizon ≤ durable_tail ⇒
    // checkpoint-covered data after a tear ⇒ fail the node loud; otherwise
    // they are unreachable replay-window appends — dropped and counted.
    let mut dropped = u64::from(torn_stop);
    let mut probe = pos.saturating_add(NODE_PAGE);
    while probe + BSET_FRAME_LEN <= node_size {
        if let FrameProbe::Frame(f) = probe_frame(&buf, probe, header.node_seq) {
            if frame_geometry_ok(&f, probe, node_size) {
                let bset_start = probe + BSET_FRAME_LEN;
                if let Ok(view) = BsetView::parse(&buf[bset_start..bset_start + f.bset_len]) {
                    let horizon = view.journal_seq_horizon();
                    if horizon <= durable_tail {
                        return Err(KvError::CheckpointCoveredBsetAfterTear {
                            node_addr,
                            bset_offset: probe,
                            horizon,
                            durable_tail,
                        });
                    }
                    dropped += 1;
                }
            }
        }
        probe += NODE_PAGE;
    }

    if dropped > 0 {
        super::META_KV_NODE_DROPPED_TAIL_BSETS
            .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(LoadedNode {
        header,
        buf,
        bset_ranges,
        tail_offset: pos,
        dropped_tail_bsets: dropped,
    })
}

/// Fold a source node's whole bset log plus the caller's frozen-delta
/// records into `(folded records, output horizon)` with the single K1
/// algebra (§4.2) under the tombstone-elision rule at `durable_tail`.
fn fold_node_sources(
    src: &LoadedNode,
    extra_records: &[Record],
    durable_tail: u64,
) -> Result<(Vec<Record>, u64), KvError> {
    let extra_horizon = extra_records.iter().map(|r| r.seq).max().unwrap_or(0);
    let extra_image = if extra_records.is_empty() {
        Vec::new()
    } else {
        build_bset(extra_records, extra_horizon)?
    };
    let mut views: Vec<BsetView<'_>> = Vec::new();
    if !extra_image.is_empty() {
        views.push(BsetView::parse(&extra_image)?);
    }
    let mut on_disk = src.bset_views_newest_first()?;
    views.append(&mut on_disk);
    let horizon = views
        .iter()
        .map(|v| v.journal_seq_horizon())
        .max()
        .unwrap_or(0);
    let folded = compact(&views, durable_tail)?;
    Ok((folded, horizon))
}

/// Require a rewrite destination that is 4 KiB aligned and not the source
/// extent (CoW: never in place, §4.1).
fn check_fresh_destination(src: &LoadedNode, dst_addr: u64) -> Result<(), KvError> {
    check_addr_alignment(dst_addr)?;
    if dst_addr == src.header.node_addr {
        return Err(KvError::Corrupt(format!(
            "CoW violation: rewrite of node {dst_addr:#x} must target a fresh extent, \
             never in place (§4.1)"
        )));
    }
    Ok(())
}

/// Compact `src` — its on-disk bset log **plus** `extra_records`, the
/// caller's frozen dirty delta that no longer fits the log (§4.6 pt 1 /
/// SMO successor build: "re-freeze any delta that accumulated … into the
/// successor") — into a single-base-bset node at a **fresh** caller-provided
/// extent (`dst_addr` ≠ the source extent — never in place, §4.1): n-way
/// merge + the single fold algebra with the §4.2 tombstone-elision rule at
/// `durable_tail`. `extra_records` is the newest fold source and must be
/// strictly `(key, seq)` ascending (bset build rules); empty is legal.
/// Identity (tree_id, level, min/max keys) carries over from `src`; the
/// output horizon is the max over source bset horizons and `extra_records`
/// seqs. A folded output too large for one node is [`KvError::NodeFull`] —
/// the caller's signal to split instead (§4.6 pt 1: "oversized
/// post-compaction ⇒ split"); a node's own log alone can never overflow
/// (folding only reclaims append padding), so that signal is reachable
/// exactly when the frozen delta folds in.
pub async fn compact_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    src: &LoadedNode,
    extra_records: &[Record],
    dst_addr: u64,
    dst_node_seq: u64,
    durable_tail: u64,
) -> Result<WrittenNode, KvError> {
    check_fresh_destination(src, dst_addr)?;
    let (folded, horizon) = fold_node_sources(src, extra_records, durable_tail)?;
    if !folded.is_empty() {
        let image_len = NODE_PAGE
            + page_align(
                BSET_FRAME_LEN
                    + BSET_HEADER_LEN
                    + folded
                        .iter()
                        .map(|r| r.record_ref().encoded_len())
                        .sum::<usize>(),
            );
        if image_len > layout.node_size() {
            return Err(KvError::NodeFull {
                needed: image_len,
                available: layout.node_size(),
            });
        }
    }
    write_node(
        path,
        layout,
        &NodeWriteParams {
            node_addr: dst_addr,
            node_seq: dst_node_seq,
            tree_id: src.header.tree_id,
            level: src.header.level,
            min_key: &src.header.min_key,
            max_key: &src.header.max_key,
        },
        &folded,
        horizon,
    )
    .await
}

/// Destination extent + incarnation for one side of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitDest {
    pub node_addr: u64,
    pub node_seq: u64,
}

/// Split `src` (+ the frozen-delta `extra_records`, as in [`compact_node`])
/// into two fresh nodes (left, right) at caller-provided extents: fold,
/// partition the folded records at an encoded-byte-balanced key boundary,
/// and write two images. Key-space bounds **partition** the source range
/// with no gap (the §4.6 revalidation contract `min_key ≤ key ≤ max_key`
/// must admit every key the interior separators route here): left spans
/// `[src.min_key, last left key]`, right spans
/// `[`[`key_successor`]`(last left key), src.max_key]` — the parent-pointer
/// update belongs to the K5 SMO task. Fewer than two folded records cannot
/// split ([`KvError::Corrupt`]); destinations must be fresh and distinct.
pub async fn split_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    src: &LoadedNode,
    extra_records: &[Record],
    left: &SplitDest,
    right: &SplitDest,
    durable_tail: u64,
) -> Result<(WrittenNode, WrittenNode), KvError> {
    check_fresh_destination(src, left.node_addr)?;
    check_fresh_destination(src, right.node_addr)?;
    if left.node_addr == right.node_addr {
        return Err(KvError::Corrupt(format!(
            "split destinations must be distinct extents (both {:#x})",
            left.node_addr
        )));
    }
    let (folded, horizon) = fold_node_sources(src, extra_records, durable_tail)?;
    if folded.len() < 2 {
        return Err(KvError::Corrupt(format!(
            "cannot split {} folded record(s): a split needs at least two keys",
            folded.len()
        )));
    }

    // Cut at an encoded-byte-balanced record boundary, both sides non-empty.
    let total: usize = folded.iter().map(|r| r.record_ref().encoded_len()).sum();
    let mut cut = folded.len() - 1;
    let mut acc = 0usize;
    for (i, r) in folded.iter().enumerate() {
        acc += r.record_ref().encoded_len();
        if acc * 2 >= total && i + 1 < folded.len() {
            cut = i + 1;
            break;
        }
    }
    // Finding 41 (the 8-GiB-crossing corruption): the cut may only land on
    // a KEY boundary. `compact_fold`'s lineage rule emits TWO same-key
    // records for a versioned layout chain (folded Put + retained link),
    // and a cut between them strands the link in the right sibling BELOW
    // its own `min_key` (= successor(left.max = key)) — unroutable forever;
    // the next gate-legal link then folds onto the bare Put and every
    // subsequent fold of the key refuses ("divergent layout-delta chain"),
    // wedging the checkpoint permanently. Advance to the next key boundary;
    // if none remains ahead, retreat to the previous one.
    while cut < folded.len() && folded[cut].key == folded[cut - 1].key {
        cut += 1;
    }
    if cut >= folded.len() {
        cut = folded.len() - 1;
        while cut > 0 && folded[cut].key == folded[cut - 1].key {
            cut -= 1;
        }
        if cut == 0 {
            return Err(KvError::Corrupt(
                "cannot split a node whose folded records all share one key \
                 (same-key cohesion, finding 41)"
                    .to_string(),
            ));
        }
    }
    let (left_records, right_records) = folded.split_at(cut);

    let path = path.as_ref();
    let left_written = write_node(
        path,
        layout,
        &NodeWriteParams {
            node_addr: left.node_addr,
            node_seq: left.node_seq,
            tree_id: src.header.tree_id,
            level: src.header.level,
            min_key: &src.header.min_key,
            max_key: &left_records[left_records.len() - 1].key,
        },
        left_records,
        horizon,
    )
    .await?;
    // Partition rule (§4.6 revalidation): right.min = successor(left.max),
    // never the first right key — a tighter bound would leave keys in
    // (left.max, first right key) routable by the parent separator yet
    // rejected by revalidation, an unroutable gap.
    let right_min = key_successor(&left_records[left_records.len() - 1].key);
    let right_written = write_node(
        path,
        layout,
        &NodeWriteParams {
            node_addr: right.node_addr,
            node_seq: right.node_seq,
            tree_id: src.header.tree_id,
            level: src.header.level,
            min_key: &right_min,
            max_key: &src.header.max_key,
        },
        right_records,
        horizon,
    )
    .await?;
    Ok((left_written, right_written))
}
