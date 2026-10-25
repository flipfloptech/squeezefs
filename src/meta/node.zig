//! CoW btree node format: header page, load walk, append, compact / split /
//! merge images (charter §5.1 "Node", §5.3; `docs/porting/kv_node.md`
//! §1–§2, §5). PORTABLE: every function here is a pure function over byte
//! images — the io binding reads one extent and writes the images this
//! module hands back.
//!
//!     Node (one heap extent, node_size bytes, 4 KiB-aligned address A):
//!       [0 .. 4096)          header page (checksummed as a unit; self-addressed)
//!       [4096 .. tail)       bset frames, in append order, each page_align()'d
//!       [tail .. node_size)  never-written tail (zeros / residue of a previous life)
//!
//! - CoW: compaction/split/merge write a FRESH extent, never in place.
//! - Appends write ONLY `[tail, tail + frame_len)` — never-written bytes —
//!   so a torn append damages nothing live.
//! - Incarnation: every frame carries `node_seq_at_write`; only frames equal
//!   to `header.node_seq` belong (recycled extents hold the previous life's
//!   frames beyond the new tail).
//! - Torn-tail classification is positional and branches only on
//!   checksum-verified bytes: the walk truncates at the first failure; a
//!   stride pass then diagnoses the remainder (a verifying same-incarnation
//!   bset with `horizon ≤ durable_tail` after a tear is checkpoint-covered
//!   data following a tear — impossible under the barrier order — LOUD;
//!   otherwise the un-checkpointed replay window, dropped and counted).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const bset = @import("bset.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Record = rec.Record;
const RecordRef = rec.RecordRef;
const BsetView = bset.BsetView;

pub const FrameStamp = bset.FrameStamp;
pub const FrameScreen = bset.FrameScreen;
pub const encodeBsetFrame = bset.encodeBsetFrame;
pub const keySuccessor = key.keySuccessor;

// ---------------------------------------------------------------------------
// §1.1 / §1.4 constants.
// ---------------------------------------------------------------------------

/// Header page size AND the append granularity: the sector/page atomicity
/// unit (the O_DIRECT sector grain's ceiling).
pub const NODE_PAGE: usize = bset.NODE_PAGE;
/// ASCII "KVND" read as a little-endian u32: appears in ASCII order on disk.
pub const NODE_MAGIC: u32 = std.mem.readInt(u32, "KVND", .little);
pub const NODE_FORMAT_VERSION: u16 = 1;
/// `4 + 2 + 1 + 1 + 8 + 8 + 4 + 2 + 2 + 8`.
pub const NODE_HEADER_FIXED_LEN: usize = 40;
pub const NODE_HEADER_CHECKSUM_OFF: usize = 32;

/// Format-knob floor `--meta-node-kib`.
pub const MIN_NODE_SIZE: usize = 64 * 1024;
/// Knob ceiling.
pub const MAX_NODE_SIZE: usize = 1024 * 1024;
/// bcachefs's shipped `btree_node_size`: interior < 3 MiB per 100 M records
/// (pinnable); 32–56 × 4 KiB appends amortize a compaction to ≈ 8 KiB per
/// append; one-node read under the ~100 µs NVMe knee (measured cold 256 KiB
/// leaf read ≈ 85–120 µs vs 4 KiB ≈ 20–80 µs); the value cap lands on
/// `XATTR_SIZE_MAX`.
pub const DEFAULT_NODE_SIZE: usize = 256 * 1024;
pub const RECORD_VALUE_CAP_CEILING = rec.RECORD_VALUE_CAP_CEILING;
pub const XATTR_RECORD_ENVELOPE_MAX = rec.XATTR_RECORD_ENVELOPE_MAX;
pub const xattrValueCap = rec.xattrValueCap;
pub const recordValueCap = rec.recordValueCap;

/// Traversal restarts a descent on a root/child seq mismatch, a routing
/// hole or a retired extent (a concurrent SMO). 256: SMOs are rare and
/// serialized, so more means a routing bug; before the per-restart
/// `yield_now` the "retry budget exhausted" tripwire fired on ~1/2 of the
/// bench runs — the budget is a bug detector, not a tuning knob.
pub const RETRY_BUDGET: u32 = 256;
/// A projection tree refreshes its root every 8 restarts: a racing SMO's
/// window closes in a handful of restarts; small against the budget.
pub const PROJECTION_REFRESH_EVERY: u32 = 8;
/// A reader mount's whole-extent read can race a single `write_at` append
/// and see a torn frame; three re-reads cover every such race (one append
/// in flight at a time per node).
pub const READER_LOAD_RETRIES: u32 = 3;

// ---- observability (names per the spec) ----
/// Torn/garbage tail bsets dropped by the load classifier; nonzero after a
/// CLEAN unmount is the corruption alert.
pub var meta_kv_node_dropped_tail_bsets: counters.ShardedCounter = .{};
pub var meta_kv_node_appends: counters.ShardedCounter = .{};
pub var meta_kv_node_append_bytes: counters.ShardedCounter = .{};
/// Whole-node CoW rewrite bytes: compaction / split / merge successors and
/// builder/format output alike — every fresh image.
pub var meta_kv_node_rewrite_bytes: counters.ShardedCounter = .{};
pub var meta_kv_node_images: counters.ShardedCounter = .{};
pub var meta_kv_foreign_frames_screened: counters.ShardedCounter = .{};
pub var meta_kv_foreign_frame_overwrite_detected: counters.ShardedCounter = .{};
/// A breach-class foreign frame (rule 1 or 4) on a DEVICE-FENCED substrate:
/// a write the reservation should have rejected. Its count is the ONE
/// `meta_kv_appender_fence_breach` key (a counter beside it counted the
/// same event under the same key: a strict parser refused the document).
pub var appender_fence_breach: counters.Tripwire = .init("meta_kv_appender_fence_breach");

// ---------------------------------------------------------------------------
// Errors and refusals.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// Structural refusal (the detail in `Refusal`).
    Corrupt,
    ChecksumMismatch,
    /// A record value over the layout's `recordValueCap`.
    ValueTooLarge,
    /// The image/frame does not fit the extent — the caller's signal to
    /// compact (append) or split (compact).
    NodeFull,
    /// A verifying same-incarnation bset with `horizon ≤ durable_tail`
    /// follows a tear: checkpoint-covered data after a tear.
    CheckpointCoveredBsetAfterTear,
    OutOfMemory,
};

/// Compaction / split / merge also run THE fold.
pub const FoldError = Error || rec.FoldError;

pub const Refusal = union(enum) {
    invalid_node_size: usize,
    header_keys_too_large: struct { min: usize, max: usize },
    header_bad_length: usize,
    bad_magic: u32,
    unsupported_version: u16,
    header_keys_overrun: struct { min: usize, max: usize },
    checksum_mismatch: struct { stored: u64, computed: u64 },
    extent_length: struct { got: usize, node_size: usize },
    self_address_mismatch: struct { header: u64, read: u64 },
    node_size_mismatch: struct { header: u32, layout: usize },
    impossible_geometry: struct { node_addr: u64, pos: usize, seq: u64, padded_len: usize, bset_len: usize },
    zombie_overwrite: struct { node_addr: u64, stamp: FrameStamp, at: usize, probe: usize },
    checkpoint_covered: struct { node_addr: u64, bset_offset: usize, horizon: u64, durable_tail: u64 },
    unaligned_address: u64,
    records_outside_bounds: u64,
    node_full: struct { needed: usize, available: usize },
    value_too_large: struct { len: usize, cap: usize },
    append_tail_offset: usize,
    cow_violation: u64,
    split_same_destination: u64,
    split_too_few: usize,
    split_one_key,
    merge_not_adjacent,
    merge_identity_mismatch,
    bset_too_large,

    pub fn err(self: Refusal) Error {
        return switch (self) {
            .checksum_mismatch => error.ChecksumMismatch,
            .node_full => error.NodeFull,
            .value_too_large => error.ValueTooLarge,
            .checkpoint_covered => error.CheckpointCoveredBsetAfterTear,
            else => error.Corrupt,
        };
    }

    /// The refusal text (the reference's strings where it has one).
    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .invalid_node_size => |n| try w.print("invalid node_size {d}: must be a 4 KiB multiple in [{d}, {d}]", .{ n, MIN_NODE_SIZE, MAX_NODE_SIZE }),
            .header_keys_too_large => |k| try w.print("node header keys too large for the header page: {d} + {d} bytes", .{ k.min, k.max }),
            .header_bad_length => |n| try w.print("node header page must be {d} bytes, got {d}", .{ NODE_PAGE, n }),
            .bad_magic => |m| try w.print("bad node magic 0x{x:0>8} (expected 0x{x:0>8})", .{ m, NODE_MAGIC }),
            .unsupported_version => |v| try w.print("unsupported node format version {d} (this binary understands {d})", .{ v, NODE_FORMAT_VERSION }),
            .header_keys_overrun => |k| try w.print("node header key lengths overrun the header page: {d} + {d}", .{ k.min, k.max }),
            .checksum_mismatch => |c| try w.print("node header checksum mismatch: stored 0x{x:0>16}, computed 0x{x:0>16}", .{ c.stored, c.computed }),
            .extent_length => |e| try w.print("node extent length {d} != node_size {d}", .{ e.got, e.node_size }),
            .self_address_mismatch => |s| try w.print("node self-address mismatch: header says 0x{x}, read from 0x{x} (misdirected read/write)", .{ s.header, s.read }),
            .node_size_mismatch => |n| try w.print("node_size mismatch: header says {d}, volume layout says {d}", .{ n.header, n.layout }),
            .impossible_geometry => |g| try w.print("node 0x{x}: bset frame at offset {d} (incarnation {d}) has impossible geometry: padded_len {d}, bset_len {d}", .{ g.node_addr, g.pos, g.seq, g.padded_len, g.bset_len }),
            .zombie_overwrite => |z| try w.print("node 0x{x}: a foreign frame (appender {d}, g {d}) at offset {d} sits BEFORE this lessee's own frame at offset {d} — a zombie overwrote an acked frame (foreign_frame_overwrite_detected, design §5.8.2 residual class (ii)); refusing to serve the log with a hole", .{ z.node_addr, z.stamp.appender_id, z.stamp.g, z.at, z.probe }),
            .checkpoint_covered => |c| try w.print("node 0x{x}: a verifying bset at offset {d} with journal_seq_horizon {d} ≤ durable tail {d} follows a torn unit — checkpoint-covered data after a tear", .{ c.node_addr, c.bset_offset, c.horizon, c.durable_tail }),
            .unaligned_address => |a| try w.print("node extent address 0x{x} is not 4 KiB aligned", .{a}),
            .records_outside_bounds => |a| try w.print("node write at 0x{x} carries records outside its key bounds — a stranded-record hazard (finding 41)", .{a}),
            .node_full => |n| try w.print("node full: {d} bytes needed, {d} available", .{ n.needed, n.available }),
            .value_too_large => |v| try w.print("record value of {d} bytes exceeds the per-volume cap of {d}", .{ v.len, v.cap }),
            .append_tail_offset => |t| try w.print("append tail offset {d} is not a 4 KiB-aligned log position within the node", .{t}),
            .cow_violation => |a| try w.print("CoW violation: rewrite of node 0x{x} must target a fresh extent, never in place (§4.1)", .{a}),
            .split_same_destination => |a| try w.print("split destinations must be distinct extents (both 0x{x})", .{a}),
            .split_too_few => |n| try w.print("cannot split {d} folded record(s): a split needs at least two keys", .{n}),
            .split_one_key => try w.writeAll("cannot split a node whose folded records all share one key (same-key cohesion, finding 41)"),
            .merge_not_adjacent => try w.writeAll("merge sources are not adjacent siblings: right.min_key != key_successor(left.max_key)"),
            .merge_identity_mismatch => try w.writeAll("merge sources differ in tree_id or level"),
            .bset_too_large => try w.writeAll("bset image or padded frame length exceeds u32"),
        }
    }
};

fn refuse(diag: ?*Refusal, r: Refusal) Error {
    if (diag) |d| d.* = r;
    return r.err();
}

// ---------------------------------------------------------------------------
// §1.4 NodeLayout — the validated node-size knob and the frame stamp.
// ---------------------------------------------------------------------------

/// The ONE frame's header length (format v4): every node of every volume.
pub const FRAME_LEN: usize = bset.BSET_FRAME_LEN;

pub const NodeLayout = struct {
    node_size: usize,
    /// The fencing stamp the next frame write carries: `(0, 0)` — the
    /// manager's structural stamp — until `stamped` names a lessee.
    stamp: FrameStamp = .{},

    /// Valid iff `64 KiB ≤ ns ≤ 1 MiB ∧ ns % 4096 == 0`; anything else is a
    /// format bug.
    pub fn new(node_size: usize, diag: ?*Refusal) Error!NodeLayout {
        if (node_size < MIN_NODE_SIZE or node_size > MAX_NODE_SIZE or node_size % NODE_PAGE != 0) {
            return refuse(diag, .{ .invalid_node_size = node_size });
        }
        return .{ .node_size = node_size };
    }

    /// This layout with `stamp` as the stamp the next write carries — a
    /// per-write copy.
    pub fn stamped(self: NodeLayout, stamp: FrameStamp) NodeLayout {
        return .{ .node_size = self.node_size, .stamp = stamp };
    }

    pub fn nodeSize(self: NodeLayout) usize {
        return self.node_size;
    }

    pub fn recordValueCapOf(self: NodeLayout) usize {
        return recordValueCap(self.node_size);
    }

    pub fn xattrValueCapOf(self: NodeLayout) usize {
        return xattrValueCap(self.node_size);
    }

    /// Encoded record bytes ONE node image holds after its header page and
    /// one bset frame: the compaction-vs-split line. 256 KiB: 257,976.
    pub fn foldCapacity(self: NodeLayout) usize {
        return self.node_size - NODE_PAGE - FRAME_LEN - bset.BSET_HEADER_LEN;
    }

    /// ¾ of `foldCapacity`: the per-part fill target on a split, so appends
    /// have headroom. 256 KiB: 193,482.
    pub fn splitPartCapacity(self: NodeLayout) usize {
        return self.foldCapacity() * 3 / 4;
    }

    /// §4.6a(a): two adjacent siblings' folds may sum to this and still
    /// merge — the split's fill target read backwards, so a merge successor
    /// is exactly as full as a fresh split part with a part's headroom.
    pub fn mergePairCapacity(self: NodeLayout) usize {
        return self.splitPartCapacity();
    }

    /// The UNDERFULL bound `¾C − ½C = ¼C`: a split cuts a fold `> C` at its
    /// byte midpoint, so every fresh half holds `> ½C` and never pairs with
    /// an underfull node (`> ½C + ≤ ¼C > ¾C`) — a split is never undone by
    /// the next merge; a merge successor needs `> ¼C` of growth to split
    /// again. 256 KiB: 64,494.
    pub fn mergeCandidateCapacity(self: NodeLayout) usize {
        return self.splitPartCapacity() - self.foldCapacity() / 2;
    }

    /// Extents the SMO of a leaf whose fold measures `fold_bytes` in `parts`
    /// greedy ¾-fill parts claims: 1 for a compaction, else `parts + 2` —
    /// one for the packer's rounding against an over-sizing key-ordered
    /// estimate, one for the cascade below `splitGrowthWindow`.
    pub fn smoExtentsForParts(self: NodeLayout, fold_bytes: usize, parts: usize) u64 {
        return if (fold_bytes <= self.foldCapacity()) 1 else @as(u64, parts) + 2;
    }

    /// Bytes a promised leaf absorbs after its exact packing without a
    /// re-walk: a new part needs more than `budget − max record` bytes.
    pub fn splitGrowthWindow(self: NodeLayout) usize {
        return @max(1, self.splitPartCapacity() -| self.recordValueCapOf());
    }

    /// On-disk length of the append frame `bytes` of encoded records
    /// produce; 0 for no records.
    pub fn appendFrameLen(self: NodeLayout, bytes: usize) usize {
        _ = self;
        return bset.appendFrameLen(bytes);
    }
};

// ---------------------------------------------------------------------------
// §1.1 The header page.
// ---------------------------------------------------------------------------

/// The fixed 40 B prefix of the header page, all LE. Keys follow at 40 with
/// no gap; the page is zero-padded to 4096.
pub const HeaderFixed = extern struct {
    magic_le: [4]u8,
    version_le: [2]u8,
    tree_id: u8,
    level: u8,
    node_addr_le: [8]u8,
    node_seq_le: [8]u8,
    node_size_le: [4]u8,
    min_key_len_le: [2]u8,
    max_key_len_le: [2]u8,
    checksum_le: [8]u8,
};

comptime {
    assert(@sizeOf(HeaderFixed) == NODE_HEADER_FIXED_LEN);
    assert(@offsetOf(HeaderFixed, "tree_id") == 6);
    assert(@offsetOf(HeaderFixed, "level") == 7);
    assert(@offsetOf(HeaderFixed, "node_addr_le") == 8);
    assert(@offsetOf(HeaderFixed, "node_seq_le") == 16);
    assert(@offsetOf(HeaderFixed, "node_size_le") == 24);
    assert(@offsetOf(HeaderFixed, "min_key_len_le") == 28);
    assert(@offsetOf(HeaderFixed, "max_key_len_le") == 30);
    assert(@offsetOf(HeaderFixed, "checksum_le") == NODE_HEADER_CHECKSUM_OFF);
    assert(NODE_MAGIC == 0x444E564B);
}

/// A decoded header. `min_key`/`max_key` are the node's inclusive key-space
/// bounds, borrowed from the page they were decoded from.
pub const NodeHeader = struct {
    node_addr: u64,
    node_seq: u64,
    tree_id: u8,
    level: u8,
    /// The extent size this node was written for (self-description).
    node_size: u32,
    min_key: []const u8,
    max_key: []const u8,
};

/// Identity + placement for a fresh node image.
pub const NodeWriteParams = struct {
    node_addr: u64,
    node_seq: u64,
    tree_id: u8,
    level: u8,
    min_key: []const u8,
    max_key: []const u8,
};

/// hash64 over the WHOLE page with bytes [32..40) zeroed — streamed, so the
/// zero-filled slot is hashed in place, never skipped.
pub fn headerPageChecksum(page: *const [NODE_PAGE]u8) u64 {
    return checksum.hash64FieldZeroed(page, NODE_HEADER_CHECKSUM_OFF);
}

/// The ONE header encoder. Refuses keys that do not fit the page.
pub fn encodeHeaderPage(layout: NodeLayout, params: NodeWriteParams, out: *[NODE_PAGE]u8, diag: ?*Refusal) Error!void {
    const min_len = params.min_key.len;
    const max_len = params.max_key.len;
    if (min_len > std.math.maxInt(u16) or max_len > std.math.maxInt(u16) or NODE_HEADER_FIXED_LEN + min_len + max_len > NODE_PAGE) {
        return refuse(diag, .{ .header_keys_too_large = .{ .min = min_len, .max = max_len } });
    }
    @memset(out, 0);
    var h: HeaderFixed = undefined;
    std.mem.writeInt(u32, &h.magic_le, NODE_MAGIC, .little);
    std.mem.writeInt(u16, &h.version_le, NODE_FORMAT_VERSION, .little);
    h.tree_id = params.tree_id;
    h.level = params.level;
    std.mem.writeInt(u64, &h.node_addr_le, params.node_addr, .little);
    std.mem.writeInt(u64, &h.node_seq_le, params.node_seq, .little);
    std.mem.writeInt(u32, &h.node_size_le, @intCast(layout.node_size), .little);
    std.mem.writeInt(u16, &h.min_key_len_le, @intCast(min_len), .little);
    std.mem.writeInt(u16, &h.max_key_len_le, @intCast(max_len), .little);
    @memset(&h.checksum_le, 0);
    out[0..NODE_HEADER_FIXED_LEN].* = std.mem.toBytes(h);
    @memcpy(out[NODE_HEADER_FIXED_LEN..][0..min_len], params.min_key);
    @memcpy(out[NODE_HEADER_FIXED_LEN + min_len ..][0..max_len], params.max_key);
    checksum.stamp(out, NODE_HEADER_CHECKSUM_OFF);
}

/// Decode order: length == 4096 → magic → version → checksum → `40 + min +
/// max ≤ 4096` → slice keys. A zeroed extent fails on magic (Corrupt),
/// never as a tear.
pub fn decodeHeaderPage(page: []const u8, diag: ?*Refusal) Error!NodeHeader {
    if (page.len != NODE_PAGE) return refuse(diag, .{ .header_bad_length = page.len });
    const p = page[0..NODE_PAGE];
    const h = std.mem.bytesToValue(HeaderFixed, p[0..NODE_HEADER_FIXED_LEN]);
    const magic = std.mem.readInt(u32, &h.magic_le, .little);
    if (magic != NODE_MAGIC) return refuse(diag, .{ .bad_magic = magic });
    const version = std.mem.readInt(u16, &h.version_le, .little);
    if (version != NODE_FORMAT_VERSION) return refuse(diag, .{ .unsupported_version = version });
    const stored = std.mem.readInt(u64, &h.checksum_le, .little);
    const computed = headerPageChecksum(p);
    if (stored != computed) return refuse(diag, .{ .checksum_mismatch = .{ .stored = stored, .computed = computed } });
    const min_len: usize = std.mem.readInt(u16, &h.min_key_len_le, .little);
    const max_len: usize = std.mem.readInt(u16, &h.max_key_len_le, .little);
    if (NODE_HEADER_FIXED_LEN + min_len + max_len > NODE_PAGE) return refuse(diag, .{ .header_keys_overrun = .{ .min = min_len, .max = max_len } });
    return .{
        .node_addr = std.mem.readInt(u64, &h.node_addr_le, .little),
        .node_seq = std.mem.readInt(u64, &h.node_seq_le, .little),
        .tree_id = h.tree_id,
        .level = h.level,
        .node_size = std.mem.readInt(u32, &h.node_size_le, .little),
        .min_key = p[NODE_HEADER_FIXED_LEN..][0..min_len],
        .max_key = p[NODE_HEADER_FIXED_LEN + min_len ..][0..max_len],
    };
}

// ---------------------------------------------------------------------------
// §1.5 Write and append images.
// ---------------------------------------------------------------------------

/// Where the next append lands: tracked in RAM between appends (there is
/// NO tail field on disk — the load walk derives it).
pub const AppendDest = struct {
    node_addr: u64,
    node_seq: u64,
    /// Node-relative offset of the unwritten tail (4 KiB aligned, ≥ 4096).
    tail_offset: usize,
};

/// A fresh node image: header page ‖ (base frame iff records). `image.len`
/// IS the fresh node's tail offset.
pub const WrittenNode = struct {
    node_addr: u64,
    node_seq: u64,
    /// Records in the base bset (0 ⇒ header-only node).
    record_count: usize,
    /// The base bset's horizon (0 for a header-only node).
    journal_seq_horizon: u64,
    /// Owned; written whole at `node_addr`.
    image: []u8,

    pub fn bytesWritten(self: *const WrittenNode) usize {
        return self.image.len;
    }

    /// The identity the image carries (keys borrow the image).
    pub fn header(self: *const WrittenNode) NodeHeader {
        // The encoder produced it; the decode cannot fail.
        return decodeHeaderPage(self.image[0..NODE_PAGE], null) catch unreachable;
    }

    pub fn deinit(self: *WrittenNode, gpa: Allocator) void {
        gpa.free(self.image);
        self.* = undefined;
    }
};

fn checkAddrAlignment(node_addr: u64, diag: ?*Refusal) Error!void {
    if (node_addr % NODE_PAGE != 0) return refuse(diag, .{ .unaligned_address = node_addr });
}

/// The frame encoder with this module's typed refusals.
fn encodeFrame(gpa: Allocator, layout: NodeLayout, node_seq_at_write: u64, records: []const RecordRef, horizon: u64, diag: ?*Refusal) Error![]u8 {
    return bset.encodeBsetFrame(gpa, layout.stamp, layout.node_size, node_seq_at_write, records, horizon) catch |e| switch (e) {
        error.ValueTooLarge => {
            const cap = layout.recordValueCapOf();
            var len: usize = 0;
            for (records) |r| if (r.value.len > cap) {
                len = r.value.len;
                break;
            };
            return refuse(diag, .{ .value_too_large = .{ .len = len, .cap = cap } });
        },
        error.BsetTooLarge => return refuse(diag, .bset_too_large),
        // `records` is non-empty and strictly ascending by the caller's
        // contract (the fold's output, or a freeze's sorted overlay).
        error.EmptyBset, error.RecordsNotAscending => unreachable,
        error.OutOfMemory => return error.OutOfMemory,
    };
}

/// `write_node`'s pure half: the fresh image for `params` — header page plus,
/// when `base_records` is non-empty, one base frame. Finding-41 guard: a
/// record outside the node's key bounds is unroutable forever, refused LOUD
/// before landing durable stranded state. `4096 + frame > node_size` ⇒
/// NodeFull. A header-only node (no records) is legal: tail 4096, horizon 0.
/// The rewrite counters move when the image is planned (one `write_at`
/// remains for the io binding).
pub fn writeNode(gpa: Allocator, layout: NodeLayout, params: NodeWriteParams, base_records: []const RecordRef, journal_seq_horizon: u64, diag: ?*Refusal) Error!WrittenNode {
    try checkAddrAlignment(params.node_addr, diag);
    if (base_records.len > 0) {
        const first = base_records[0].key;
        const last = base_records[base_records.len - 1].key;
        if (key.lessThan(first, params.min_key) or key.lessThan(params.max_key, last)) {
            return refuse(diag, .{ .records_outside_bounds = params.node_addr });
        }
    }
    var page: [NODE_PAGE]u8 = undefined;
    try encodeHeaderPage(layout, params, &page, diag);
    if (base_records.len == 0) {
        const image = try gpa.dupe(u8, &page);
        meta_kv_node_rewrite_bytes.add(image.len);
        meta_kv_node_images.inc();
        return .{ .node_addr = params.node_addr, .node_seq = params.node_seq, .record_count = 0, .journal_seq_horizon = 0, .image = image };
    }
    const frame = try encodeFrame(gpa, layout, params.node_seq, base_records, journal_seq_horizon, diag);
    defer gpa.free(frame);
    if (NODE_PAGE + frame.len > layout.node_size) {
        return refuse(diag, .{ .node_full = .{ .needed = NODE_PAGE + frame.len, .available = layout.node_size } });
    }
    const image = try gpa.alloc(u8, NODE_PAGE + frame.len);
    @memcpy(image[0..NODE_PAGE], &page);
    @memcpy(image[NODE_PAGE..], frame);
    meta_kv_node_rewrite_bytes.add(image.len);
    meta_kv_node_images.inc();
    return .{ .node_addr = params.node_addr, .node_seq = params.node_seq, .record_count = base_records.len, .journal_seq_horizon = journal_seq_horizon, .image = image };
}

/// One planned append: the padded frame and the ONE device offset it lands
/// at. Appends write only never-written tail bytes.
pub const AppendFrame = struct {
    /// `node_addr + tail_offset`.
    offset: u64,
    /// Owned; a 4 KiB multiple.
    frame: []u8,
    /// The node-relative tail after this frame lands.
    new_tail: usize,

    pub fn deinit(self: *AppendFrame, gpa: Allocator) void {
        gpa.free(self.frame);
        self.* = undefined;
    }
};

/// `append_bset`'s pure half. Preconditions: aligned address; `4096 ≤
/// tail_offset ≤ node_size ∧ tail_offset % 4096 == 0`. `frame.len >
/// node_size − tail_offset` ⇒ NodeFull (nothing written). The append
/// counters move here, when the frame is planned: the io binding's single
/// `write_at` is the only step left, and a refused append plans nothing.
pub fn appendBset(gpa: Allocator, layout: NodeLayout, dest: AppendDest, records: []const RecordRef, journal_seq_horizon: u64, diag: ?*Refusal) Error!AppendFrame {
    try checkAddrAlignment(dest.node_addr, diag);
    if (dest.tail_offset < NODE_PAGE or dest.tail_offset % NODE_PAGE != 0 or dest.tail_offset > layout.node_size) {
        return refuse(diag, .{ .append_tail_offset = dest.tail_offset });
    }
    const frame = try encodeFrame(gpa, layout, dest.node_seq, records, journal_seq_horizon, diag);
    errdefer gpa.free(frame);
    const available = layout.node_size - dest.tail_offset;
    if (frame.len > available) return refuse(diag, .{ .node_full = .{ .needed = frame.len, .available = available } });
    meta_kv_node_appends.inc();
    meta_kv_node_append_bytes.add(frame.len);
    return .{ .offset = dest.node_addr + dest.tail_offset, .frame = frame, .new_tail = dest.tail_offset + frame.len };
}

// ---------------------------------------------------------------------------
// §2 LOAD — the pure half of `load_node`.
// ---------------------------------------------------------------------------

pub const Range = struct { start: usize, end: usize };

/// A loaded, verified node: header + the surviving bset log. `buf` is the
/// caller's extent read (borrowed; the node cache takes ownership of the
/// verified bytes zero-copy).
pub const LoadedNode = struct {
    header: NodeHeader,
    buf: []const u8,
    /// Verified bset image ranges in append order (base first).
    bset_ranges: []Range,
    /// Where the next append lands.
    tail_offset: usize,
    dropped_tail_bsets: u64,
    /// Frames the screen classified FOREIGN at this load (0 or 1).
    foreign_frames_screened: u64,

    pub fn deinit(self: *LoadedNode, gpa: Allocator) void {
        gpa.free(self.bset_ranges);
        self.* = undefined;
    }

    pub fn bsetCount(self: *const LoadedNode) usize {
        return self.bset_ranges.len;
    }

    /// The `i`-th bset (append order). The range was verified at load, so
    /// the re-parse cannot fail; the error only propagates OOM.
    pub fn bsetAt(self: *const LoadedNode, gpa: Allocator, i: usize) Allocator.Error!BsetView {
        const r = self.bset_ranges[i];
        return BsetView.parse(gpa, self.buf[r.start..r.end]) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt, error.ChecksumMismatch => unreachable,
        };
    }

    /// All bsets NEWEST-first (reverse append order) — the K1 merge's
    /// `sources[0]`-newest shape. Free with `deinitViews`.
    pub fn bsetViewsNewestFirst(self: *const LoadedNode, gpa: Allocator) Allocator.Error![]BsetView {
        const views = try gpa.alloc(BsetView, self.bset_ranges.len);
        var done: usize = 0;
        errdefer {
            for (views[0..done]) |*v| v.deinit(gpa);
            gpa.free(views);
        }
        for (views, 0..) |*v, i| {
            v.* = try self.bsetAt(gpa, self.bset_ranges.len - 1 - i);
            done += 1;
        }
        return views;
    }

    /// Point lookup across this node's bsets with the single fold algebra.
    pub fn lookup(self: *const LoadedNode, gpa: Allocator, k: []const u8) rec.FoldError!rec.Folded {
        const views = try self.bsetViewsNewestFirst(gpa);
        defer deinitViews(gpa, views);
        return bset.lookup(gpa, views, k);
    }

    pub fn appendDest(self: *const LoadedNode) AppendDest {
        return .{ .node_addr = self.header.node_addr, .node_seq = self.header.node_seq, .tail_offset = self.tail_offset };
    }
};

pub fn deinitViews(gpa: Allocator, views: []BsetView) void {
    for (views) |*v| v.deinit(gpa);
    gpa.free(views);
}

/// `verify_node_extent`: a load with no lease plane (rule 3 alone — the
/// one-member forest's every load).
pub fn verifyNodeExtent(gpa: Allocator, buf: []const u8, layout: NodeLayout, node_addr: u64, durable_tail: u64, diag: ?*Refusal) Error!LoadedNode {
    return verifyNodeExtentScreened(gpa, buf, layout, node_addr, durable_tail, null, diag);
}

const ScreenedAt = struct { pos: usize, stamp: FrameStamp, rule: u8 };

/// The pure half of `load_node` under the frame screen. `buf.len` must be
/// exactly `node_size`. Header decode → self-address → node_size → the main
/// walk (§2.1) → the screened-stop diagnosis (§2.2) or the torn-tail pass
/// (§2.4). Branches only on checksum-verified bytes.
pub fn verifyNodeExtentScreened(gpa: Allocator, buf: []const u8, layout: NodeLayout, node_addr: u64, durable_tail: u64, screen: ?FrameScreen, diag: ?*Refusal) Error!LoadedNode {
    const node_size = layout.node_size;
    if (buf.len != node_size) return refuse(diag, .{ .extent_length = .{ .got = buf.len, .node_size = node_size } });
    const header = try decodeHeaderPage(buf[0..NODE_PAGE], diag);
    if (header.node_addr != node_addr) return refuse(diag, .{ .self_address_mismatch = .{ .header = header.node_addr, .read = node_addr } });
    if (header.node_size != node_size) return refuse(diag, .{ .node_size_mismatch = .{ .header = header.node_size, .layout = node_size } });

    // Main walk: frames in append order; truncate at the first unit that
    // does not verify.
    var ranges: std.ArrayList(Range) = .empty;
    errdefer ranges.deinit(gpa);
    var pos: usize = NODE_PAGE;
    var torn_stop = false;
    var prev_g: ?u32 = null;
    var screened_at: ?ScreenedAt = null;
    walk: while (pos < node_size) {
        switch (bset.probeFrame(buf, pos, header.node_seq)) {
            .clean_end, .stale_incarnation => break :walk,
            .garbage => {
                torn_stop = true;
                break :walk;
            },
            .frame => |f| {
                if (!bset.frameGeometryOk(f, pos, node_size)) {
                    // A verified same-incarnation frame lying about its
                    // geometry cannot be a tear (a torn prefix breaks the
                    // checksum): writer bug or real corruption.
                    return refuse(diag, .{ .impossible_geometry = .{ .node_addr = node_addr, .pos = pos, .seq = f.node_seq_at_write, .padded_len = f.padded_len, .bset_len = f.bset_len } });
                }
                // Rule 3 needs no plane; rules 1–2–4 read the screen.
                const rule: ?u8 = if (screen) |sc| sc.foreignRule(f.stamp, pos, prev_g) else if (prev_g != null and f.stamp.g < prev_g.?) 3 else null;
                if (rule) |r| {
                    screened_at = .{ .pos = pos, .stamp = f.stamp, .rule = r };
                    break :walk;
                }
                prev_g = f.stamp.g;
                const bset_start = pos + FRAME_LEN;
                var view = BsetView.parse(gpa, buf[bset_start .. bset_start + f.bset_len]) catch |e| switch (e) {
                    error.OutOfMemory => return error.OutOfMemory,
                    // The frame header landed, the records did not. Never
                    // branch on its bytes.
                    error.Corrupt, error.ChecksumMismatch => {
                        torn_stop = true;
                        break :walk;
                    },
                };
                view.deinit(gpa);
                try ranges.append(gpa, .{ .start = bset_start, .end = bset_start + f.bset_len });
                pos += f.padded_len;
            },
        }
    }

    // The screen's verdict: a foreign frame ends this lessee's log at `pos`;
    // counted by rule and class, then the stride scan looks PAST it for a
    // frame of the current generation — residual class (ii): a zombie wrote
    // at a position the successor had already written; acked loss, refused
    // loud rather than served with a hole.
    var screened: u64 = 0;
    if (screened_at) |s| {
        screened = 1;
        const pr_fenced = if (screen) |sc| sc.pr_fenced else false;
        const g_current: u32 = if (screen) |sc| sc.g_current else 0;
        if (FrameScreen.isBreachRule(s.rule) and pr_fenced) {
            var detail_buf: [256]u8 = undefined;
            const appender_current: u32 = if (screen) |sc| sc.appender_current orelse 0 else 0;
            const detail = std.fmt.bufPrint(&detail_buf, "node 0x{x}: a frame at offset {d} carries slot generation {d} from appender {d} against the current lease generation {d} of appender {d} (rule {d}) on a DEVICE-FENCED substrate — a write the reservation should have rejected", .{ node_addr, s.pos, s.stamp.g, s.stamp.appender_id, g_current, appender_current, s.rule }) catch detail_buf[0..0];
            appender_fence_breach.fire(detail);
        } else {
            meta_kv_foreign_frames_screened.inc();
            log.warn("node 0x{x}: bset frame at offset {d} (appender {d}, slot generation {d}) screened FOREIGN by rule {d} (design-symmetric-metadata §5.8.2; the lessee's current generation is {d}) — the log ends before it", .{ node_addr, s.pos, s.stamp.appender_id, s.stamp.g, s.rule, g_current });
        }
        var probe = s.pos +| NODE_PAGE;
        while (probe + FRAME_LEN <= node_size) : (probe += NODE_PAGE) {
            switch (bset.probeFrame(buf, probe, header.node_seq)) {
                .frame => |f| {
                    if (!bset.frameGeometryOk(f, probe, node_size)) continue;
                    const sc = screen orelse continue;
                    if (sc.isCurrentOwn(f.stamp)) {
                        meta_kv_foreign_frame_overwrite_detected.inc();
                        return refuse(diag, .{ .zombie_overwrite = .{ .node_addr = node_addr, .stamp = s.stamp, .at = s.pos, .probe = probe } });
                    }
                },
                else => {},
            }
        }
    }

    // Torn-tail diagnosis pass: scan the remainder at 4 KiB strides for
    // same-incarnation bsets that DO verify. `horizon ≤ durable_tail` ⇒
    // checkpoint-covered data after a tear ⇒ loud; otherwise unreachable
    // replay-window appends — dropped and counted. The horizon is a record
    // STAMP and the tail a ring-0 position: on a forest slot tree the
    // domain gap can only make this tripwire QUIETER, never a load unsafe.
    // A screened stop is the screen's stop, not a tear: skipped.
    var dropped: u64 = @intFromBool(torn_stop);
    if (screened_at == null) {
        var probe = pos +| NODE_PAGE;
        while (probe + FRAME_LEN <= node_size) : (probe += NODE_PAGE) {
            switch (bset.probeFrame(buf, probe, header.node_seq)) {
                .frame => |f| {
                    if (!bset.frameGeometryOk(f, probe, node_size)) continue;
                    const bset_start = probe + FRAME_LEN;
                    var view = BsetView.parse(gpa, buf[bset_start .. bset_start + f.bset_len]) catch |e| switch (e) {
                        error.OutOfMemory => return error.OutOfMemory,
                        error.Corrupt, error.ChecksumMismatch => continue,
                    };
                    const horizon = view.journal_seq_horizon;
                    view.deinit(gpa);
                    if (horizon <= durable_tail) {
                        return refuse(diag, .{ .checkpoint_covered = .{ .node_addr = node_addr, .bset_offset = probe, .horizon = horizon, .durable_tail = durable_tail } });
                    }
                    dropped += 1;
                },
                else => {},
            }
        }
    }
    if (dropped > 0) meta_kv_node_dropped_tail_bsets.add(dropped);
    return .{
        .header = header,
        .buf = buf,
        .bset_ranges = try ranges.toOwnedSlice(gpa),
        .tail_offset = pos,
        .dropped_tail_bsets = dropped,
        .foreign_frames_screened = screened,
    };
}

/// An extent image's load verdict as text — the refusal's own words (for
/// a covered frame behind a hole: its offset, its horizon, the tail) —
/// written into `out`; empty when the image verifies (or the verification
/// ran out of memory).
pub fn describeRefusal(gpa: Allocator, buf: []const u8, layout: NodeLayout, node_addr: u64, durable_tail: u64, out: []u8) []const u8 {
    var diag: Refusal = undefined;
    var loaded = verifyNodeExtent(gpa, buf, layout, node_addr, durable_tail, &diag) catch |e| {
        if (e == error.OutOfMemory) return "";
        var w: std.Io.Writer = .fixed(out);
        diag.write(&w) catch {};
        return w.buffered();
    };
    loaded.deinit(gpa);
    return "";
}

/// §2.6 The highest node-seq stamp BELOW `below` the RESIDUE of one extent
/// carries: the header's `node_seq` if the page decodes, and
/// `node_seq_at_write` of every frame header at any 4 KiB slot whose magic
/// is KBSF and whose checksum verifies over `[0..32)` — whatever
/// incarnation stamped it; geometry, version and reserved are never trusted
/// (only the stamp is read). 0 when nothing verifies. `below` is the
/// minting handle's ceiling: a stamp at or above it is another space's (an
/// earlier format's, a joiner's), can never equal a mint of this one, and
/// must not hide this space's own highest stamp beneath it — a handle that
/// ignores the foreign maximum would re-mint a lost root's seq into the
/// same extent (xfstests generic/056, 073, 342 after a power cut). The
/// census reclaim and a tree's first mint floor the writer's seq above it.
pub fn residueSeqCeiling(buf: []const u8, below: u64) u64 {
    var ceiling: u64 = 0;
    if (buf.len >= NODE_PAGE) {
        if (decodeHeaderPage(buf[0..NODE_PAGE], null)) |h| {
            if (h.node_seq < below) ceiling = h.node_seq;
        } else |_| {}
    }
    var pos: usize = NODE_PAGE;
    while (pos + FRAME_LEN <= buf.len) : (pos += NODE_PAGE) {
        const hdr = buf[pos..][0..FRAME_LEN];
        if (std.mem.readInt(u32, hdr[0..4], .little) != bset.BSET_FRAME_MAGIC) continue;
        const checked = bset.BSET_FRAME_CHECKSUM_OFF;
        if (std.mem.readInt(u64, hdr[checked..][0..8], .little) != checksum.hash64(hdr[0..checked])) continue;
        const stamp = std.mem.readInt(u64, hdr[8..16], .little);
        if (stamp < below) ceiling = @max(ceiling, stamp);
    }
    return ceiling;
}

/// Whether the RESIDUE of one extent carries an image `appender_id`
/// wrote: any 4 KiB slot past the header whose frame verifies (magic,
/// checksum over `[0..32)`) and whose fencing stamp names the appender —
/// whatever incarnation, whatever generation. An extent the appender
/// claimed but never wrote (a RAM claim, a grant it never drew on) answers
/// false: it is not an orphan IMAGE. The header page carries no stamp.
pub fn residueStampedBy(buf: []const u8, appender_id: u32) bool {
    var pos: usize = NODE_PAGE;
    while (pos + FRAME_LEN <= buf.len) : (pos += NODE_PAGE) {
        const hdr = buf[pos..][0..FRAME_LEN];
        if (std.mem.readInt(u32, hdr[0..4], .little) != bset.BSET_FRAME_MAGIC) continue;
        const checked = bset.BSET_FRAME_CHECKSUM_OFF;
        if (std.mem.readInt(u64, hdr[checked..][0..8], .little) != checksum.hash64(hdr[0..checked])) continue;
        if (std.mem.readInt(u32, hdr[24..28], .little) == appender_id) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// §5 COMPACT / SPLIT / MERGE — fresh images from folded sources.
// ---------------------------------------------------------------------------

/// A fold's output: owned records (strictly `(key, seq)` ascending) and the
/// horizon = max over every source view.
pub const Folded = struct {
    records: std.ArrayList(Record),
    horizon: u64,

    pub fn deinit(self: *Folded, gpa: Allocator) void {
        rec.deinitRecords(gpa, self.records.items);
        self.records.deinit(gpa);
        self.* = undefined;
    }

    pub fn refs(self: *const Folded, gpa: Allocator) Allocator.Error![]RecordRef {
        const out = try gpa.alloc(RecordRef, self.records.items.len);
        for (self.records.items, out) |*r, *o| o.* = r.ref();
        return out;
    }

    pub fn encodedBytes(self: *const Folded) usize {
        var n: usize = 0;
        for (self.records.items) |*r| n += r.ref().encodedLen();
        return n;
    }
};

fn maxSeq(records: []const RecordRef) u64 {
    var m: u64 = 0;
    for (records) |r| m = @max(m, r.seq);
    return m;
}

/// Fold the source nodes' whole bset logs plus each caller's frozen-delta
/// records (`extras[i]`, the newest source for node `i`; strictly `(key,
/// seq)` ascending; empty legal) with the single K1 algebra under the
/// tombstone-elision rule at `durable_tail`. Newest sources FIRST: every
/// extra image, then every node's views newest-first. Key spaces of distinct
/// nodes are disjoint, so the order across nodes is immaterial.
pub fn foldNodeSources(gpa: Allocator, srcs: []const *const LoadedNode, extras: []const []const RecordRef, durable_tail: u64) FoldError!Folded {
    assert(srcs.len == extras.len);
    var images: std.ArrayList([]u8) = .empty;
    defer {
        for (images.items) |img| gpa.free(img);
        images.deinit(gpa);
    }
    var views: std.ArrayList(BsetView) = .empty;
    defer {
        for (views.items) |*v| v.deinit(gpa);
        views.deinit(gpa);
    }
    for (extras) |extra| {
        if (extra.len == 0) continue;
        const img = bset.buildBset(gpa, extra, maxSeq(extra)) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            // The caller's contract: non-empty (checked) and ascending.
            error.EmptyBset, error.RecordsNotAscending, error.BsetTooLarge => unreachable,
        };
        try images.append(gpa, img);
        try views.append(gpa, BsetView.parse(gpa, img) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Corrupt, error.ChecksumMismatch => unreachable,
        });
    }
    for (srcs) |src| {
        var i = src.bset_ranges.len;
        while (i > 0) {
            i -= 1;
            try views.append(gpa, try src.bsetAt(gpa, i));
        }
    }
    var horizon: u64 = 0;
    for (views.items) |*v| horizon = @max(horizon, v.journal_seq_horizon);
    var out: Folded = .{ .records = .empty, .horizon = horizon };
    errdefer out.deinit(gpa);
    try bset.compact(gpa, views.items, durable_tail, &out.records);
    return out;
}

/// `f(N)`: the encoded bytes `N`'s log plus `extra` fold to under the
/// elision rule at `durable_tail` — the merge rule's measure.
pub fn foldBytes(gpa: Allocator, src: *const LoadedNode, extra: []const RecordRef, durable_tail: u64) FoldError!usize {
    var folded = try foldNodeSources(gpa, &.{src}, &.{extra}, durable_tail);
    defer folded.deinit(gpa);
    return folded.encodedBytes();
}

fn checkFreshDestination(src: *const LoadedNode, dst_addr: u64, diag: ?*Refusal) Error!void {
    try checkAddrAlignment(dst_addr, diag);
    if (dst_addr == src.header.node_addr) return refuse(diag, .{ .cow_violation = dst_addr });
}

/// Compact `src` — its on-disk log PLUS `extra` (the frozen dirty delta that
/// no longer fits) — into a single-base-bset image for a FRESH extent.
/// Identity carries over from `src`; the horizon is the max over sources. A
/// fold too large for one node is NodeFull — the split signal (a node's own
/// log alone never overflows: folding only reclaims append padding, so the
/// signal is reachable exactly when `extra` folds in).
pub fn compactNode(gpa: Allocator, layout: NodeLayout, src: *const LoadedNode, extra: []const RecordRef, dst_addr: u64, dst_node_seq: u64, durable_tail: u64, diag: ?*Refusal) FoldError!WrittenNode {
    try checkFreshDestination(src, dst_addr, diag);
    var folded = try foldNodeSources(gpa, &.{src}, &.{extra}, durable_tail);
    defer folded.deinit(gpa);
    if (folded.records.items.len > 0) {
        const image_len = NODE_PAGE + bset.pageAlign(FRAME_LEN + bset.BSET_HEADER_LEN + folded.encodedBytes());
        if (image_len > layout.node_size) return refuse(diag, .{ .node_full = .{ .needed = image_len, .available = layout.node_size } });
    }
    const refs = try folded.refs(gpa);
    defer gpa.free(refs);
    return writeNode(gpa, layout, .{
        .node_addr = dst_addr,
        .node_seq = dst_node_seq,
        .tree_id = src.header.tree_id,
        .level = src.header.level,
        .min_key = src.header.min_key,
        .max_key = src.header.max_key,
    }, refs, folded.horizon, diag);
}

/// Destination extent + incarnation for one side of a split.
pub const SplitDest = struct { node_addr: u64, node_seq: u64 };

pub const SplitResult = struct {
    left: WrittenNode,
    right: WrittenNode,

    pub fn deinit(self: *SplitResult, gpa: Allocator) void {
        self.left.deinit(gpa);
        self.right.deinit(gpa);
    }
};

/// The split cut over a folded record set: byte-balanced (`acc·2 ≥ total`
/// at `i + 1`), then finding-41 same-key cohesion forward-then-backward —
/// `compact_fold` emits TWO same-key records for a versioned layout chain
/// (folded Put + retained link) and a cut between them strands the link in
/// the right sibling BELOW its own `min_key`, unroutable forever. Null when
/// every record shares one key.
pub fn splitCut(folded: []const Record) ?usize {
    assert(folded.len >= 2);
    var total: usize = 0;
    for (folded) |*r| total += r.ref().encodedLen();
    var cut = folded.len - 1;
    var acc: usize = 0;
    for (folded, 0..) |*r, i| {
        acc += r.ref().encodedLen();
        if (acc * 2 >= total and i + 1 < folded.len) {
            cut = i + 1;
            break;
        }
    }
    while (cut < folded.len and std.mem.eql(u8, folded[cut].key, folded[cut - 1].key)) cut += 1;
    if (cut >= folded.len) {
        cut = folded.len - 1;
        while (cut > 0 and std.mem.eql(u8, folded[cut].key, folded[cut - 1].key)) cut -= 1;
        if (cut == 0) return null;
    }
    return cut;
}

/// Split `src` (+ `extra`) into two fresh images: fold, cut per `splitCut`,
/// and write two images whose bounds PARTITION the source range gap-free:
/// left `[src.min, L.last.key]`, right `[key_successor(L.last.key),
/// src.max]` (never the first right key — a tighter bound would leave an
/// unroutable gap). Both carry the same horizon, tree_id and level.
pub fn splitNode(gpa: Allocator, layout: NodeLayout, src: *const LoadedNode, extra: []const RecordRef, left: SplitDest, right: SplitDest, durable_tail: u64, diag: ?*Refusal) FoldError!SplitResult {
    try checkFreshDestination(src, left.node_addr, diag);
    try checkFreshDestination(src, right.node_addr, diag);
    if (left.node_addr == right.node_addr) return refuse(diag, .{ .split_same_destination = left.node_addr });
    var folded = try foldNodeSources(gpa, &.{src}, &.{extra}, durable_tail);
    defer folded.deinit(gpa);
    const items = folded.records.items;
    if (items.len < 2) return refuse(diag, .{ .split_too_few = items.len });
    const cut = splitCut(items) orelse return refuse(diag, .split_one_key);
    const refs = try folded.refs(gpa);
    defer gpa.free(refs);
    const left_refs = refs[0..cut];
    const right_refs = refs[cut..];
    const left_max = left_refs[left_refs.len - 1].key;
    var lw = try writeNode(gpa, layout, .{
        .node_addr = left.node_addr,
        .node_seq = left.node_seq,
        .tree_id = src.header.tree_id,
        .level = src.header.level,
        .min_key = src.header.min_key,
        .max_key = left_max,
    }, left_refs, folded.horizon, diag);
    errdefer lw.deinit(gpa);
    const right_min = keySuccessor(left_max);
    const rw = try writeNode(gpa, layout, .{
        .node_addr = right.node_addr,
        .node_seq = right.node_seq,
        .tree_id = src.header.tree_id,
        .level = src.header.level,
        .min_key = right_min.slice(),
        .max_key = src.header.max_key,
    }, right_refs, folded.horizon, diag);
    return .{ .left = lw, .right = rw };
}

/// §5.4 Two ADJACENT siblings under one parent are mergeable iff
/// `f(L) + f(R) ≤ ¾C ∧ min(f(L), f(R)) ≤ ¼C`.
pub fn mergeable(layout: NodeLayout, fold_left: usize, fold_right: usize) bool {
    return fold_left + fold_right <= layout.mergePairCapacity() and @min(fold_left, fold_right) <= layout.mergeCandidateCapacity();
}

/// `R.min == key_successor(L.max)`: the gap-free sibling partition.
pub fn adjacentSiblings(left: *const LoadedNode, right: *const LoadedNode) bool {
    const succ = keySuccessor(left.header.max_key);
    return std.mem.eql(u8, succ.slice(), right.header.min_key);
}

/// The merge successor image: `level = L.level`, `min = L.min`, `max =
/// R.max`, ONE fold of both logs plus both frozen deltas over the
/// concatenated newest-first views. The parent then applies `Put(R.max →
/// (succ_addr, succ_seq))` + `Delete(L.max)` (`mergeParentRecords`).
pub fn mergeNodes(gpa: Allocator, layout: NodeLayout, left: *const LoadedNode, right: *const LoadedNode, extra_left: []const RecordRef, extra_right: []const RecordRef, dst_addr: u64, dst_node_seq: u64, durable_tail: u64, diag: ?*Refusal) FoldError!WrittenNode {
    try checkFreshDestination(left, dst_addr, diag);
    try checkFreshDestination(right, dst_addr, diag);
    if (left.header.tree_id != right.header.tree_id or left.header.level != right.header.level) return refuse(diag, .merge_identity_mismatch);
    if (!adjacentSiblings(left, right)) return refuse(diag, .merge_not_adjacent);
    var folded = try foldNodeSources(gpa, &.{ left, right }, &.{ extra_left, extra_right }, durable_tail);
    defer folded.deinit(gpa);
    if (folded.records.items.len > 0) {
        const image_len = NODE_PAGE + bset.pageAlign(FRAME_LEN + bset.BSET_HEADER_LEN + folded.encodedBytes());
        if (image_len > layout.node_size) return refuse(diag, .{ .node_full = .{ .needed = image_len, .available = layout.node_size } });
    }
    const refs = try folded.refs(gpa);
    defer gpa.free(refs);
    return writeNode(gpa, layout, .{
        .node_addr = dst_addr,
        .node_seq = dst_node_seq,
        .tree_id = left.header.tree_id,
        .level = left.header.level,
        .min_key = left.header.min_key,
        .max_key = right.header.max_key,
    }, refs, folded.horizon, diag);
}

/// The parent's pointer records for a merge in the entry's order: `Put(R.max
/// → succ)` then `Delete(L.max)`, both at `seq` (the entry is `[Put, Delete,
/// alloc(succ), free(L), free(R)]`).
pub fn mergeParentRecords(gpa: Allocator, left_max: []const u8, right_max: []const u8, succ_addr: u64, succ_seq: u64, seq: u64) Allocator.Error![2]Record {
    var put = try Record.put(gpa, right_max, seq, &key.encodeInteriorValue(succ_addr, succ_seq));
    errdefer put.deinit(gpa);
    const del = try Record.delete(gpa, left_max, seq);
    return .{ put, del };
}

// ---------------------------------------------------------------------------
// Tests — kv_node.md §7 (the device-flagged ones as image tests) and the
// §1.4 tie tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const InodeValue = rec.InodeValue;
const InodeDelta = rec.InodeDelta;

fn inodeValue(seed: u64) InodeValue {
    return .{ .mode = 0o100644, .uid = @truncate(seed), .gid = 0, .nlink = 1, .size = seed * 3, .atime = seed, .mtime = seed, .ctime = seed };
}

fn putIno(gpa: Allocator, ino: u64, seq: u64) !Record {
    return Record.put(gpa, &key.inodeKey(ino), seq, &inodeValue(seq).encode());
}

fn refsOf(records: []const Record, out: []RecordRef) []RecordRef {
    for (records, 0..) |*r, i| out[i] = r.ref();
    return out[0..records.len];
}

fn wp(addr: u64, seq: u64, min_key: []const u8, max_key: []const u8) NodeWriteParams {
    return .{ .node_addr = addr, .node_seq = seq, .tree_id = key.TREE_INODES, .level = 0, .min_key = min_key, .max_key = max_key };
}

fn defaultLayout() NodeLayout {
    return NodeLayout.new(DEFAULT_NODE_SIZE, null) catch unreachable;
}

/// A RAM "volume": images land at their offsets; loads read one extent.
const Volume = struct {
    bytes: []u8,

    fn init(gpa: Allocator, len: usize) !Volume {
        const b = try gpa.alloc(u8, len);
        @memset(b, 0);
        return .{ .bytes = b };
    }

    fn deinit(self: *Volume, gpa: Allocator) void {
        gpa.free(self.bytes);
    }

    fn write(self: *Volume, offset: u64, data: []const u8) void {
        @memcpy(self.bytes[@intCast(offset)..][0..data.len], data);
    }

    fn writeNodeAt(self: *Volume, gpa: Allocator, layout: NodeLayout, p: NodeWriteParams, records: []const RecordRef, horizon: u64) !WrittenNode {
        const w = try writeNode(gpa, layout, p, records, horizon, null);
        self.write(w.node_addr, w.image);
        return w;
    }

    fn append(self: *Volume, gpa: Allocator, layout: NodeLayout, dest: *AppendDest, records: []const RecordRef, horizon: u64) !usize {
        var f = try appendBset(gpa, layout, dest.*, records, horizon, null);
        defer f.deinit(gpa);
        self.write(f.offset, f.frame);
        dest.tail_offset = f.new_tail;
        return f.new_tail;
    }

    fn extent(self: *const Volume, layout: NodeLayout, addr: u64) []const u8 {
        return self.bytes[@intCast(addr)..][0..layout.node_size];
    }

    fn load(self: *const Volume, gpa: Allocator, layout: NodeLayout, addr: u64, durable_tail: u64) !LoadedNode {
        return verifyNodeExtent(gpa, self.extent(layout, addr), layout, addr, durable_tail, null);
    }
};

test "NodeLayout validates the format knob: 64 KiB/128 KiB/256 KiB/1 MiB accepted; 0, 4096, 60 KiB, 256 KiB+1, +512, 2 MiB rejected Corrupt" {
    for ([_]usize{ MIN_NODE_SIZE, 128 * 1024, DEFAULT_NODE_SIZE, MAX_NODE_SIZE }) |good| {
        const l = try NodeLayout.new(good, null);
        try testing.expectEqual(good, l.nodeSize());
        try testing.expectEqual(FrameStamp{}, l.stamp);
    }
    var diag: Refusal = undefined;
    for ([_]usize{ 0, NODE_PAGE, MIN_NODE_SIZE - NODE_PAGE, DEFAULT_NODE_SIZE + 1, DEFAULT_NODE_SIZE + 512, MAX_NODE_SIZE * 2 }) |bad| {
        try testing.expectError(error.Corrupt, NodeLayout.new(bad, &diag));
        try testing.expectEqual(bad, diag.invalid_node_size);
    }
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try diag.write(&w);
    try testing.expectEqualStrings("invalid node_size 2097152: must be a 4 KiB multiple in [65536, 1048576]", w.buffered());
    // ONE frame: 40 B stamped (0, 0) by default; `stamped` is a per-write
    // copy that leaves the original's stamp alone.
    try testing.expectEqual(@as(usize, 40), FRAME_LEN);
    const l = defaultLayout();
    const st = l.stamped(.{ .appender_id = 3, .g = 9 });
    try testing.expectEqual(FrameStamp{ .appender_id = 3, .g = 9 }, st.stamp);
    try testing.expectEqual(FrameStamp{}, l.stamp);
    try testing.expectEqual(l.node_size, st.node_size);
}

test "record value cap is min(ceiling, node/4) + the 256 envelope; the §1.4 geometry ties: C 257,976 / ¾C 193,482 / ¼C 64,494 at 256 KiB; the 64 KiB variants" {
    try testing.expectEqual(@as(usize, 256), XATTR_RECORD_ENVELOPE_MAX);
    try testing.expectEqual(@as(usize, 65_536), xattrValueCap(DEFAULT_NODE_SIZE));
    try testing.expectEqual(@as(usize, 16_384), xattrValueCap(MIN_NODE_SIZE));
    try testing.expectEqual(@as(usize, 32_768), xattrValueCap(128 * 1024));
    try testing.expectEqual(@as(usize, 65_536), xattrValueCap(512 * 1024));
    try testing.expectEqual(@as(usize, 65_536), xattrValueCap(MAX_NODE_SIZE));
    for ([_]usize{ MIN_NODE_SIZE, 128 * 1024, DEFAULT_NODE_SIZE, MAX_NODE_SIZE }) |ns| {
        try testing.expectEqual(xattrValueCap(ns) + XATTR_RECORD_ENVELOPE_MAX, recordValueCap(ns));
    }
    const l = defaultLayout();
    try testing.expectEqual(@as(usize, 65_792), l.recordValueCapOf());
    try testing.expectEqual(@as(usize, 65_536), l.xattrValueCapOf());
    // C = node − 4096 (header page) − 40 (frame) − 32 (bset header).
    try testing.expectEqual(DEFAULT_NODE_SIZE - NODE_PAGE - FRAME_LEN - bset.BSET_HEADER_LEN, l.foldCapacity());
    try testing.expectEqual(@as(usize, 257_976), l.foldCapacity());
    try testing.expectEqual(@as(usize, 193_482), l.splitPartCapacity());
    try testing.expectEqual(@as(usize, 193_482), l.mergePairCapacity());
    try testing.expectEqual(@as(usize, 64_494), l.mergeCandidateCapacity());
    try testing.expectEqual(@as(usize, 127_690), l.splitGrowthWindow());
    const small = try NodeLayout.new(MIN_NODE_SIZE, null);
    try testing.expectEqual(@as(usize, 16_640), small.recordValueCapOf());
    try testing.expectEqual(@as(usize, 61_368), small.foldCapacity());
    try testing.expectEqual(@as(usize, 46_026), small.splitPartCapacity());
    try testing.expectEqual(@as(usize, 15_342), small.mergeCandidateCapacity());
    // smo_extents_for_parts: 1 for a fit, parts + 2 past it.
    try testing.expectEqual(@as(u64, 1), l.smoExtentsForParts(l.foldCapacity(), 7));
    try testing.expectEqual(@as(u64, 4), l.smoExtentsForParts(l.foldCapacity() + 1, 2));
    // append_frame_len: 0 for nothing; one page for a small batch; the
    // frame + bset headers push a page-filling batch to the next page.
    try testing.expectEqual(@as(usize, 0), l.appendFrameLen(0));
    try testing.expectEqual(@as(usize, 4096), l.appendFrameLen(100));
    try testing.expectEqual(@as(usize, 8192), l.appendFrameLen(4096 - 72 + 1));
    try testing.expectEqual(@as(usize, 4096), l.appendFrameLen(4096 - 72));
    // Fanout (format v4): an interior record = 16 B header + 16 B value +
    // the separator key — 41 B over a 9 B inode key (the shortest real
    // separator of a slot tree) ⇒ 6,292 children at 256 KiB ⇒ 2.5×10¹¹
    // records at depth ≤ 3, inside the charter's "≈ 7,000 ⇒ 100 M at depth
    // ≤ 3" envelope (the v3 estimate was 36 B ⇒ 7,166; the 16 B header and
    // the kind byte on every key cost 2 B per child). A ref record frames
    // at 16 + 29 + 4 = 49 B (the reference measured 46 B).
    const interior_record_len = rec.RECORD_HEADER_LEN + key.InteriorValue.LEN + key.INODE_KEY_LEN;
    try testing.expectEqual(@as(usize, 41), interior_record_len);
    try testing.expectEqual(@as(usize, 6_292), l.foldCapacity() / interior_record_len);
    const fanout = l.foldCapacity() / interior_record_len;
    try testing.expect(fanout * fanout * fanout >= 100_000_000);
    try testing.expectEqual(@as(usize, 49), rec.RECORD_HEADER_LEN + key.BLOCK_REF_KEY_LEN + rec.BLOCK_REF_VALUE_LEN);
    // Interior values are exactly 16 B.
    const iv = key.encodeInteriorValue(0x840000, 7);
    try testing.expectEqual(@as(usize, 16), iv.len);
    const dec = try key.InteriorValue.decode(&iv);
    try testing.expectEqual(@as(u64, 0x840000), dec.childAddr());
    try testing.expectEqual(@as(u64, 7), dec.childSeq());
    try testing.expectError(error.Corrupt, key.InteriorValue.decode(iv[0..15]));
    // The named bounds with their WHY.
    try testing.expectEqual(@as(u32, 256), RETRY_BUDGET);
    try testing.expectEqual(@as(u32, 3), READER_LOAD_RETRIES);
    try testing.expectEqual(@as(u32, 8), PROJECTION_REFRESH_EVERY);
}

test "header page byte layout: KVND @0, version @4, tree_id @6, level @7, node_addr @8, node_seq @16, node_size @24, min/max len @28/@30, hash64 @32 over the field-zeroed page, keys packed at 40" {
    const l = defaultLayout();
    var page: [NODE_PAGE]u8 = undefined;
    try encodeHeaderPage(l, .{ .node_addr = 0x840000, .node_seq = 0x574beb7afdab4e6c, .tree_id = 1, .level = 0, .min_key = &.{}, .max_key = &key.KEY_SPACE_MAX }, &page, null);
    try testing.expectEqualStrings("KVND", page[0..4]);
    try testing.expectEqualSlices(u8, &.{ 1, 0 }, page[4..6]);
    try testing.expectEqual(@as(u8, 1), page[6]);
    try testing.expectEqual(@as(u8, 0), page[7]);
    try testing.expectEqual(@as(u64, 0x840000), std.mem.readInt(u64, page[8..16], .little));
    try testing.expectEqualSlices(u8, &.{ 0x6c, 0x4e, 0xab, 0xfd, 0x7a, 0xeb, 0x4b, 0x57 }, page[16..24]);
    try testing.expectEqual(@as(u32, 262144), std.mem.readInt(u32, page[24..28], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, page[28..30], .little));
    try testing.expectEqual(@as(u16, 32), std.mem.readInt(u16, page[30..32], .little));
    try testing.expect(std.mem.allEqual(u8, page[40..72], 0xFF));
    try testing.expect(std.mem.allEqual(u8, page[72..], 0));
    // The streaming recipe: [0..32) ‖ 8 zero bytes ‖ [40..4096).
    var h: checksum.Hasher = .init();
    h.update(page[0..32]);
    h.update(&@as([8]u8, @splat(0)));
    h.update(page[40..]);
    try testing.expectEqual(h.finalLow64(), std.mem.readInt(u64, page[32..40], .little));
    try testing.expectEqual(headerPageChecksum(&page), std.mem.readInt(u64, page[32..40], .little));
    const dec = try decodeHeaderPage(&page, null);
    try testing.expectEqual(@as(u64, 0x840000), dec.node_addr);
    try testing.expectEqual(@as(u8, 1), dec.tree_id);
    try testing.expectEqual(@as(usize, 0), dec.min_key.len);
    try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, dec.max_key);
    // Keys pack with no gap: min then max.
    try encodeHeaderPage(l, .{ .node_addr = 0, .node_seq = 1, .tree_id = 2, .level = 3, .min_key = "ab", .max_key = "xyz" }, &page, null);
    try testing.expectEqualStrings("abxyz", page[40..45]);
    const d2 = try decodeHeaderPage(&page, null);
    try testing.expectEqualStrings("ab", d2.min_key);
    try testing.expectEqualStrings("xyz", d2.max_key);
    try testing.expectEqual(@as(u8, 3), d2.level);
    // Encode refuses keys that overrun the page; the decoder refuses a
    // restamped liar; the order of the decode gates.
    var diag: Refusal = undefined;
    const big: [4000]u8 = @splat(1);
    try testing.expectError(error.Corrupt, encodeHeaderPage(l, .{ .node_addr = 0, .node_seq = 1, .tree_id = 1, .level = 0, .min_key = &big, .max_key = &big }, &page, &diag));
    try testing.expect(diag == .header_keys_too_large);
    try encodeHeaderPage(l, .{ .node_addr = 0, .node_seq = 1, .tree_id = 1, .level = 0, .min_key = "a", .max_key = "b" }, &page, null);
    try testing.expectError(error.Corrupt, decodeHeaderPage(page[0..4000], &diag));
    try testing.expect(diag == .header_bad_length);
    var bad = page;
    bad[0] ^= 0xFF;
    try testing.expectError(error.Corrupt, decodeHeaderPage(&bad, &diag));
    try testing.expect(diag == .bad_magic);
    bad = page;
    bad[4] = 2;
    try testing.expectError(error.Corrupt, decodeHeaderPage(&bad, &diag));
    try testing.expectEqual(@as(u16, 2), diag.unsupported_version);
    bad = page;
    bad[17] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decodeHeaderPage(&bad, &diag));
    bad = page;
    bad[4000] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decodeHeaderPage(&bad, &diag));
    bad = page;
    std.mem.writeInt(u16, bad[28..30], 4000, .little);
    std.mem.writeInt(u16, bad[30..32], 100, .little);
    checksum.stamp(&bad, NODE_HEADER_CHECKSUM_OFF);
    try testing.expectError(error.Corrupt, decodeHeaderPage(&bad, &diag));
    try testing.expect(diag == .header_keys_overrun);
    // A zeroed page fails on magic, never as a tear.
    const zero: [NODE_PAGE]u8 = @splat(0);
    try testing.expectError(error.Corrupt, decodeHeaderPage(&zero, &diag));
    try testing.expectEqual(@as(u32, 0), diag.bad_magic);
}

test "record value cap enforced with a typed error on both write paths; exactly-cap passes (64 KiB cap = 16,640)" {
    const gpa = testing.allocator;
    const small = try NodeLayout.new(MIN_NODE_SIZE, null);
    const cap = small.recordValueCapOf();
    try testing.expectEqual(@as(usize, 16_384 + 256), cap);
    const over_v = try gpa.alloc(u8, cap + 1);
    defer gpa.free(over_v);
    @memset(over_v, 0xAB);
    const over = RecordRef{ .key = &key.inodeKey(7), .seq = 1, .kind = .put, .value = over_v };
    const at_cap = RecordRef{ .key = &key.inodeKey(7), .seq = 1, .kind = .put, .value = over_v[0..cap] };
    var diag: Refusal = undefined;
    try testing.expectError(error.ValueTooLarge, writeNode(gpa, small, wp(0, 1, "", "\xff"), &.{over}, 1, &diag));
    try testing.expectEqual(cap + 1, diag.value_too_large.len);
    try testing.expectEqual(cap, diag.value_too_large.cap);
    var vol = try Volume.init(gpa, 4 * MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, small, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = hdr.bytesWritten() };
    try testing.expectError(error.ValueTooLarge, appendBset(gpa, small, dest, &.{over}, 2, &diag));
    const tail = try vol.append(gpa, small, &dest, &.{at_cap}, 2);
    try testing.expect(tail > NODE_PAGE);
    var w2 = try vol.writeNodeAt(gpa, small, wp(MIN_NODE_SIZE, 2, "", "\xff"), &.{at_cap}, 1);
    defer w2.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), w2.record_count);
}

test "write/load roundtrip preserves header and base records at a nonzero extent; tail == bytes_written; images are page-granular" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, 4 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    const addr: u64 = 2 * DEFAULT_NODE_SIZE;
    var recs = [_]Record{ try putIno(gpa, 10, 1), try putIno(gpa, 20, 2), try putIno(gpa, 30, 3) };
    defer rec.deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    const refs = refsOf(&recs, &rbuf);
    const images_before = meta_kv_node_images.load();
    var written = try vol.writeNodeAt(gpa, l, wp(addr, 42, &key.inodeKey(10), &key.inodeKey(30)), refs, 3);
    defer written.deinit(gpa);
    try testing.expectEqual(images_before + 1, meta_kv_node_images.load());
    try testing.expectEqual(addr, written.node_addr);
    try testing.expectEqual(@as(u64, 42), written.node_seq);
    try testing.expectEqual(@as(usize, 3), written.record_count);
    try testing.expectEqual(@as(u64, 3), written.journal_seq_horizon);
    try testing.expectEqual(@as(usize, 0), written.bytesWritten() % NODE_PAGE);
    try testing.expectEqual(@as(usize, 2 * NODE_PAGE), written.bytesWritten());
    try testing.expectEqualSlices(u8, &key.inodeKey(30), written.header().max_key);
    var node = try vol.load(gpa, l, addr, 0);
    defer node.deinit(gpa);
    try testing.expectEqual(addr, node.header.node_addr);
    try testing.expectEqual(@as(u64, 42), node.header.node_seq);
    try testing.expectEqual(key.TREE_INODES, node.header.tree_id);
    try testing.expectEqual(@as(u8, 0), node.header.level);
    try testing.expectEqual(@as(u32, DEFAULT_NODE_SIZE), node.header.node_size);
    try testing.expectEqualSlices(u8, &key.inodeKey(10), node.header.min_key);
    try testing.expectEqualSlices(u8, &key.inodeKey(30), node.header.max_key);
    try testing.expectEqual(@as(usize, 1), node.bsetCount());
    try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
    try testing.expectEqual(@as(u64, 0), node.foreign_frames_screened);
    try testing.expectEqual(written.bytesWritten(), node.tail_offset);
    var base = try node.bsetAt(gpa, 0);
    defer base.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), base.len());
    try testing.expectEqual(@as(u64, 3), base.journal_seq_horizon);
    for (0..3) |i| try testing.expect(base.record(i).eql(recs[i].ref()));
    try testing.expectEqual(AppendDest{ .node_addr = addr, .node_seq = 42, .tail_offset = 2 * NODE_PAGE }, node.appendDest());
    // The finding-41 guard: a base record outside the bounds is refused.
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, writeNode(gpa, l, wp(addr, 43, &key.inodeKey(11), &key.inodeKey(30)), refs, 3, &diag));
    try testing.expect(diag == .records_outside_bounds);
    try testing.expectError(error.Corrupt, writeNode(gpa, l, wp(addr, 43, &key.inodeKey(10), &key.inodeKey(29)), refs, 3, &diag));
    try testing.expectError(error.Corrupt, writeNode(gpa, l, wp(addr + 1, 43, "", "\xff"), refs, 3, &diag));
    try testing.expect(diag == .unaligned_address);
}

test "header-only node roundtrip: 0 bsets, tail 4096, lookup absent" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var w = try vol.writeNodeAt(gpa, l, wp(0, 7, "", "\xff"), &.{}, 0);
    defer w.deinit(gpa);
    try testing.expectEqual(@as(usize, NODE_PAGE), w.bytesWritten());
    try testing.expectEqual(@as(u64, 0), w.journal_seq_horizon);
    var node = try vol.load(gpa, l, 0, 0);
    defer node.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), node.bsetCount());
    try testing.expectEqual(@as(usize, NODE_PAGE), node.tail_offset);
    try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
    try testing.expectEqualStrings("", node.header.min_key);
    try testing.expectEqualStrings("\xff", node.header.max_key);
    var f = try node.lookup(gpa, &key.inodeKey(1));
    defer f.deinit(gpa);
    try testing.expectEqual(rec.Folded.absent, f);
}

test "append bsets then load folds LWW: a small append consumes exactly one page; 3 bsets in order; Δtime folds onto base; tombstone shadows; untouched key is zero-copy" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var base_r = [_]Record{ try putIno(gpa, 1, 1), try putIno(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &base_r);
    var rbuf: [8]RecordRef = undefined;
    var written = try vol.writeNodeAt(gpa, l, wp(0, 1, &key.inodeKey(1), &key.inodeKey(99)), refsOf(&base_r, &rbuf), 2);
    defer written.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = written.bytesWritten() };
    const appends_before = meta_kv_node_appends.load();
    const bytes_before = meta_kv_node_append_bytes.load();
    var a1 = [_]Record{ try Record.delta(gpa, &key.inodeKey(1), 5, InodeDelta.times(500, 501)), try putIno(gpa, 3, 4) };
    defer rec.deinitRecords(gpa, &a1);
    const tail0 = dest.tail_offset;
    const tail1 = try vol.append(gpa, l, &dest, refsOf(&a1, &rbuf), 5);
    try testing.expectEqual(@as(usize, 0), tail1 % NODE_PAGE);
    try testing.expectEqual(@as(usize, NODE_PAGE), tail1 - tail0);
    var a2 = [_]Record{try Record.delete(gpa, &key.inodeKey(2), 7)};
    defer rec.deinitRecords(gpa, &a2);
    const tail2 = try vol.append(gpa, l, &dest, refsOf(&a2, &rbuf), 7);
    try testing.expect(tail2 > tail1);
    try testing.expectEqual(appends_before + 2, meta_kv_node_appends.load());
    try testing.expectEqual(bytes_before + 2 * NODE_PAGE, meta_kv_node_append_bytes.load());
    var node = try vol.load(gpa, l, 0, 0);
    defer node.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), node.bsetCount());
    try testing.expectEqual(tail2, node.tail_offset);
    try testing.expectEqual(tail2, node.appendDest().tail_offset);
    var b1 = try node.bsetAt(gpa, 1);
    defer b1.deinit(gpa);
    try testing.expectEqual(@as(u64, 5), b1.journal_seq_horizon);
    var b2 = try node.bsetAt(gpa, 2);
    defer b2.deinit(gpa);
    try testing.expectEqual(@as(u64, 7), b2.journal_seq_horizon);
    var f1 = try node.lookup(gpa, &key.inodeKey(1));
    defer f1.deinit(gpa);
    try testing.expectEqual(@as(u64, 5), f1.put.seq);
    const v = try InodeValue.decode(f1.put.value);
    try testing.expectEqual(@as(u64, 500), v.mtime);
    try testing.expectEqual(@as(u64, 501), v.ctime);
    try testing.expectEqual(@as(u32, 1), v.uid);
    var f2 = try node.lookup(gpa, &key.inodeKey(2));
    defer f2.deinit(gpa);
    try testing.expectEqual(rec.Folded{ .tombstone = .{ .seq = 7 } }, f2);
    var f3 = try node.lookup(gpa, &key.inodeKey(3));
    defer f3.deinit(gpa);
    try testing.expectEqual(@as(u64, 4), f3.put.seq);
    try testing.expect(!f3.put.owned);
    // Zero-copy: the value points into the extent.
    const p = @intFromPtr(f3.put.value.ptr);
    try testing.expect(p >= @intFromPtr(vol.bytes.ptr) and p < @intFromPtr(vol.bytes.ptr) + vol.bytes.len);
    var f9 = try node.lookup(gpa, &key.inodeKey(9));
    defer f9.deinit(gpa);
    try testing.expectEqual(rec.Folded.absent, f9);
}

test "append on a full node returns NodeFull{needed > available} and keeps the node loadable: a 64 KiB node takes exactly 15 one-page appends; tail == 65,536; 0 drops" {
    const gpa = testing.allocator;
    const small = try NodeLayout.new(MIN_NODE_SIZE, null);
    var vol = try Volume.init(gpa, MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, small, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = NODE_PAGE };
    const value: [2048]u8 = @splat(0x5A);
    var appended: u64 = 0;
    var diag: Refusal = undefined;
    while (true) {
        const r = RecordRef{ .key = &key.inodeKey(appended), .seq = appended + 1, .kind = .put, .value = &value };
        _ = vol.append(gpa, small, &dest, &.{r}, appended + 1) catch |e| {
            try testing.expectEqual(error.NodeFull, e);
            break;
        };
        appended += 1;
        try testing.expect(appended < 64);
    }
    try testing.expectEqual(@as(u64, 15), appended);
    try testing.expectError(error.NodeFull, appendBset(gpa, small, dest, &.{.{ .key = &key.inodeKey(99), .seq = 99, .kind = .put, .value = &value }}, 99, &diag));
    try testing.expect(diag.node_full.needed > diag.node_full.available);
    try testing.expectEqual(@as(usize, 0), diag.node_full.available);
    var node = try vol.load(gpa, small, 0, 0);
    defer node.deinit(gpa);
    try testing.expectEqual(@as(usize, 15), node.bsetCount());
    try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
    try testing.expectEqual(MIN_NODE_SIZE, node.tail_offset);
    // Append preconditions: an unaligned, too-low or past-the-end tail.
    for ([_]usize{ 0, NODE_PAGE + 1, MIN_NODE_SIZE + NODE_PAGE }) |bad_tail| {
        try testing.expectError(error.Corrupt, appendBset(gpa, small, .{ .node_addr = 0, .node_seq = 1, .tail_offset = bad_tail }, &.{.{ .key = "k", .seq = 1, .kind = .put, .value = "v" }}, 1, &diag));
        try testing.expectEqual(bad_tail, diag.append_tail_offset);
    }
}

test "load detects header corruption and misdirection: a flipped header byte ⇒ ChecksumMismatch; a valid image copied elsewhere ⇒ self-address Corrupt; a zeroed extent ⇒ Corrupt (not a tear); node_size mismatch ⇒ Corrupt" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, 6 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    const addr: u64 = DEFAULT_NODE_SIZE;
    var recs = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &recs);
    var rbuf: [1]RecordRef = undefined;
    var w = try vol.writeNodeAt(gpa, l, wp(addr, 3, &key.inodeKey(1), &key.inodeKey(2)), refsOf(&recs, &rbuf), 1);
    defer w.deinit(gpa);
    const page_before = vol.bytes[addr..][0..NODE_PAGE].*;
    vol.bytes[addr + 17] ^= 0x01;
    var diag: Refusal = undefined;
    try testing.expectError(error.ChecksumMismatch, verifyNodeExtent(gpa, vol.extent(l, addr), l, addr, 0, &diag));
    vol.write(addr, &page_before);
    var ok = try vol.load(gpa, l, addr, 0);
    ok.deinit(gpa);
    const elsewhere: u64 = 3 * DEFAULT_NODE_SIZE;
    const image = try gpa.dupe(u8, vol.extent(l, addr));
    defer gpa.free(image);
    vol.write(elsewhere, image);
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.extent(l, elsewhere), l, elsewhere, 0, &diag));
    try testing.expectEqual(addr, diag.self_address_mismatch.header);
    try testing.expectEqual(elsewhere, diag.self_address_mismatch.read);
    var buf: [256]u8 = undefined;
    var wr: std.Io.Writer = .fixed(&buf);
    try diag.write(&wr);
    try testing.expectEqualStrings("node self-address mismatch: header says 0x40000, read from 0xc0000 (misdirected read/write)", wr.buffered());
    const zeroed: u64 = 5 * DEFAULT_NODE_SIZE;
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.extent(l, zeroed), l, zeroed, 0, &diag));
    try testing.expect(diag == .bad_magic);
    // The layout's node_size disagreeing with the header's.
    const sixty_four = try NodeLayout.new(MIN_NODE_SIZE, null);
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.bytes[addr..][0..MIN_NODE_SIZE], sixty_four, addr, 0, &diag));
    try testing.expectEqual(@as(u32, DEFAULT_NODE_SIZE), diag.node_size_mismatch.header);
    // A short/long buffer is refused before anything is decoded.
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.bytes[addr..][0 .. DEFAULT_NODE_SIZE - 1], l, addr, 0, &diag));
    try testing.expect(diag == .extent_length);
}

test "stale bsets from a recycled extent do not leak: a first life (seq 5, base + 2 appends) overwritten by a second life (seq 9, base only) loads 1 bset, 0 drops; a HIGHER stale stamp is equally silent" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var r1 = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &r1);
    var rbuf: [1]RecordRef = undefined;
    var first = try vol.writeNodeAt(gpa, l, wp(0, 5, "", "\xff"), refsOf(&r1, &rbuf), 1);
    defer first.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 5, .tail_offset = first.bytesWritten() };
    var r2 = [_]Record{try putIno(gpa, 2, 2)};
    defer rec.deinitRecords(gpa, &r2);
    _ = try vol.append(gpa, l, &dest, refsOf(&r2, &rbuf), 2);
    var r3 = [_]Record{try putIno(gpa, 3, 3)};
    defer rec.deinitRecords(gpa, &r3);
    _ = try vol.append(gpa, l, &dest, refsOf(&r3, &rbuf), 3);
    const dropped_before = meta_kv_node_dropped_tail_bsets.load();
    for ([_]u64{ 9, 4 }) |second_seq| {
        var r50 = [_]Record{try putIno(gpa, 50, 10)};
        defer rec.deinitRecords(gpa, &r50);
        var second = try vol.writeNodeAt(gpa, l, wp(0, second_seq, "", "\xff"), refsOf(&r50, &rbuf), 10);
        defer second.deinit(gpa);
        var node = try vol.load(gpa, l, 0, 0);
        defer node.deinit(gpa);
        try testing.expectEqual(second_seq, node.header.node_seq);
        try testing.expectEqual(@as(usize, 1), node.bsetCount());
        try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
        try testing.expectEqual(second.bytesWritten(), node.tail_offset);
        var f2 = try node.lookup(gpa, &key.inodeKey(2));
        defer f2.deinit(gpa);
        try testing.expectEqual(rec.Folded.absent, f2);
        var f50 = try node.lookup(gpa, &key.inodeKey(50));
        defer f50.deinit(gpa);
        try testing.expectEqual(@as(u64, 10), f50.put.seq);
    }
    try testing.expectEqual(dropped_before, meta_kv_node_dropped_tail_bsets.load());
    // The residue ceiling sees every verifying stamp whatever the incarnation.
    try testing.expectEqual(@as(u64, 5), residueSeqCeiling(vol.extent(l, 0), std.math.maxInt(u64)));
}

test "torn-tail classifier: a torn unit truncates the view and is counted (priors intact); a valid bset after a tear with horizon ≤ durable_tail fails loud; a geometry liar is Corrupt, not a tear" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var r1 = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &r1);
    var rbuf: [1]RecordRef = undefined;
    var base = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), refsOf(&r1, &rbuf), 1);
    defer base.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = base.bytesWritten() };
    var r2 = [_]Record{try putIno(gpa, 2, 12)};
    defer rec.deinitRecords(gpa, &r2);
    const t1 = dest.tail_offset;
    _ = try vol.append(gpa, l, &dest, refsOf(&r2, &rbuf), 12);
    var r3 = [_]Record{try putIno(gpa, 3, 20)};
    defer rec.deinitRecords(gpa, &r3);
    const t2 = dest.tail_offset;
    _ = try vol.append(gpa, l, &dest, refsOf(&r3, &rbuf), 20);
    // Tear the second frame's bset (frame header intact, records garbage):
    // the walk stops there; the third bset (horizon 20) is beyond it.
    const pristine = try gpa.dupe(u8, vol.bytes);
    defer gpa.free(pristine);
    vol.bytes[t1 + FRAME_LEN + 40] ^= 0xFF;
    const dropped_before = meta_kv_node_dropped_tail_bsets.load();
    {
        // durable_tail 15 < 20: the trailing bset is the replay window ⇒
        // dropped silently: the torn unit (1) + the verifying bset (1).
        var node = try vol.load(gpa, l, 0, 15);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 1), node.bsetCount());
        try testing.expectEqual(t1, node.tail_offset);
        try testing.expectEqual(@as(u64, 2), node.dropped_tail_bsets);
        try testing.expectEqual(dropped_before + 2, meta_kv_node_dropped_tail_bsets.load());
        var f1 = try node.lookup(gpa, &key.inodeKey(1));
        defer f1.deinit(gpa);
        try testing.expectEqual(@as(u64, 1), f1.put.seq);
    }
    {
        // durable_tail 20 ≥ 20: checkpoint-covered data after a tear ⇒ LOUD.
        var diag: Refusal = undefined;
        try testing.expectError(error.CheckpointCoveredBsetAfterTear, verifyNodeExtent(gpa, vol.extent(l, 0), l, 0, 20, &diag));
        try testing.expectEqual(t2, diag.checkpoint_covered.bset_offset);
        try testing.expectEqual(@as(u64, 20), diag.checkpoint_covered.horizon);
        try testing.expectEqual(@as(u64, 20), diag.checkpoint_covered.durable_tail);
    }
    // Tear the frame HEADER instead (magic intact, checksum broken ⇒
    // Garbage): same classes.
    @memcpy(vol.bytes, pristine);
    vol.bytes[t1 + 9] ^= 0x01;
    {
        var node = try vol.load(gpa, l, 0, 0);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 1), node.bsetCount());
        try testing.expectEqual(@as(u64, 2), node.dropped_tail_bsets);
    }
    // A tear that kept less than the magic is a clean end: the stride pass
    // still finds the verifying bset beyond it (dropped = 1, no torn unit).
    @memcpy(vol.bytes, pristine);
    vol.bytes[t1] ^= 0xFF;
    {
        var node = try vol.load(gpa, l, 0, 0);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 1), node.bsetCount());
        try testing.expectEqual(@as(u64, 1), node.dropped_tail_bsets);
    }
    // A verified frame lying about its geometry is Corrupt, never a tear.
    @memcpy(vol.bytes, pristine);
    std.mem.writeInt(u32, vol.bytes[t1 + 16 ..][0..4], DEFAULT_NODE_SIZE, .little);
    restampFrame(vol.bytes[t1..][0..FRAME_LEN]);
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.extent(l, 0), l, 0, 0, &diag));
    try testing.expectEqual(t1, diag.impossible_geometry.pos);
    @memcpy(vol.bytes, pristine);
    std.mem.writeInt(u32, vol.bytes[t1 + 20 ..][0..4], 31, .little);
    restampFrame(vol.bytes[t1..][0..FRAME_LEN]);
    try testing.expectError(error.Corrupt, verifyNodeExtent(gpa, vol.extent(l, 0), l, 0, 0, &diag));
    try testing.expect(diag == .impossible_geometry);
    // Untouched: everything loads, nothing dropped.
    @memcpy(vol.bytes, pristine);
    var clean = try vol.load(gpa, l, 0, 100);
    defer clean.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), clean.bsetCount());
    try testing.expectEqual(@as(u64, 0), clean.dropped_tail_bsets);
}

/// Restamp a doctored frame header: the frame's checksum covers `[0..32)`
/// (not the field-zeroed whole header), so the structural gates behind it
/// are reachable.
fn restampFrame(hdr: *[FRAME_LEN]u8) void {
    std.mem.writeInt(u64, hdr[bset.BSET_FRAME_CHECKSUM_OFF..][0..8], checksum.hash64(hdr[0..bset.BSET_FRAME_CHECKSUM_OFF]), .little);
}

/// A frame image with an explicit stamp, for the screen tests.
fn stampedFrame(gpa: Allocator, ns: usize, node_seq: u64, stamp: FrameStamp, ino: u64, seq: u64) ![]u8 {
    const v = inodeValue(seq).encode();
    const r = RecordRef{ .key = &key.inodeKey(ino), .seq = seq, .kind = .put, .value = &v };
    return bset.encodeBsetFrame(gpa, stamp, ns, node_seq, &.{r}, seq);
}

test "the frame screen: rule 3 alone without a plane ((0,0) priming law); rules 1→2→3→4 in order with a plane; a screened stop skips the torn-tail pass; breach + fenced trips the tripwire" {
    const gpa = testing.allocator;
    const l = try NodeLayout.new(MIN_NODE_SIZE, null);
    var vol = try Volume.init(gpa, MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    // Frames at g = 2, 2, then a (0,0) frame (written with no fence
    // installed), then another g = 2 frame beyond it.
    const stamps = [_]FrameStamp{ .{ .appender_id = 7, .g = 2 }, .{ .appender_id = 7, .g = 2 }, .{}, .{ .appender_id = 7, .g = 2 } };
    var tail: usize = NODE_PAGE;
    for (stamps, 0..) |s, i| {
        const f = try stampedFrame(gpa, MIN_NODE_SIZE, 1, s, i + 1, 10 + i);
        defer gpa.free(f);
        vol.write(tail, f);
        tail += f.len;
    }
    const screened_before = meta_kv_foreign_frames_screened.load();
    const dropped_before = meta_kv_node_dropped_tail_bsets.load();
    {
        // No plane: rule 3 trips on the (0,0) frame (g 0 < prev 2) and ENDS
        // the log there; no torn-tail pass (the frame beyond is not counted).
        var node = try vol.load(gpa, l, 0, 0);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 2), node.bsetCount());
        try testing.expectEqual(@as(usize, 3 * NODE_PAGE), node.tail_offset);
        try testing.expectEqual(@as(u64, 1), node.foreign_frames_screened);
        try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
        try testing.expectEqual(screened_before + 1, meta_kv_foreign_frames_screened.load());
        try testing.expectEqual(dropped_before, meta_kv_node_dropped_tail_bsets.load());
    }
    // A fresh log's first frame has prev_g = None: rule 3 cannot fire on it.
    {
        var vol2 = try Volume.init(gpa, MIN_NODE_SIZE);
        defer vol2.deinit(gpa);
        var h2 = try vol2.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), &.{}, 0);
        defer h2.deinit(gpa);
        const f = try stampedFrame(gpa, MIN_NODE_SIZE, 1, .{}, 1, 1);
        defer gpa.free(f);
        vol2.write(NODE_PAGE, f);
        var node = try vol2.load(gpa, l, 0, 0);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 1), node.bsetCount());
        try testing.expectEqual(@as(u64, 0), node.foreign_frames_screened);
    }
    // The rules in order with a plane.
    const sc: FrameScreen = .{ .g_current = 2, .appender_current = 7, .recorded_tail = .{ .g = 1, .tail = 8192 }, .pr_fenced = false };
    try testing.expectEqual(@as(?u8, 1), sc.foreignRule(.{ .appender_id = 7, .g = 3 }, 4096, null));
    try testing.expectEqual(@as(?u8, 2), sc.foreignRule(.{ .appender_id = 7, .g = 1 }, 8192, null));
    try testing.expectEqual(@as(?u8, null), sc.foreignRule(.{ .appender_id = 7, .g = 1 }, 4096, null));
    try testing.expectEqual(@as(?u8, 3), sc.foreignRule(.{ .appender_id = 7, .g = 1 }, 4096, 2));
    try testing.expectEqual(@as(?u8, 4), sc.foreignRule(.{ .appender_id = 8, .g = 2 }, 4096, 2));
    try testing.expectEqual(@as(?u8, null), sc.foreignRule(.{ .appender_id = 7, .g = 2 }, 4096, 2));
    try testing.expect(FrameScreen.isBreachRule(1) and FrameScreen.isBreachRule(4) and !FrameScreen.isBreachRule(2) and !FrameScreen.isBreachRule(3));
    try testing.expect(sc.isCurrentOwn(.{ .appender_id = 7, .g = 2 }));
    try testing.expect(!sc.isCurrentOwn(.{ .appender_id = 8, .g = 2 }));
    // Unleased: rule 4 inert, every current-g stamp is "own".
    const unleased: FrameScreen = .{ .g_current = 2, .appender_current = null, .recorded_tail = null, .pr_fenced = false };
    try testing.expectEqual(@as(?u8, null), unleased.foreignRule(.{ .appender_id = 8, .g = 2 }, 4096, 2));
    try testing.expect(unleased.isCurrentOwn(.{ .appender_id = 8, .g = 2 }));
    // With the plane over the same log: the (0,0) frame is screened (rule 2:
    // g 0 ≤ 1 at pos ≥ 8192, before rule 3), and the lessee's OWN g-2 frame
    // beyond it is the zombie-overwrite shape ⇒ Corrupt, not a hole.
    {
        var diag: Refusal = undefined;
        try testing.expectError(error.Corrupt, verifyNodeExtentScreened(gpa, vol.extent(l, 0), l, 0, 0, sc, &diag));
        try testing.expectEqual(@as(usize, 3 * NODE_PAGE), diag.zombie_overwrite.at);
        try testing.expectEqual(@as(usize, 4 * NODE_PAGE), diag.zombie_overwrite.probe);
        // The same log cut before the own frame: a plain screened stop.
        var cut = try Volume.init(gpa, MIN_NODE_SIZE);
        defer cut.deinit(gpa);
        @memcpy(cut.bytes[0 .. 4 * NODE_PAGE], vol.bytes[0 .. 4 * NODE_PAGE]);
        var node = try verifyNodeExtentScreened(gpa, cut.extent(l, 0), l, 0, 0, sc, null);
        defer node.deinit(gpa);
        try testing.expectEqual(@as(usize, 2), node.bsetCount());
        try testing.expectEqual(@as(u64, 1), node.foreign_frames_screened);
        try testing.expectEqual(@as(u64, 0), node.dropped_tail_bsets);
    }
    // Breach class under a device fence: a g above the current one at the
    // first slot trips rule 1 ⇒ the tripwire fires (one loud log).
    {
        var vol3 = try Volume.init(gpa, MIN_NODE_SIZE);
        defer vol3.deinit(gpa);
        var h3 = try vol3.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), &.{}, 0);
        defer h3.deinit(gpa);
        const f = try stampedFrame(gpa, MIN_NODE_SIZE, 1, .{ .appender_id = 7, .g = 5 }, 1, 1);
        defer gpa.free(f);
        vol3.write(NODE_PAGE, f);
        const fenced: FrameScreen = .{ .g_current = 2, .appender_current = 7, .recorded_tail = null, .pr_fenced = true };
        const tw_before = appender_fence_breach.load();
        log.testing.expectErrorLogs(1);
        var node = try verifyNodeExtentScreened(gpa, vol3.extent(l, 0), l, 0, 0, fenced, null);
        defer node.deinit(gpa);
        try log.testing.expectAllFired();
        try testing.expectEqual(@as(usize, 0), node.bsetCount());
        try testing.expectEqual(tw_before + 1, appender_fence_breach.load());
        // Not fenced: the same shape is screened, not a breach.
        var unfenced = fenced;
        unfenced.pr_fenced = false;
        var n2 = try verifyNodeExtentScreened(gpa, vol3.extent(l, 0), l, 0, 0, unfenced, null);
        defer n2.deinit(gpa);
        try testing.expectEqual(tw_before + 1, appender_fence_breach.load());
    }
}

test "screened-stop zombie-overwrite scan: a foreign frame BEFORE the lessee's own current-generation frame is foreign_frame_overwrite_detected ⇒ Corrupt" {
    const gpa = testing.allocator;
    const l = try NodeLayout.new(MIN_NODE_SIZE, null);
    var vol = try Volume.init(gpa, MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    // Own frame at g 3, then a stale lessee's g 2 frame (rule 3), then our
    // own g 3 frame again: the zombie overwrote the slot between two acked
    // frames.
    const stamps = [_]FrameStamp{ .{ .appender_id = 7, .g = 3 }, .{ .appender_id = 6, .g = 2 }, .{ .appender_id = 7, .g = 3 } };
    var tail: usize = NODE_PAGE;
    for (stamps, 0..) |s, i| {
        const f = try stampedFrame(gpa, MIN_NODE_SIZE, 1, s, i + 1, 10 + i);
        defer gpa.free(f);
        vol.write(tail, f);
        tail += f.len;
    }
    const sc: FrameScreen = .{ .g_current = 3, .appender_current = 7, .recorded_tail = null, .pr_fenced = false };
    const before = meta_kv_foreign_frame_overwrite_detected.load();
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, verifyNodeExtentScreened(gpa, vol.extent(l, 0), l, 0, 0, sc, &diag));
    try testing.expectEqual(before + 1, meta_kv_foreign_frame_overwrite_detected.load());
    try testing.expectEqual(@as(usize, 2 * NODE_PAGE), diag.zombie_overwrite.at);
    try testing.expectEqual(@as(usize, 3 * NODE_PAGE), diag.zombie_overwrite.probe);
    try testing.expectEqual(FrameStamp{ .appender_id = 6, .g = 2 }, diag.zombie_overwrite.stamp);
    // Without a plane there is no "own" notion: the log ends at the foreign
    // frame and the frame beyond is simply not this log's.
    var node = try vol.load(gpa, l, 0, 0);
    defer node.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), node.bsetCount());
    try testing.expectEqual(@as(u64, 1), node.foreign_frames_screened);
}

test "a frame with a foreign version or a non-zero reserved field is Garbage (a counted torn stop); the one-member forest's (0, 0) frames load everywhere" {
    const gpa = testing.allocator;
    const l = try NodeLayout.new(MIN_NODE_SIZE, null);
    var vol = try Volume.init(gpa, MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    const f = try stampedFrame(gpa, MIN_NODE_SIZE, 1, .{}, 1, 1);
    defer gpa.free(f);
    vol.write(NODE_PAGE, f);
    {
        var n = try vol.load(gpa, l, 0, 0);
        defer n.deinit(gpa);
        try testing.expectEqual(@as(usize, 1), n.bsetCount());
        try testing.expectEqual(@as(u64, 0), n.dropped_tail_bsets);
    }
    // A foreign frame version, restamped so only the version rule rejects.
    std.mem.writeInt(u16, vol.bytes[NODE_PAGE + 4 ..][0..2], 2, .little);
    restampFrame(vol.bytes[NODE_PAGE..][0..FRAME_LEN]);
    {
        var n = try vol.load(gpa, l, 0, 0);
        defer n.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), n.bsetCount());
        try testing.expectEqual(@as(u64, 1), n.dropped_tail_bsets);
    }
    // A non-zero reserved field, restamped: the same verified-liar class.
    vol.write(NODE_PAGE, f);
    std.mem.writeInt(u16, vol.bytes[NODE_PAGE + 6 ..][0..2], 1, .little);
    restampFrame(vol.bytes[NODE_PAGE..][0..FRAME_LEN]);
    {
        var n = try vol.load(gpa, l, 0, 0);
        defer n.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), n.bsetCount());
        try testing.expectEqual(@as(u64, 1), n.dropped_tail_bsets);
    }
}

test "residue ceiling: the header's seq and every verifying frame stamp at any 4 KiB slot below the minting space's ceiling; 0 for nothing" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    try testing.expectEqual(@as(u64, 0), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    try testing.expectEqual(@as(u64, 0), residueSeqCeiling(vol.bytes[0..100], std.math.maxInt(u64)));
    var hdr = try vol.writeNodeAt(gpa, l, wp(0, 40, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    try testing.expectEqual(@as(u64, 40), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    // Frames of other incarnations deep in the extent, under any stamp:
    // every verifying one counts; a frame with a broken checksum does not.
    var r = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &r);
    var rbuf: [1]RecordRef = undefined;
    const f1 = try bset.encodeBsetFrame(gpa, .{}, DEFAULT_NODE_SIZE, 77, refsOf(&r, &rbuf), 1);
    defer gpa.free(f1);
    vol.write(10 * NODE_PAGE, f1);
    try testing.expectEqual(@as(u64, 77), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    const f2 = try stampedFrame(gpa, DEFAULT_NODE_SIZE, 90, .{ .appender_id = 1, .g = 1 }, 2, 2);
    defer gpa.free(f2);
    vol.write(20 * NODE_PAGE, f2);
    try testing.expectEqual(@as(u64, 90), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    // A stamp at or above the ceiling is another space's: skipped, never
    // hiding the highest stamp below it (77, the header's 40 below 41).
    try testing.expectEqual(@as(u64, 77), residueSeqCeiling(vol.bytes, 90));
    try testing.expectEqual(@as(u64, 40), residueSeqCeiling(vol.bytes, 77));
    try testing.expectEqual(@as(u64, 0), residueSeqCeiling(vol.bytes, 40));
    vol.bytes[20 * NODE_PAGE + 9] ^= 1;
    try testing.expectEqual(@as(u64, 77), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    // The ceiling trusts only the checksum: a verifying frame of a foreign
    // VERSION still names its incarnation (residue is residue).
    std.mem.writeInt(u16, vol.bytes[10 * NODE_PAGE + 4 ..][0..2], 3, .little);
    restampFrame(vol.bytes[10 * NODE_PAGE ..][0..FRAME_LEN]);
    try testing.expectEqual(@as(u64, 77), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
    vol.bytes[10 * NODE_PAGE + 9] ^= 1;
    try testing.expectEqual(@as(u64, 40), residueSeqCeiling(vol.bytes, std.math.maxInt(u64)));
}

test "residue stamped by: an unwritten extent and a header-only image answer false for every appender; a verifying frame stamped by the appender anywhere in the extent answers true, a broken checksum or another appender's stamp does not" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    try testing.expect(!residueStampedBy(vol.bytes, 1));
    try testing.expect(!residueStampedBy(vol.bytes[0..100], 1));
    // The manager's structural image: header + a (0, 0)-stamped base frame.
    var r = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &r);
    var rbuf: [1]RecordRef = undefined;
    var hdr = try vol.writeNodeAt(gpa, l, wp(0, 40, "", "\xff"), refsOf(&r, &rbuf), 1);
    defer hdr.deinit(gpa);
    try testing.expect(residueStampedBy(vol.bytes, 0));
    try testing.expect(!residueStampedBy(vol.bytes, 1));
    // A dead lessee's frame deep in the extent (a superseded image's
    // residue): named whatever its incarnation; a broken checksum is not.
    const f = try stampedFrame(gpa, DEFAULT_NODE_SIZE, 90, .{ .appender_id = 1, .g = 7 }, 2, 2);
    defer gpa.free(f);
    vol.write(20 * NODE_PAGE, f);
    try testing.expect(residueStampedBy(vol.bytes, 1));
    try testing.expect(!residueStampedBy(vol.bytes, 2));
    vol.bytes[20 * NODE_PAGE + 9] ^= 1;
    try testing.expect(!residueStampedBy(vol.bytes, 1));
}

test "compact folds into a fresh extent, old untouched: in-place refused; durable_tail 6 elides the seq-5 tombstone, seq 8 survives; horizon 9" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    var vol = try Volume.init(gpa, 2 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    const dst_addr: u64 = DEFAULT_NODE_SIZE;
    var base_r = [_]Record{ try putIno(gpa, 1, 1), try putIno(gpa, 2, 2), try putIno(gpa, 3, 3) };
    defer rec.deinitRecords(gpa, &base_r);
    var rbuf: [8]RecordRef = undefined;
    var written = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), refsOf(&base_r, &rbuf), 3);
    defer written.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = written.bytesWritten() };
    var a1 = [_]Record{ try Record.delta(gpa, &key.inodeKey(1), 9, InodeDelta.times(90, 91)), try Record.delete(gpa, &key.inodeKey(2), 5) };
    defer rec.deinitRecords(gpa, &a1);
    _ = try vol.append(gpa, l, &dest, refsOf(&a1, &rbuf), 9);
    var a2 = [_]Record{try Record.delete(gpa, &key.inodeKey(3), 8)};
    defer rec.deinitRecords(gpa, &a2);
    _ = try vol.append(gpa, l, &dest, refsOf(&a2, &rbuf), 9);
    var src = try vol.load(gpa, l, 0, 0);
    defer src.deinit(gpa);
    const before = try gpa.dupe(u8, vol.extent(l, 0));
    defer gpa.free(before);
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, compactNode(gpa, l, &src, &.{}, 0, 2, 6, &diag));
    try testing.expectEqual(@as(u64, 0), diag.cow_violation);
    var compacted = try compactNode(gpa, l, &src, &.{}, dst_addr, 2, 6, null);
    defer compacted.deinit(gpa);
    vol.write(dst_addr, compacted.image);
    try testing.expectEqual(dst_addr, compacted.node_addr);
    try testing.expectEqual(@as(u64, 2), compacted.node_seq);
    try testing.expectEqual(@as(usize, 2), compacted.record_count);
    try testing.expectEqual(@as(u64, 9), compacted.journal_seq_horizon);
    var dst = try vol.load(gpa, l, dst_addr, 0);
    defer dst.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), dst.bsetCount());
    try testing.expectEqualSlices(u8, src.header.min_key, dst.header.min_key);
    try testing.expectEqualSlices(u8, src.header.max_key, dst.header.max_key);
    var base = try dst.bsetAt(gpa, 0);
    defer base.deinit(gpa);
    try testing.expectEqual(rec.RecordKind.put, base.record(0).kind);
    try testing.expectEqual(rec.RecordKind.delete, base.record(1).kind);
    try testing.expectEqual(@as(u64, 8), base.record(1).seq);
    var f1 = try dst.lookup(gpa, &key.inodeKey(1));
    defer f1.deinit(gpa);
    try testing.expectEqual(@as(u64, 9), f1.put.seq);
    const v = try InodeValue.decode(f1.put.value);
    try testing.expectEqual(@as(u64, 90), v.mtime);
    try testing.expectEqual(@as(u64, 91), v.ctime);
    var f2 = try dst.lookup(gpa, &key.inodeKey(2));
    defer f2.deinit(gpa);
    try testing.expectEqual(rec.Folded.absent, f2);
    // CoW: the source extent is byte-identical and still loads.
    try testing.expectEqualSlices(u8, before, vol.extent(l, 0));
    var again = try vol.load(gpa, l, 0, 0);
    defer again.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), again.bsetCount());
    // f(N): the fold's encoded bytes under the same rule.
    const fb = try foldBytes(gpa, &src, &.{}, 6);
    try testing.expectEqual(@as(usize, (16 + 9 + 68) + (16 + 9)), fb);
}

test "split partitions the key space into two fresh nodes: left == right and dst == src refused; folds concatenate; right.min == key_successor(left.max); bounds inclusive; < 2 records refused" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    const n: u64 = DEFAULT_NODE_SIZE;
    var vol = try Volume.init(gpa, 6 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var records: [100]Record = undefined;
    for (&records, 0..) |*r, i| r.* = try putIno(gpa, @as(u64, i) * 2, @as(u64, i) + 1);
    defer rec.deinitRecords(gpa, &records);
    const refs = try gpa.alloc(RecordRef, 100);
    defer gpa.free(refs);
    var written = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), refsOf(&records, refs), 100);
    defer written.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = written.bytesWritten() };
    var a = [_]Record{ try Record.put(gpa, &key.inodeKey(0), 200, &inodeValue(200).encode()), try Record.delete(gpa, &key.inodeKey(198), 201) };
    defer rec.deinitRecords(gpa, &a);
    var abuf: [2]RecordRef = undefined;
    _ = try vol.append(gpa, l, &dest, refsOf(&a, &abuf), 201);
    var src = try vol.load(gpa, l, 0, 0);
    defer src.deinit(gpa);
    const ld: SplitDest = .{ .node_addr = n, .node_seq = 2 };
    const rd: SplitDest = .{ .node_addr = 2 * n, .node_seq = 3 };
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, splitNode(gpa, l, &src, &.{}, ld, ld, 300, &diag));
    try testing.expect(diag == .split_same_destination);
    try testing.expectError(error.Corrupt, splitNode(gpa, l, &src, &.{}, .{ .node_addr = 0, .node_seq = 2 }, rd, 300, &diag));
    try testing.expect(diag == .cow_violation);
    var sr = try splitNode(gpa, l, &src, &.{}, ld, rd, 300, null);
    defer sr.deinit(gpa);
    vol.write(n, sr.left.image);
    vol.write(2 * n, sr.right.image);
    try testing.expect(sr.left.record_count >= 1 and sr.right.record_count >= 1);
    try testing.expectEqual(@as(usize, 99), sr.left.record_count + sr.right.record_count);
    try testing.expectEqual(@as(u64, 201), sr.left.journal_seq_horizon);
    try testing.expectEqual(@as(u64, 201), sr.right.journal_seq_horizon);
    var left = try vol.load(gpa, l, n, 0);
    defer left.deinit(gpa);
    var right = try vol.load(gpa, l, 2 * n, 0);
    defer right.deinit(gpa);
    try testing.expectEqual(@as(u64, 2), left.header.node_seq);
    try testing.expectEqual(@as(u64, 3), right.header.node_seq);
    var lb = try left.bsetAt(gpa, 0);
    defer lb.deinit(gpa);
    var rb = try right.bsetAt(gpa, 0);
    defer rb.deinit(gpa);
    var i: u64 = 0;
    for ([_]*const BsetView{ &lb, &rb }) |b| {
        for (0..b.len()) |j| {
            try testing.expectEqualSlices(u8, &key.inodeKey(i * 2), b.record(j).key);
            i += 1;
        }
    }
    try testing.expectEqual(@as(u64, 99), i);
    try testing.expectEqualSlices(u8, src.header.min_key, left.header.min_key);
    try testing.expectEqualSlices(u8, src.header.max_key, right.header.max_key);
    try testing.expectEqualSlices(u8, lb.record(lb.len() - 1).key, left.header.max_key);
    try testing.expectEqualSlices(u8, keySuccessor(left.header.max_key).slice(), right.header.min_key);
    try testing.expect(!key.lessThan(rb.record(0).key, right.header.min_key));
    try testing.expect(key.lessThan(left.header.max_key, right.header.min_key));
    try testing.expect(adjacentSiblings(&left, &right));
    var f0 = try left.lookup(gpa, &key.inodeKey(0));
    defer f0.deinit(gpa);
    try testing.expectEqual(@as(u64, 200), f0.put.seq);
    // Byte balance: each half holds roughly half the fold.
    try testing.expect(sr.left.record_count >= 45 and sr.left.record_count <= 55);
    // Fewer than two folded records cannot split.
    var one_r = [_]Record{try putIno(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &one_r);
    var obuf: [1]RecordRef = undefined;
    var one_w = try vol.writeNodeAt(gpa, l, wp(3 * n, 8, "", "\xff"), refsOf(&one_r, &obuf), 1);
    defer one_w.deinit(gpa);
    var one = try vol.load(gpa, l, 3 * n, 0);
    defer one.deinit(gpa);
    try testing.expectError(error.Corrupt, splitNode(gpa, l, &one, &.{}, .{ .node_addr = 4 * n, .node_seq = 9 }, .{ .node_addr = 5 * n, .node_seq = 10 }, 0, &diag));
    try testing.expectEqual(@as(usize, 1), diag.split_too_few);
}

test "compact overflow signals split: a full 64 KiB log + 8 × 4 KiB-value extra ⇒ NodeFull; the log alone compacts to 15; split absorbs 15 + 8" {
    const gpa = testing.allocator;
    const small = try NodeLayout.new(MIN_NODE_SIZE, null);
    var vol = try Volume.init(gpa, 4 * MIN_NODE_SIZE);
    defer vol.deinit(gpa);
    var hdr = try vol.writeNodeAt(gpa, small, wp(0, 1, "", "\xff"), &.{}, 0);
    defer hdr.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = 0, .node_seq = 1, .tail_offset = NODE_PAGE };
    const v2k: [2048]u8 = @splat(0x11);
    for (0..15) |i| {
        const r = RecordRef{ .key = &key.inodeKey(i), .seq = i + 1, .kind = .put, .value = &v2k };
        _ = try vol.append(gpa, small, &dest, &.{r}, i + 1);
    }
    var src = try vol.load(gpa, small, 0, 0);
    defer src.deinit(gpa);
    try testing.expectEqual(@as(usize, 15), src.bsetCount());
    try testing.expectEqual(MIN_NODE_SIZE, src.tail_offset);
    const v4k: [4096]u8 = @splat(0x22);
    var keys: [8][key.INODE_KEY_LEN]u8 = undefined;
    var extra: [8]RecordRef = undefined;
    for (&extra, 0..) |*e, i| {
        keys[i] = key.inodeKey(100 + i);
        e.* = .{ .key = &keys[i], .seq = 100 + i, .kind = .put, .value = &v4k };
    }
    var diag: Refusal = undefined;
    try testing.expectError(error.NodeFull, compactNode(gpa, small, &src, &extra, MIN_NODE_SIZE, 2, 0, &diag));
    try testing.expect(diag.node_full.needed > MIN_NODE_SIZE);
    var alone = try compactNode(gpa, small, &src, &.{}, MIN_NODE_SIZE, 2, 0, null);
    defer alone.deinit(gpa);
    try testing.expectEqual(@as(usize, 15), alone.record_count);
    try testing.expectEqual(@as(u64, 15), alone.journal_seq_horizon);
    var sr = try splitNode(gpa, small, &src, &extra, .{ .node_addr = 2 * MIN_NODE_SIZE, .node_seq = 3 }, .{ .node_addr = 3 * MIN_NODE_SIZE, .node_seq = 4 }, 0, null);
    defer sr.deinit(gpa);
    try testing.expectEqual(@as(usize, 15 + 8), sr.left.record_count + sr.right.record_count);
    // The extra records are the NEWEST source: the horizon rises to them.
    try testing.expectEqual(@as(u64, 107), sr.left.journal_seq_horizon);
}

fn f41LayoutPut(gpa: Allocator, k: []const u8, seq: u64, blocks: u32) !Record {
    var layout: rec.Layout = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .block_map = .empty };
    defer layout.deinit(gpa);
    var name_buf: [32]u8 = undefined;
    for (0..blocks) |b| try layout.insert(gpa, @intCast(b), try std.fmt.bufPrint(&name_buf, "be://data/block-{d:0>8}", .{b}));
    const enc = try layout.encodeAlloc(gpa);
    defer gpa.free(enc);
    const value = try (rec.XattrValue{ .name = "layout", .value = enc }).encodeAlloc(gpa);
    defer gpa.free(value);
    return Record.put(gpa, k, seq, value);
}

fn f41LayoutLink(gpa: Allocator, k: []const u8, seq: u64, block: u32, base_version: u64, version: u64) !Record {
    var d: rec.LayoutDelta = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .entries = &.{.{ .block = block, .key = "be://data:tail" }} };
    d.setVersions(base_version, version);
    const value = try d.encodeAlloc(gpa);
    defer gpa.free(value);
    return .{ .key = try gpa.dupe(u8, k), .seq = seq, .kind = .delta, .value = try gpa.dupe(u8, value) };
}

test "finding 41: a split never cuts a same-key group (a versioned layout chain's folded Put + retained link stay on ONE side within its bounds); all-one-key refuses" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    const n: u64 = DEFAULT_NODE_SIZE;
    var vol = try Volume.init(gpa, 3 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    // A small, B's folded Put large (a near-crossing inline layout), B's
    // retained link small: the byte-balanced cut lands right after B.put.
    var records = [_]Record{
        try f41LayoutPut(gpa, "xattr-layout-a", 1, 1),
        try f41LayoutPut(gpa, "xattr-layout-b", 2, 1200),
        try f41LayoutLink(gpa, "xattr-layout-b", 3, 1200, 0, 0x51),
    };
    defer rec.deinitRecords(gpa, &records);
    var rbuf: [3]RecordRef = undefined;
    var written = try vol.writeNodeAt(gpa, l, wp(0, 1, "", "\xff"), refsOf(&records, &rbuf), 3);
    defer written.deinit(gpa);
    var src = try vol.load(gpa, l, 0, 0);
    defer src.deinit(gpa);
    // The fold keeps both of B's records, and the raw byte cut would land
    // between them; cohesion moves it.
    var folded = try foldNodeSources(gpa, &.{&src}, &.{&.{}}, 0);
    defer folded.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), folded.records.items.len);
    try testing.expectEqual(@as(?usize, 1), splitCut(folded.records.items));
    var sr = try splitNode(gpa, l, &src, &.{}, .{ .node_addr = n, .node_seq = 2 }, .{ .node_addr = 2 * n, .node_seq = 3 }, 0, null);
    defer sr.deinit(gpa);
    vol.write(n, sr.left.image);
    vol.write(2 * n, sr.right.image);
    try testing.expect(sr.left.record_count >= 1 and sr.right.record_count >= 1);
    try testing.expectEqual(@as(usize, 1), sr.left.record_count);
    try testing.expectEqual(@as(usize, 2), sr.right.record_count);
    for ([_]u64{ n, 2 * n }) |addr| {
        var node = try vol.load(gpa, l, addr, 0);
        defer node.deinit(gpa);
        var b = try node.bsetAt(gpa, 0);
        defer b.deinit(gpa);
        for (0..b.len()) |i| {
            const k = b.record(i).key;
            try testing.expect(!key.lessThan(k, node.header.min_key) and !key.lessThan(node.header.max_key, k));
        }
    }
    // The lineage survives whole: B folds to a live value on its side.
    var right = try vol.load(gpa, l, 2 * n, 0);
    defer right.deinit(gpa);
    var fb = try right.lookup(gpa, "xattr-layout-b");
    defer fb.deinit(gpa);
    try testing.expect(fb == .put);
    // All records on one key: no legal cut.
    var same = [_]Record{
        try f41LayoutPut(gpa, "only-key", 1, 1),
        try f41LayoutLink(gpa, "only-key", 2, 1, 0, 0x10),
    };
    defer rec.deinitRecords(gpa, &same);
    try testing.expectEqual(@as(?usize, null), splitCut(&same));
    // Forward cohesion when the cut lands mid-group with a key beyond it.
    var fwd = [_]Record{
        try putIno(gpa, 1, 1),
        try Record.delta(gpa, &key.inodeKey(1), 2, InodeDelta.times(1, 1)),
        try Record.delta(gpa, &key.inodeKey(1), 3, InodeDelta.times(2, 2)),
        try putIno(gpa, 2, 4),
    };
    defer rec.deinitRecords(gpa, &fwd);
    try testing.expectEqual(@as(?usize, 3), splitCut(&fwd));
    // Backward cohesion when every key beyond the cut is the trailing group.
    var back = [_]Record{
        try putIno(gpa, 1, 1),
        try putIno(gpa, 2, 2),
        try Record.delta(gpa, &key.inodeKey(2), 3, InodeDelta.times(1, 1)),
        try Record.delta(gpa, &key.inodeKey(2), 4, InodeDelta.times(2, 2)),
        try Record.delta(gpa, &key.inodeKey(2), 5, InodeDelta.times(3, 3)),
    };
    defer rec.deinitRecords(gpa, &back);
    try testing.expectEqual(@as(?usize, 1), splitCut(&back));
}

test "merge: f(L)+f(R) ≤ ¾C ∧ min ≤ ¼C; adjacency required; the successor spans [L.min, R.max] with one fold; parent records Put(R.max → succ) + Delete(L.max)" {
    const gpa = testing.allocator;
    const l = defaultLayout();
    const n: u64 = DEFAULT_NODE_SIZE;
    // The rule's arithmetic.
    try testing.expect(mergeable(l, 60_000, 100_000));
    try testing.expect(!mergeable(l, 100_000, 100_000)); // both above ¼C
    try testing.expect(!mergeable(l, 10_000, 190_000)); // sum above ¾C
    try testing.expect(mergeable(l, 64_494, 128_988));
    try testing.expect(!mergeable(l, 64_495, 128_987));
    // A fresh half (> ½C) never pairs with an underfull node.
    try testing.expect(!mergeable(l, l.foldCapacity() / 2 + 1, l.mergeCandidateCapacity()));
    var vol = try Volume.init(gpa, 4 * DEFAULT_NODE_SIZE);
    defer vol.deinit(gpa);
    var lr = [_]Record{ try putIno(gpa, 1, 1), try putIno(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &lr);
    var rr = [_]Record{ try putIno(gpa, 5, 3), try putIno(gpa, 6, 4) };
    defer rec.deinitRecords(gpa, &rr);
    var rbuf: [2]RecordRef = undefined;
    const left_max = key.inodeKey(3);
    var lw = try vol.writeNodeAt(gpa, l, wp(0, 1, "", &left_max), refsOf(&lr, &rbuf), 2);
    defer lw.deinit(gpa);
    const right_min = keySuccessor(&left_max);
    var rw = try vol.writeNodeAt(gpa, l, wp(n, 2, right_min.slice(), "\xff"), refsOf(&rr, &rbuf), 4);
    defer rw.deinit(gpa);
    var dest: AppendDest = .{ .node_addr = n, .node_seq = 2, .tail_offset = rw.bytesWritten() };
    var del = [_]Record{try Record.delete(gpa, &key.inodeKey(5), 7)};
    defer rec.deinitRecords(gpa, &del);
    _ = try vol.append(gpa, l, &dest, refsOf(&del, &rbuf), 7);
    var left = try vol.load(gpa, l, 0, 0);
    defer left.deinit(gpa);
    var right = try vol.load(gpa, l, n, 0);
    defer right.deinit(gpa);
    try testing.expect(adjacentSiblings(&left, &right));
    try testing.expect(!adjacentSiblings(&right, &left));
    const fl = try foldBytes(gpa, &left, &.{}, 0);
    const fr = try foldBytes(gpa, &right, &.{}, 8);
    try testing.expect(mergeable(l, fl, fr));
    var diag: Refusal = undefined;
    try testing.expectError(error.Corrupt, mergeNodes(gpa, l, &right, &left, &.{}, &.{}, 2 * n, 3, 8, &diag));
    try testing.expect(diag == .merge_not_adjacent);
    try testing.expectError(error.Corrupt, mergeNodes(gpa, l, &left, &right, &.{}, &.{}, 0, 3, 8, &diag));
    try testing.expect(diag == .cow_violation);
    // An extra delta on the left folds in; the right's tombstone (seq 7 <
    // tail 8) elides.
    var xl = [_]Record{try Record.delta(gpa, &key.inodeKey(1), 9, InodeDelta.times(9, 9))};
    defer rec.deinitRecords(gpa, &xl);
    var xbuf: [1]RecordRef = undefined;
    var succ = try mergeNodes(gpa, l, &left, &right, refsOf(&xl, &xbuf), &.{}, 2 * n, 3, 8, null);
    defer succ.deinit(gpa);
    vol.write(2 * n, succ.image);
    try testing.expectEqual(@as(usize, 3), succ.record_count);
    try testing.expectEqual(@as(u64, 9), succ.journal_seq_horizon);
    var s = try vol.load(gpa, l, 2 * n, 0);
    defer s.deinit(gpa);
    try testing.expectEqualStrings("", s.header.min_key);
    try testing.expectEqualStrings("\xff", s.header.max_key);
    try testing.expectEqual(@as(u8, 0), s.header.level);
    var f1 = try s.lookup(gpa, &key.inodeKey(1));
    defer f1.deinit(gpa);
    try testing.expectEqual(@as(u64, 9), f1.put.seq);
    var f5 = try s.lookup(gpa, &key.inodeKey(5));
    defer f5.deinit(gpa);
    try testing.expectEqual(rec.Folded.absent, f5);
    var f6 = try s.lookup(gpa, &key.inodeKey(6));
    defer f6.deinit(gpa);
    try testing.expectEqual(@as(u64, 4), f6.put.seq);
    var parent = try mergeParentRecords(gpa, left.header.max_key, right.header.max_key, 2 * n, 3, 50);
    defer rec.deinitRecords(gpa, &parent);
    try testing.expectEqual(rec.RecordKind.put, parent[0].kind);
    try testing.expectEqualStrings("\xff", parent[0].key);
    const iv = try key.InteriorValue.decode(parent[0].value);
    try testing.expectEqual(2 * n, iv.childAddr());
    try testing.expectEqual(@as(u64, 3), iv.childSeq());
    try testing.expectEqual(rec.RecordKind.delete, parent[1].kind);
    try testing.expectEqualSlices(u8, &left_max, parent[1].key);
    try testing.expectEqual(@as(u64, 50), parent[1].seq);
}

fn headerPageFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [NODE_PAGE + 8]u8 = undefined;
    const len = smith.slice(&buf);
    var diag: Refusal = undefined;
    _ = decodeHeaderPage(buf[0..len], &diag) catch {};
    if (len >= NODE_PAGE) {
        const page = buf[0..NODE_PAGE];
        checksum.stamp(page, NODE_HEADER_CHECKSUM_OFF);
        if (decodeHeaderPage(page, &diag)) |h| {
            // What decodes re-encodes: the header is the whole page's authority.
            var re: [NODE_PAGE]u8 = undefined;
            const layout: NodeLayout = .{ .node_size = h.node_size };
            encodeHeaderPage(layout, .{ .node_addr = h.node_addr, .node_seq = h.node_seq, .tree_id = h.tree_id, .level = h.level, .min_key = h.min_key, .max_key = h.max_key }, &re, &diag) catch return;
            const back = try decodeHeaderPage(&re, &diag);
            try std.testing.expectEqual(h.node_addr, back.node_addr);
            try std.testing.expectEqual(h.node_seq, back.node_seq);
            try std.testing.expectEqualSlices(u8, h.min_key, back.min_key);
            try std.testing.expectEqualSlices(u8, h.max_key, back.max_key);
        } else |_| {}
    }
}

test "fuzz: the node header page decoder is total on arbitrary and restamped pages; a decoded header survives encode∘decode" {
    try fuzzkit.fuzz({}, headerPageFuzz, .{});
}
