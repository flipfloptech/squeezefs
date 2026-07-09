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

use super::bset::BsetView;
use super::record::Record;
use super::KvError;
use bytes::Bytes;
use std::ops::Range;
use std::path::Path;

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

/// The per-volume record-value cap `min(65,536, node_size/4)` (§4.2) —
/// enforced at this layer on every write path (PR K2).
pub fn record_value_cap(node_size: usize) -> usize {
    let _ = node_size;
    todo!("PR K2 implementation commit")
}

/// A validated node-size knob (§5.1: 64 KiB–1 MiB, 4 KiB-page aligned).
/// The knob *plumbing* (superblock field, `--meta-node-kib`, the sub-256 KiB
/// format-time warning) is PR K6a's; here the size arrives as a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeLayout {
    _node_size: usize,
}

impl NodeLayout {
    /// Validate `node_size`: within `[MIN_NODE_SIZE, MAX_NODE_SIZE]` and a
    /// multiple of [`NODE_PAGE`]; anything else is a format bug, rejected
    /// with [`KvError::Corrupt`].
    pub fn new(node_size: usize) -> Result<Self, KvError> {
        let _ = node_size;
        todo!("PR K2 implementation commit")
    }

    /// The validated node size in bytes.
    pub fn node_size(&self) -> usize {
        todo!("PR K2 implementation commit")
    }

    /// This layout's record-value cap: [`record_value_cap`]`(node_size)`.
    pub fn record_value_cap(&self) -> usize {
        todo!("PR K2 implementation commit")
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
        todo!("PR K2 implementation commit")
    }

    /// Number of live bsets, in append order (base first).
    pub fn bset_count(&self) -> usize {
        todo!("PR K2 implementation commit")
    }

    /// Parse the `i`-th bset (append order). Every range was verified at
    /// load, so re-parsing cannot fail; the `Result` only propagates the
    /// impossible for callers that must not panic. Panics on out-of-range
    /// `i`, like slice indexing.
    pub fn bset(&self, i: usize) -> Result<BsetView<'_>, KvError> {
        let _ = i;
        todo!("PR K2 implementation commit")
    }

    /// All bsets **newest-first** (reverse append order) — the source order
    /// the K1 fold/merge layer expects (`sources[0]` newest).
    pub fn bset_views_newest_first(&self) -> Result<Vec<BsetView<'_>>, KvError> {
        todo!("PR K2 implementation commit")
    }

    /// Point lookup across this node's bsets with the single fold algebra
    /// (§4.2): newest-first sources into `bset::lookup`.
    pub fn lookup(&self, key: &[u8]) -> Result<super::record::Folded<'_>, KvError> {
        let _ = key;
        todo!("PR K2 implementation commit")
    }

    /// Node-relative offset of the unwritten tail (where the next append
    /// lands), 4 KiB aligned.
    pub fn tail_offset(&self) -> usize {
        todo!("PR K2 implementation commit")
    }

    /// Torn/garbage tail bsets dropped by THIS load's §4.5 classifier
    /// (also accumulated in [`super::META_KV_NODE_DROPPED_TAIL_BSETS`]).
    pub fn dropped_tail_bsets(&self) -> u64 {
        todo!("PR K2 implementation commit")
    }

    /// The append destination continuing this node's log.
    pub fn append_dest(&self) -> AppendDest {
        todo!("PR K2 implementation commit")
    }
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
    let _ = (layout, node_seq_at_write, records, journal_seq_horizon);
    todo!("PR K2 implementation commit")
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
    let _ = (
        path.as_ref(),
        layout,
        params,
        base_records,
        journal_seq_horizon,
    );
    todo!("PR K2 implementation commit")
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
    let _ = (path.as_ref(), layout, dest, records, journal_seq_horizon);
    todo!("PR K2 implementation commit")
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
    let _ = (path.as_ref(), layout, node_addr, durable_tail);
    todo!("PR K2 implementation commit")
}

/// Compact `src` into a single-base-bset node at a **fresh** caller-provided
/// extent (`dst_addr` ≠ the source extent — never in place, §4.1): n-way
/// merge + the single fold algebra with the §4.2 tombstone-elision rule at
/// `durable_tail`. Identity (tree_id, level, min/max keys) carries over from
/// `src`; the output horizon is the max over source bset horizons. A folded
/// output too large for one node is [`KvError::NodeFull`] — the caller's
/// signal to split instead (§4.6 pt 1).
pub async fn compact_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    src: &LoadedNode,
    dst_addr: u64,
    dst_node_seq: u64,
    durable_tail: u64,
) -> Result<WrittenNode, KvError> {
    let _ = (
        path.as_ref(),
        layout,
        src,
        dst_addr,
        dst_node_seq,
        durable_tail,
    );
    todo!("PR K2 implementation commit")
}

/// Destination extent + incarnation for one side of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitDest {
    pub node_addr: u64,
    pub node_seq: u64,
}

/// Split `src` into two fresh nodes (left, right) at caller-provided
/// extents: fold as in [`compact_node`], partition the folded records at an
/// encoded-byte-balanced key boundary, and write two images. Key-space
/// bounds: left spans `[src.min_key, last left key]`, right spans
/// `[first right key, src.max_key]` — the parent-pointer update belongs to
/// the K5 SMO task. Fewer than two folded records cannot split
/// ([`KvError::Corrupt`]); destinations must be fresh and distinct.
pub async fn split_node(
    path: impl AsRef<Path>,
    layout: &NodeLayout,
    src: &LoadedNode,
    left: &SplitDest,
    right: &SplitDest,
    durable_tail: u64,
) -> Result<(WrittenNode, WrittenNode), KvError> {
    let _ = (path.as_ref(), layout, src, left, right, durable_tail);
    todo!("PR K2 implementation commit")
}
