//! The offline image builder (charter §5.1; `docs/porting/kv_alloc_superblock.md`
//! §3): an in-memory volume DESCRIPTION (dirs / files / hard links /
//! xattrs) → key-sorted seq-0 records of the native slot tree → bottom-up
//! ¾-fill packed node images through the K2 node layer. PORTABLE: images
//! in, images out; `format.zig` lays them into a volume plan.
//!
//! Determinism: identical descriptions (including `hash_seed` and `uuid`)
//! build byte-identical images — inos are assigned monotonically from 2 in
//! add order, record seqs are 0 (checkpoint-covered by construction), node
//! seqs count up in a fixed tree-then-level order from the uuid-namespaced
//! base, extents claim lowest-first, `coll_seq`s follow name order within a
//! `(parent, hash)` group, timestamps default to 0.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const bset = @import("bset.zig");
const node = @import("node.zig");
const alloc_ext = @import("alloc_ext.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Record = rec.Record;
const RecordRef = rec.RecordRef;
const InodeValue = rec.InodeValue;
const NodeLayout = node.NodeLayout;

/// ino 0 is reserved, 1 is the root; `next_ino` starts above the built
/// population.
pub const ROOT_INO: u64 = 1;
/// Per-mount guard records the digest walk excludes: unique to every mount
/// by design, never user-visible content.
pub const WRITER_CLAIM_XATTR = "writer_claim";
pub const WRITER_TERM_XATTR = "writer_term";

/// The wire's `mode` bits (identical on Linux and macOS, and on-disk
/// constants regardless of the host).
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const PERM_MASK: u32 = 0o7777;

/// `d_type`-style byte for a mode's `S_IFMT` bits (the dentry value's u8
/// field); the read side shifts back to `mode & S_IFMT`.
pub fn dtOf(mode: u32) u8 {
    return @intCast((mode & S_IFMT) >> 12);
}

/// Format-time identity + geometry knobs for one built image.
pub const BuilderConfig = struct {
    node_size: usize,
    /// `--meta-journal-mb` in bytes; null = the clamp.
    journal_len_override: ?u64,
    /// The set's independent random seed (`geometry.SetIdentity`); fixed
    /// in tests.
    hash_seed: u64,
    /// Per-volume, random in production.
    uuid: [16]u8,
};

pub const DescribeError = error{
    EmptyName,
    NameTooLong,
    NoSuchParent,
    ParentNotDirectory,
    DuplicateName,
    NoSuchIno,
    DirectoryHardLink,
    /// `1 + name.len + value.len > recordValueCap(node_size)`.
    ValueTooLarge,
    /// A 257th same-`(parent, hash54)` name.
    DentryChainOverflow,
    /// A 257th same-`(ino, hash56)` xattr name.
    XattrChainOverflow,
    OutOfMemory,
};

const Dentry = struct { child: u64, dt: u8 };
const Dir = std.StringArrayHashMapUnmanaged(Dentry);
const Xattr = struct { ino: u64, name: []u8, value: []u8 };

/// The in-memory volume description. Population methods refuse duplicate
/// names, missing/non-directory parents, oversized names and values with
/// typed errors — a description error surfaces before any byte is built.
pub const ImageBuilder = struct {
    gpa: Allocator,
    cfg: BuilderConfig,
    /// `cfg.hash_seed` expanded once per build.
    name_key: key.NameKey,
    layout: NodeLayout,
    /// Insertion order == ino order (inos are minted monotonically).
    inodes: std.AutoArrayHashMapUnmanaged(u64, InodeValue),
    /// Per-directory `name → (child, dt)`; names are owned.
    dentries: std.AutoArrayHashMapUnmanaged(u64, Dir),
    xattrs: std.ArrayList(Xattr),
    next_ino: u64,

    /// A description holding only the root directory (ino 1, `S_IFDIR |
    /// 0o755`, 0:0, nlink 2 — the live law: a directory's nlink is 2 +
    /// subdirectories; the historical 1 sat one below it and fired the
    /// `dir_nlink_underflows` tripwire on every first full `rm -rf`).
    pub fn init(gpa: Allocator, cfg: BuilderConfig) (node.Error || Allocator.Error)!ImageBuilder {
        const layout = try NodeLayout.new(cfg.node_size, null);
        var self: ImageBuilder = .{ .gpa = gpa, .cfg = cfg, .name_key = .init(cfg.hash_seed), .layout = layout, .inodes = .empty, .dentries = .empty, .xattrs = .empty, .next_ino = ROOT_INO + 1 };
        errdefer self.deinit();
        try self.inodes.put(gpa, ROOT_INO, .{ .mode = S_IFDIR | 0o755, .nlink = 2 });
        try self.dentries.put(gpa, ROOT_INO, .empty);
        return self;
    }

    pub fn deinit(self: *ImageBuilder) void {
        self.inodes.deinit(self.gpa);
        for (self.dentries.values()) |*dir| {
            for (dir.keys()) |name| self.gpa.free(name);
            dir.deinit(self.gpa);
        }
        self.dentries.deinit(self.gpa);
        for (self.xattrs.items) |x| {
            self.gpa.free(x.name);
            self.gpa.free(x.value);
        }
        self.xattrs.deinit(self.gpa);
        self.* = undefined;
    }

    fn checkName(name: []const u8) DescribeError!void {
        if (name.len == 0) return error.EmptyName;
        if (name.len > rec.NAME_MAX) return error.NameTooLong;
    }

    /// Bind `name` under `parent` to `(child, dt)` with the shared checks.
    fn bind(self: *ImageBuilder, parent: u64, name: []const u8, child: u64, dt: u8) DescribeError!void {
        try checkName(name);
        const pv = self.inodes.get(parent) orelse return error.NoSuchParent;
        if (pv.mode & S_IFMT != S_IFDIR) return error.ParentNotDirectory;
        const dir = self.dentries.getPtr(parent) orelse unreachable;
        if (dir.contains(name)) return error.DuplicateName;
        const owned = try self.gpa.dupe(u8, name);
        errdefer self.gpa.free(owned);
        try dir.put(self.gpa, owned, .{ .child = child, .dt = dt });
    }

    fn addInode(self: *ImageBuilder, value: InodeValue) Allocator.Error!u64 {
        const ino = self.next_ino;
        try self.inodes.put(self.gpa, ino, value);
        self.next_ino += 1;
        return ino;
    }

    /// Add a directory under `parent` (child nlink 2; the parent gains one
    /// for the child's `..`); returns its ino.
    pub fn addDir(self: *ImageBuilder, parent: u64, name: []const u8, perm: u32, uid: u32, gid: u32) DescribeError!u64 {
        const mode = S_IFDIR | (perm & PERM_MASK);
        const ino = self.next_ino;
        try self.bind(parent, name, ino, dtOf(mode));
        try self.dentries.ensureUnusedCapacity(self.gpa, 1);
        const minted = try self.addInode(.{ .mode = mode, .uid = uid, .gid = gid, .nlink = 2 });
        assert(minted == ino);
        self.dentries.putAssumeCapacity(ino, .empty);
        self.inodes.getPtr(parent).?.nlink += 1;
        return ino;
    }

    /// Add a regular file under `parent` (nlink 1); returns its ino.
    pub fn addFile(self: *ImageBuilder, parent: u64, name: []const u8, perm: u32, uid: u32, gid: u32, size: u64) DescribeError!u64 {
        const mode = S_IFREG | (perm & PERM_MASK);
        const ino = self.next_ino;
        try self.bind(parent, name, ino, dtOf(mode));
        const minted = try self.addInode(.{ .mode = mode, .uid = uid, .gid = gid, .nlink = 1, .size = size });
        assert(minted == ino);
        return ino;
    }

    /// One more name for an existing non-directory ino (`nlink` maintained).
    pub fn addLink(self: *ImageBuilder, ino: u64, parent: u64, name: []const u8) DescribeError!void {
        const target = self.inodes.getPtr(ino) orelse return error.NoSuchIno;
        if (target.mode & S_IFMT == S_IFDIR) return error.DirectoryHardLink;
        try self.bind(parent, name, ino, dtOf(target.mode));
        target.nlink += 1;
    }

    /// Set (or replace) one xattr on `ino`. The record value is the encoded
    /// `XattrValue`; the per-volume cap is enforced here so the error
    /// carries the caller's context.
    pub fn setXattr(self: *ImageBuilder, ino: u64, name: []const u8, value: []const u8) DescribeError!void {
        try checkName(name);
        if (!self.inodes.contains(ino)) return error.NoSuchIno;
        if (1 + name.len + value.len > self.layout.recordValueCapOf()) return error.ValueTooLarge;
        const owned_value = try self.gpa.dupe(u8, value);
        errdefer self.gpa.free(owned_value);
        for (self.xattrs.items) |*x| {
            if (x.ino == ino and std.mem.eql(u8, x.name, name)) {
                self.gpa.free(x.value);
                x.value = owned_value;
                return;
            }
        }
        const owned_name = try self.gpa.dupe(u8, name);
        errdefer self.gpa.free(owned_name);
        try self.xattrs.append(self.gpa, .{ .ino = ino, .name = owned_name, .value = owned_value });
    }

    /// Override an ino's timestamps (ns); the default is 0.
    pub fn setTimes(self: *ImageBuilder, ino: u64, atime: u64, mtime: u64, ctime: u64) DescribeError!void {
        const v = self.inodes.getPtr(ino) orelse return error.NoSuchIno;
        v.atime = atime;
        v.mtime = mtime;
        v.ctime = ctime;
    }

    /// The `format` root-stamping surface (the root belongs to the invoking
    /// user); the builder default stays 0:0.
    pub fn setOwner(self: *ImageBuilder, ino: u64, uid: u32, gid: u32) DescribeError!void {
        const v = self.inodes.getPtr(ino) orelse return error.NoSuchIno;
        v.uid = uid;
        v.gid = gid;
    }

    pub fn inodeCount(self: *const ImageBuilder) u64 {
        return self.inodes.count();
    }

    pub fn nextIno(self: *const ImageBuilder) u64 {
        return self.next_ino;
    }

    pub fn inode(self: *const ImageBuilder, ino: u64) ?InodeValue {
        return self.inodes.get(ino);
    }

    // -----------------------------------------------------------------
    // Record assembly (key-sorted, seq 0, checkpoint-covered).
    // -----------------------------------------------------------------

    /// Inode records in ino order (== big-endian key order).
    pub fn inodeRecords(self: *const ImageBuilder, gpa: Allocator) Allocator.Error![]Record {
        var out: std.ArrayList(Record) = .empty;
        errdefer freeRecords(gpa, &out);
        var it = self.inodes.iterator();
        while (it.next()) |e| {
            try out.append(gpa, try Record.put(gpa, &key.inodeKey(e.key_ptr.*), 0, &e.value_ptr.encode()));
        }
        return out.toOwnedSlice(gpa);
    }

    /// Dentry records: per parent, names sorted, same-hash names take
    /// `coll_seq` 0, 1, … in that order; the whole set is then memcmp-
    /// sorted by key (parent-major but hash-shuffled within a directory).
    pub fn dentryRecords(self: *const ImageBuilder, gpa: Allocator) DescribeError![]Record {
        var out: std.ArrayList(Record) = .empty;
        errdefer freeRecords(gpa, &out);
        var groups: std.AutoHashMapUnmanaged(u64, u16) = .empty;
        defer groups.deinit(gpa);
        var dit = self.dentries.iterator();
        while (dit.next()) |de| {
            const parent = de.key_ptr.*;
            const dir = de.value_ptr;
            const names = try gpa.dupe([]const u8, dir.keys());
            defer gpa.free(names);
            std.mem.sort([]const u8, names, {}, nameLess);
            groups.clearRetainingCapacity();
            for (names) |name| {
                const d = dir.get(name).?;
                const hash = key.dentryNameHash54(name, &self.name_key);
                const coll = (try nextInChain(u64, gpa, &groups, hash)) orelse {
                    key.stats.meta_kv_dentry_collision_overflows.inc();
                    return error.DentryChainOverflow;
                };
                const value = (rec.DentryValue{ .child_ino = d.child, .file_type = d.dt, .name = name }).encodeAlloc(gpa) catch |e| switch (e) {
                    error.OutOfMemory => return error.OutOfMemory,
                    // `bind` refused every name past NAME_MAX.
                    error.NameTooLong => unreachable,
                };
                defer gpa.free(value);
                try out.append(gpa, try Record.put(gpa, &key.dentryKey(parent, hash, coll), 0, value));
            }
        }
        std.mem.sort(Record, out.items, {}, recordKeyLess);
        return out.toOwnedSlice(gpa);
    }

    /// Xattr records: `(ino, name)`-sorted iteration assigns `coll` within
    /// `(ino, hash56)`; then memcmp-sorted by key.
    pub fn xattrRecords(self: *const ImageBuilder, gpa: Allocator) DescribeError![]Record {
        var out: std.ArrayList(Record) = .empty;
        errdefer freeRecords(gpa, &out);
        const sorted = try gpa.dupe(Xattr, self.xattrs.items);
        defer gpa.free(sorted);
        std.mem.sort(Xattr, sorted, {}, xattrLess);
        var groups: std.AutoHashMapUnmanaged([2]u64, u16) = .empty;
        defer groups.deinit(gpa);
        for (sorted) |x| {
            const hash = key.xattrNameHash56(x.name, &self.name_key);
            const coll = (try nextInChain([2]u64, gpa, &groups, .{ x.ino, hash })) orelse return error.XattrChainOverflow;
            const value = (rec.XattrValue{ .name = x.name, .value = x.value }).encodeAlloc(gpa) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.NameTooLong => unreachable,
            };
            defer gpa.free(value);
            try out.append(gpa, try Record.put(gpa, &key.xattrKey(x.ino, hash, coll), 0, value));
        }
        std.mem.sort(Record, out.items, {}, recordKeyLess);
        return out.toOwnedSlice(gpa);
    }

    /// The native slot tree's records: kinds 1, 2, 3 merged and memcmp-
    /// sorted — every key carries its kind byte (charter §5.1), so the
    /// merge interleaves an ino's inode, dentries and xattrs.
    pub fn nativeRecords(self: *const ImageBuilder, gpa: Allocator) DescribeError![]Record {
        var out: std.ArrayList(Record) = .empty;
        errdefer freeRecords(gpa, &out);
        inline for (.{ key.TREE_INODES, key.TREE_DENTRIES, key.TREE_XATTRS }) |kind| {
            const records = switch (kind) {
                key.TREE_INODES => try self.inodeRecords(gpa),
                key.TREE_DENTRIES => try self.dentryRecords(gpa),
                key.TREE_XATTRS => try self.xattrRecords(gpa),
                else => unreachable,
            };
            defer gpa.free(records);
            errdefer rec.deinitRecords(gpa, records);
            try out.appendSlice(gpa, records);
        }
        std.mem.sort(Record, out.items, {}, recordKeyLess);
        return out.toOwnedSlice(gpa);
    }

    /// The post-fold digest of the description's USER content: hash64 over
    /// `kind ‖ (len ‖ key ‖ len ‖ value)*` for kinds 1, 2, 3 in kind-then-
    /// key order.
    pub fn digest(self: *const ImageBuilder, gpa: Allocator) DescribeError!u64 {
        var h: checksum.Hasher = .init();
        inline for (.{ key.TREE_INODES, key.TREE_DENTRIES, key.TREE_XATTRS }) |kind| {
            const records = switch (kind) {
                key.TREE_INODES => try self.inodeRecords(gpa),
                key.TREE_DENTRIES => try self.dentryRecords(gpa),
                key.TREE_XATTRS => try self.xattrRecords(gpa),
                else => unreachable,
            };
            defer {
                rec.deinitRecords(gpa, records);
                gpa.free(records);
            }
            const refs = try gpa.alloc(RecordRef, records.len);
            defer gpa.free(refs);
            for (records, refs) |*r, *o| o.* = r.ref();
            digestUserKind(&h, kind, refs);
        }
        return h.finalLow64();
    }
};

/// The next `coll_seq` of group `k`: 0, 1, … 255 in call order (name order
/// at the call sites), null for the 257th member — a chain full.
fn nextInChain(comptime K: type, gpa: Allocator, groups: *std.AutoHashMapUnmanaged(K, u16), k: K) Allocator.Error!?u8 {
    const slot = try groups.getOrPutValue(gpa, k, 0);
    if (slot.value_ptr.* > std.math.maxInt(u8)) return null;
    const coll: u8 = @intCast(slot.value_ptr.*);
    slot.value_ptr.* += 1;
    return coll;
}

fn nameLess(_: void, a: []const u8, b: []const u8) bool {
    return std.mem.order(u8, a, b) == .lt;
}

fn xattrLess(_: void, a: Xattr, b: Xattr) bool {
    if (a.ino != b.ino) return a.ino < b.ino;
    return std.mem.order(u8, a.name, b.name) == .lt;
}

fn recordKeyLess(_: void, a: Record, b: Record) bool {
    return key.lessThan(a.key, b.key);
}

pub fn freeRecords(gpa: Allocator, list: *std.ArrayList(Record)) void {
    rec.deinitRecords(gpa, list.items);
    list.deinit(gpa);
}

/// The digest walk's exclusion predicate: the per-mount `writer_claim` /
/// `writer_term` guard xattrs.
pub fn isMountGuardXattr(value: []const u8) bool {
    const x = rec.XattrValue.decode(value) catch return false;
    return std.mem.eql(u8, x.name, WRITER_CLAIM_XATTR) or std.mem.eql(u8, x.name, WRITER_TERM_XATTR);
}

/// One user kind's contribution to the digest walk: the kind byte, then per
/// live record `len ‖ key ‖ len ‖ value` (u64 LE lengths), mount-guard
/// xattrs excluded.
pub fn digestUserKind(h: *checksum.Hasher, kind: u8, records: []const RecordRef) void {
    h.update(&[_]u8{kind});
    for (records) |r| {
        if (kind == key.TREE_XATTRS and isMountGuardXattr(r.value)) continue;
        var len: [8]u8 = undefined;
        std.mem.writeInt(u64, &len, r.key.len, .little);
        h.update(&len);
        h.update(r.key);
        std.mem.writeInt(u64, &len, r.value.len, .little);
        h.update(&len);
        h.update(r.value);
    }
}

pub const KindSetDigest = struct { count: u64, sum: u64 };

/// The ORDER-INDEPENDENT per-kind oracle for the set-shaped kinds (6, 7):
/// `(count, wrapping Σ hash64(len ‖ key ‖ len ‖ value))` — a sum, because a
/// forest's kind-routed walk yields these per slot tree.
pub fn digestKindSet(records: []const RecordRef) KindSetDigest {
    var out: KindSetDigest = .{ .count = 0, .sum = 0 };
    for (records) |r| {
        var h: checksum.Hasher = .init();
        var len: [8]u8 = undefined;
        std.mem.writeInt(u64, &len, r.key.len, .little);
        h.update(&len);
        h.update(r.key);
        std.mem.writeInt(u64, &len, r.value.len, .little);
        h.update(&len);
        h.update(r.value);
        out.count += 1;
        out.sum +%= h.finalLow64();
    }
    return out;
}

// ---------------------------------------------------------------------------
// §3.2 TreeWriter — bottom-up packing.
// ---------------------------------------------------------------------------

/// `(node_size − 4096 − 40 − 32) × 3/4` = `NodeLayout.splitPartCapacity`:
/// leaves pack to ¾ of a node's usable bytes (the K5 split-fill convention
/// — append headroom before the first SMO). 64 KiB ⇒ 46,026; 256 KiB ⇒
/// 193,482.
pub fn usableBudgetOf(layout: NodeLayout) usize {
    return (layout.node_size - node.NODE_PAGE - node.FRAME_LEN - bset.BSET_HEADER_LEN) * 3 / 4;
}

/// Greedy chunking of key-ascending records into runs of ≤ `budget` encoded
/// bytes (every run non-empty; a single record never exceeds the budget —
/// the value cap guarantees it at every node size). Empty input ⇒ one empty
/// chunk.
pub fn chunkByEncodedLen(gpa: Allocator, records: []const RecordRef, budget: usize, out: *std.ArrayList([]const RecordRef)) Allocator.Error!void {
    var start: usize = 0;
    var acc: usize = 0;
    for (records, 0..) |r, i| {
        const len = r.encodedLen();
        if (acc + len > budget and i > start) {
            try out.append(gpa, records[start..i]);
            start = i;
            acc = 0;
        }
        acc += len;
    }
    try out.append(gpa, records[start..]);
}

/// One written node of a level: its placement and the separator key its
/// parent carries.
pub const LevelNode = struct { addr: u64, seq: u64, max_key: key.KeyBuf };

pub const Root = struct { addr: u64, seq: u64 };

pub const WriteError = node.Error || error{ NoSpace, TreeTooDeep };

/// Writes whole trees as images: leaves greedily packed to the ¾ budget,
/// then interior levels of `(child max_key → (addr, seq))` separators until
/// a single root remains. Extents claim lowest-first (`claimInternal`), node
/// seqs count up in write order from the floor — both deterministic. The
/// images land in `out` in write order.
pub const TreeWriter = struct {
    gpa: Allocator,
    layout: NodeLayout,
    heap_base: u64,
    alloc: *const alloc_ext.ExtentAllocator,
    next_node_seq: u64,
    nodes_written: u64,
    out: *std.ArrayList(node.WrittenNode),

    /// A writer over `alloc`'s heap at `heap_base`, minting node seqs
    /// strictly above `node_seq_floor`.
    pub fn init(gpa: Allocator, layout: NodeLayout, heap_base: u64, alloc: *const alloc_ext.ExtentAllocator, node_seq_floor: u64, out: *std.ArrayList(node.WrittenNode)) TreeWriter {
        return .{ .gpa = gpa, .layout = layout, .heap_base = heap_base, .alloc = alloc, .next_node_seq = node_seq_floor, .nodes_written = 0, .out = out };
    }

    /// The highest node seq minted so far (the ledger's watermark).
    pub fn nodeSeqWatermark(self: *const TreeWriter) u64 {
        return self.next_node_seq;
    }

    pub fn nodesWritten(self: *const TreeWriter) u64 {
        return self.nodes_written;
    }

    /// The nodes `writeTree` WOULD write for key-ascending `records` — the
    /// same chunking level by level, nothing written: the capacity
    /// preflight's exact term (one heap extent per node).
    pub fn plannedNodes(gpa: Allocator, layout: NodeLayout, records: []const RecordRef) Allocator.Error!u64 {
        const budget = usableBudgetOf(layout);
        var chunks: std.ArrayList([]const RecordRef) = .empty;
        defer chunks.deinit(gpa);
        try chunkByEncodedLen(gpa, records, budget, &chunks);
        if (chunks.items.len <= 1) return 1;
        // The interior value is two fixed-width u64s, so a planned
        // separator's encoded length is the written one's.
        var seps: std.ArrayList(Record) = .empty;
        defer freeRecords(gpa, &seps);
        const zero_value = key.encodeInteriorValue(0, 0);
        for (chunks.items, 0..) |chunk, i| {
            const max_key: []const u8 = if (i + 1 == chunks.items.len) &key.KEY_SPACE_MAX else chunk[chunk.len - 1].key;
            try seps.append(gpa, try Record.put(gpa, max_key, 0, &zero_value));
        }
        const refs = try gpa.alloc(RecordRef, seps.items.len);
        defer gpa.free(refs);
        for (seps.items, refs) |*s, *o| o.* = s.ref();
        return chunks.items.len + try plannedNodes(gpa, layout, refs);
    }

    /// Write one whole tree of `tree_id` from key-ascending `records`;
    /// returns its root.
    pub fn writeTree(self: *TreeWriter, tree_id: u8, records: []const RecordRef) WriteError!Root {
        const gpa = self.gpa;
        var level: u8 = 0;
        var nodes = try self.writeLevel(tree_id, level, records);
        defer nodes.deinit(gpa);
        while (nodes.items.len > 1) {
            level = std.math.add(u8, level, 1) catch return error.TreeTooDeep;
            var seps: std.ArrayList(Record) = .empty;
            defer freeRecords(gpa, &seps);
            for (nodes.items) |n| {
                try seps.append(gpa, try Record.put(gpa, n.max_key.slice(), 0, &key.encodeInteriorValue(n.addr, n.seq)));
            }
            const refs = try gpa.alloc(RecordRef, seps.items.len);
            defer gpa.free(refs);
            for (seps.items, refs) |*s, *o| o.* = s.ref();
            const next = try self.writeLevel(tree_id, level, refs);
            nodes.deinit(gpa);
            nodes = next;
        }
        const root = nodes.items[0];
        return .{ .addr = root.addr, .seq = root.seq };
    }

    /// One whole level: `records` chunked by the budget, each chunk one
    /// node, sibling bounds partitioning the key space gap-free (`next.min =
    /// successor(prev.max)`; first min `""`, last max `KEY_SPACE_MAX`).
    /// Empty input still emits one empty leaf.
    fn writeLevel(self: *TreeWriter, tree_id: u8, level: u8, records: []const RecordRef) WriteError!std.ArrayList(LevelNode) {
        const gpa = self.gpa;
        var chunks: std.ArrayList([]const RecordRef) = .empty;
        defer chunks.deinit(gpa);
        try chunkByEncodedLen(gpa, records, usableBudgetOf(self.layout), &chunks);
        var out: std.ArrayList(LevelNode) = .empty;
        errdefer out.deinit(gpa);
        var min_key: key.KeyBuf = .{};
        for (chunks.items, 0..) |chunk, i| {
            const max_key: key.KeyBuf = if (i + 1 == chunks.items.len) key.KeyBuf.fromSlice(&key.KEY_SPACE_MAX) else key.KeyBuf.fromSlice(chunk[chunk.len - 1].key);
            const extent = try self.alloc.claimInternal();
            const addr = self.heap_base + extent * self.layout.node_size;
            self.next_node_seq += 1;
            const node_seq = self.next_node_seq;
            const written = try node.writeNode(gpa, self.layout, .{
                .node_addr = addr,
                .node_seq = node_seq,
                .tree_id = tree_id,
                .level = level,
                .min_key = min_key.slice(),
                .max_key = max_key.slice(),
            }, chunk, 0, null);
            try self.out.append(gpa, written);
            self.nodes_written += 1;
            min_key = key.keySuccessor(max_key.slice());
            try out.append(gpa, .{ .addr = addr, .seq = node_seq, .max_key = max_key });
        }
        return out;
    }
};

// ---------------------------------------------------------------------------
// §3.3 StreamWriter — the same packing over a record STREAM (the online
// slot-tree copy, charter §5.4: a slot IS a tree, so bulk copy = build the
// target tree bottom-up from the source's leaf snapshots; images go to the
// device as they fill, never held whole in RAM).
// ---------------------------------------------------------------------------

/// Where a built image goes: the migration engine binds the target
/// volume's node loader (one synchronous device write per node).
pub const ImageSink = struct {
    ctx: *anyopaque,
    write: *const fn (ctx: *anyopaque, addr: u64, image: []const u8) anyerror!void,
};

/// A node-seq mint: the target volume's shared handle (a private counter
/// would collide with the SMOs minting beside the copy).
pub const SeqMint = struct {
    ctx: *anyopaque,
    mint: *const fn (ctx: *anyopaque) anyerror!u64,
};

pub const StreamError = WriteError || error{ SinkFailed, SeqMintFailed };

/// Leaves are emitted as the ¾ budget fills (`chunkByEncodedLen`'s law, one
/// record at a time); `finish` closes the last leaf at `KEY_SPACE_MAX` and
/// packs the interior levels from the leaves' separators (one `KeyBuf`
/// per leaf in RAM — the only state that grows with the tree). Keys must
/// arrive ascending. The result is byte-for-byte the tree `TreeWriter.
/// writeTree` would pack from the same records (the tie test).
pub const StreamWriter = struct {
    gpa: Allocator,
    layout: NodeLayout,
    heap_base: u64,
    alloc: *const alloc_ext.ExtentAllocator,
    tree_id: u8,
    sink: ImageSink,
    seq: SeqMint,
    budget: usize,
    /// The open leaf's records (owned copies).
    pending: std.ArrayList(Record) = .empty,
    pending_bytes: usize = 0,
    /// The leaf's min key: `""` for the first, `successor(prev.max)` after.
    min_key: key.KeyBuf = .{},
    leaves: std.ArrayList(LevelNode) = .empty,
    records_written: u64 = 0,
    nodes_written: u64 = 0,
    /// The highest node seq minted (the target's watermark input).
    seq_high: u64 = 0,
    last_key: ?key.KeyBuf = null,

    pub fn init(gpa: Allocator, layout: NodeLayout, heap_base: u64, alloc: *const alloc_ext.ExtentAllocator, tree_id: u8, sink: ImageSink, seq: SeqMint) StreamWriter {
        return .{ .gpa = gpa, .layout = layout, .heap_base = heap_base, .alloc = alloc, .tree_id = tree_id, .sink = sink, .seq = seq, .budget = usableBudgetOf(layout) };
    }

    pub fn deinit(self: *StreamWriter) void {
        freeRecords(self.gpa, &self.pending);
        self.leaves.deinit(self.gpa);
        self.* = undefined;
    }

    /// One record (seq 0, a `Put`): queued into the open leaf; a leaf that
    /// would overflow the budget is written first.
    pub fn append(self: *StreamWriter, k: []const u8, value: []const u8) StreamError!void {
        if (self.last_key) |lk| assert(key.lessThan(lk.slice(), k));
        self.last_key = key.KeyBuf.fromSlice(k);
        const r = try Record.put(self.gpa, k, 0, value);
        errdefer {
            var tmp = r;
            tmp.deinit(self.gpa);
        }
        const len = r.ref().encodedLen();
        if (self.pending_bytes + len > self.budget and self.pending.items.len > 0) try self.flushLeaf(false);
        try self.pending.append(self.gpa, r);
        self.pending_bytes += len;
    }

    fn flushLeaf(self: *StreamWriter, last: bool) StreamError!void {
        const max_key: key.KeyBuf = if (last) key.KeyBuf.fromSlice(&key.KEY_SPACE_MAX) else key.KeyBuf.fromSlice(self.pending.items[self.pending.items.len - 1].key);
        const refs = try self.gpa.alloc(RecordRef, self.pending.items.len);
        defer self.gpa.free(refs);
        for (self.pending.items, refs) |*r, *o| o.* = r.ref();
        const n = try self.writeNode(0, self.min_key.slice(), max_key.slice(), refs);
        try self.leaves.append(self.gpa, n);
        self.records_written += refs.len;
        freeRecords(self.gpa, &self.pending);
        self.pending = .empty;
        self.pending_bytes = 0;
        self.min_key = key.keySuccessor(max_key.slice());
    }

    fn writeNode(self: *StreamWriter, level: u8, min_key: []const u8, max_key: []const u8, records: []const RecordRef) StreamError!LevelNode {
        const extent = try self.alloc.claimInternal();
        errdefer self.alloc.releaseUnpublished(extent);
        const addr = self.heap_base + extent * self.layout.node_size;
        const node_seq = self.seq.mint(self.seq.ctx) catch return error.SeqMintFailed;
        self.seq_high = @max(self.seq_high, node_seq);
        var written = try node.writeNode(self.gpa, self.layout, .{
            .node_addr = addr,
            .node_seq = node_seq,
            .tree_id = self.tree_id,
            .level = level,
            .min_key = min_key,
            .max_key = max_key,
        }, records, 0, null);
        defer written.deinit(self.gpa);
        self.sink.write(self.sink.ctx, addr, written.image) catch return error.SinkFailed;
        self.nodes_written += 1;
        return .{ .addr = addr, .seq = node_seq, .max_key = key.KeyBuf.fromSlice(max_key) };
    }

    /// Close the tree: the last leaf (an empty stream still writes one
    /// empty root leaf spanning the key space), then the interior levels
    /// until one root remains.
    pub fn finish(self: *StreamWriter) StreamError!Root {
        try self.flushLeaf(true);
        var level: u8 = 0;
        var nodes = self.leaves;
        self.leaves = .empty;
        defer nodes.deinit(self.gpa);
        while (nodes.items.len > 1) {
            level = std.math.add(u8, level, 1) catch return error.TreeTooDeep;
            var seps: std.ArrayList(Record) = .empty;
            defer freeRecords(self.gpa, &seps);
            for (nodes.items) |n| try seps.append(self.gpa, try Record.put(self.gpa, n.max_key.slice(), 0, &key.encodeInteriorValue(n.addr, n.seq)));
            const refs = try self.gpa.alloc(RecordRef, seps.items.len);
            defer self.gpa.free(refs);
            for (seps.items, refs) |*s, *o| o.* = s.ref();
            var chunks: std.ArrayList([]const RecordRef) = .empty;
            defer chunks.deinit(self.gpa);
            try chunkByEncodedLen(self.gpa, refs, self.budget, &chunks);
            var next: std.ArrayList(LevelNode) = .empty;
            errdefer next.deinit(self.gpa);
            var min_key: key.KeyBuf = .{};
            for (chunks.items, 0..) |chunk, i| {
                const max_key: key.KeyBuf = if (i + 1 == chunks.items.len) key.KeyBuf.fromSlice(&key.KEY_SPACE_MAX) else key.KeyBuf.fromSlice(chunk[chunk.len - 1].key);
                try next.append(self.gpa, try self.writeNode(level, min_key.slice(), max_key.slice(), chunk));
                min_key = key.keySuccessor(max_key.slice());
            }
            nodes.deinit(self.gpa);
            nodes = next;
        }
        const root = nodes.items[0];
        return .{ .addr = root.addr, .seq = root.seq };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const test_cfg: BuilderConfig = .{ .node_size = 65536, .journal_len_override = 1 << 20, .hash_seed = 0x5EED_CAFE_F00D_D00D, .uuid = "kv-backend-test!".* };

fn refsOf(gpa: Allocator, records: []const Record) ![]RecordRef {
    const out = try gpa.alloc(RecordRef, records.len);
    for (records, out) |*r, *o| o.* = r.ref();
    return out;
}

fn freeOwned(gpa: Allocator, records: []Record) void {
    rec.deinitRecords(gpa, records);
    gpa.free(records);
}

test "a fresh description holds the root alone: ino 1 S_IFDIR|0755, 0:0, nlink 2, times 0; next_ino 2; mode bits and dt" {
    var b = try ImageBuilder.init(testing.allocator, test_cfg);
    defer b.deinit();
    try testing.expectEqual(@as(u64, 1), b.inodeCount());
    try testing.expectEqual(@as(u64, 2), b.nextIno());
    const root = b.inode(ROOT_INO).?;
    try testing.expectEqual(@as(u32, 0o040755), root.mode);
    try testing.expectEqual(@as(u32, 0), root.uid);
    try testing.expectEqual(@as(u32, 2), root.nlink);
    try testing.expectEqual(@as(u64, 0), root.mtime);
    try testing.expectEqual(@as(u8, 4), dtOf(S_IFDIR));
    try testing.expectEqual(@as(u8, 8), dtOf(S_IFREG | 0o644));
    // The root inode record is the v4 bytes: key `ino ‖ 01`, value the
    // 68 B fixed layout (`ed 41 00 00 ‖ uid 0 ‖ gid 0 ‖ nlink 2 ‖ …`).
    const recs = try b.inodeRecords(testing.allocator);
    defer freeOwned(testing.allocator, recs);
    try testing.expectEqual(@as(usize, 1), recs.len);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 1, key.TREE_INODES }, recs[0].key);
    try testing.expectEqual(rec.INODE_VALUE_LEN, recs[0].value.len);
    try testing.expectEqualSlices(u8, &.{ 0xed, 0x41, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0 }, recs[0].value[0..16]);
    try testing.expect(std.mem.allEqual(u8, recs[0].value[16..], 0));
    try b.setOwner(ROOT_INO, 1000, 1000);
    try testing.expectEqual(@as(u32, 1000), b.inode(ROOT_INO).?.gid);
    try b.setTimes(ROOT_INO, 1, 2, 3);
    try testing.expectEqual(@as(u64, 3), b.inode(ROOT_INO).?.ctime);
}

test "describe: dirs raise the parent's nlink; files nlink 1; links bump nlink; typed refusals (duplicate, missing/non-dir parent, dir link, 256-byte name, over-cap xattr)" {
    const gpa = testing.allocator;
    var b = try ImageBuilder.init(gpa, test_cfg);
    defer b.deinit();
    const d = try b.addDir(ROOT_INO, "dir", 0o700, 1, 2);
    try testing.expectEqual(@as(u64, 2), d);
    try testing.expectEqual(@as(u32, 3), b.inode(ROOT_INO).?.nlink);
    try testing.expectEqual(@as(u32, 0o040700), b.inode(d).?.mode);
    const f = try b.addFile(d, "file", 0o644, 1, 2, 4096);
    try testing.expectEqual(@as(u64, 3), f);
    try testing.expectEqual(@as(u32, 1), b.inode(f).?.nlink);
    try testing.expectEqual(@as(u64, 4096), b.inode(f).?.size);
    try b.addLink(f, ROOT_INO, "link");
    try testing.expectEqual(@as(u32, 2), b.inode(f).?.nlink);
    try testing.expectEqual(@as(u64, 4), b.nextIno());
    try testing.expectError(error.DuplicateName, b.addFile(ROOT_INO, "dir", 0o644, 0, 0, 0));
    try testing.expectError(error.NoSuchParent, b.addFile(99, "x", 0o644, 0, 0, 0));
    try testing.expectError(error.ParentNotDirectory, b.addFile(f, "x", 0o644, 0, 0, 0));
    try testing.expectError(error.DirectoryHardLink, b.addLink(d, ROOT_INO, "dirlink"));
    try testing.expectError(error.NoSuchIno, b.addLink(77, ROOT_INO, "ghost"));
    try testing.expectError(error.EmptyName, b.addDir(ROOT_INO, "", 0o755, 0, 0));
    const long: [256]u8 = @splat('a');
    try testing.expectError(error.NameTooLong, b.addDir(ROOT_INO, &long, 0o755, 0, 0));
    try testing.expectEqual(@as(u64, 4), b.nextIno()); // refusals mint nothing
    // 17 KiB xattr on a 64 KiB-node volume: 1 + 4 + 17408 > 16,640.
    const big = try gpa.alloc(u8, 17 * 1024);
    defer gpa.free(big);
    @memset(big, 7);
    try testing.expectError(error.ValueTooLarge, b.setXattr(ROOT_INO, "user", big));
    try testing.expectError(error.NoSuchIno, b.setXattr(99, "user.x", "v"));
    try b.setXattr(ROOT_INO, "user.note", "one");
    try b.setXattr(ROOT_INO, "user.note", "two"); // replace
    try testing.expectEqual(@as(usize, 1), b.xattrs.items.len);
    try testing.expectEqualStrings("two", b.xattrs.items[0].value);
}

test "record assembly: inode records in ino order; dentry keys hash-shuffled then memcmp-sorted with name-ordered coll_seq; xattr keys by (ino, hash56); the native slot tree merges the kinds ino-major" {
    const gpa = testing.allocator;
    var b = try ImageBuilder.init(gpa, test_cfg);
    defer b.deinit();
    const d = try b.addDir(ROOT_INO, "docs", 0o755, 0, 0);
    _ = try b.addFile(ROOT_INO, "b.txt", 0o644, 0, 0, 10);
    _ = try b.addFile(ROOT_INO, "a.txt", 0o644, 0, 0, 20);
    _ = try b.addFile(d, "readme", 0o644, 0, 0, 30);
    try b.setXattr(ROOT_INO, "user.b", "2");
    try b.setXattr(ROOT_INO, "user.a", "1");
    try b.setXattr(d, "user.c", "3");
    const inodes = try b.inodeRecords(gpa);
    defer freeOwned(gpa, inodes);
    try testing.expectEqual(@as(usize, 5), inodes.len);
    for (inodes, 1..) |r, ino| try testing.expectEqual(ino, try key.decodeInodeKey(r.key));
    const dentries = try b.dentryRecords(gpa);
    defer freeOwned(gpa, dentries);
    try testing.expectEqual(@as(usize, 4), dentries.len);
    for (dentries[1..], 1..) |r, i| try testing.expect(key.lessThan(dentries[i - 1].key, r.key));
    var names_seen: usize = 0;
    for (dentries) |r| {
        const dk = try key.DentryKey.decode(r.key);
        const dv = try rec.DentryValue.decode(r.value);
        const nk: key.NameKey = .init(test_cfg.hash_seed);
        try testing.expectEqual(key.dentryNameHash54(dv.name, &nk), dk.hash54());
        try testing.expectEqual(@as(u8, 0), dk.collSeq());
        if (std.mem.eql(u8, dv.name, "docs")) {
            try testing.expectEqual(@as(u64, 1), dk.parentIno());
            try testing.expectEqual(d, dv.child_ino);
            try testing.expectEqual(@as(u8, 4), dv.file_type);
        }
        names_seen += 1;
    }
    try testing.expectEqual(@as(usize, 4), names_seen);
    const xattrs = try b.xattrRecords(gpa);
    defer freeOwned(gpa, xattrs);
    try testing.expectEqual(@as(usize, 3), xattrs.len);
    for (xattrs[1..], 1..) |r, i| try testing.expect(key.lessThan(xattrs[i - 1].key, r.key));
    const xk2 = try key.XattrKey.decode(xattrs[2].key);
    try testing.expectEqual(d, xk2.ino());
    const native = try b.nativeRecords(gpa);
    defer freeOwned(gpa, native);
    try testing.expectEqual(@as(usize, 12), native.len);
    for (native[1..], 1..) |r, i| try testing.expect(key.lessThan(native[i - 1].key, r.key));
    // ino 1's records lead: inode (kind 1), dentries (kind 2), xattrs (kind 3).
    try testing.expectEqual(key.TREE_INODES, try key.slotTreeKeyKind(native[0].key));
    try testing.expectEqual(key.TREE_DENTRIES, try key.slotTreeKeyKind(native[1].key));
    try testing.expectEqual(key.TREE_XATTRS, try key.slotTreeKeyKind(native[4].key));
    try testing.expectEqual(key.TREE_INODES, try key.slotTreeKeyKind(native[6].key));
    try testing.expectEqualSlices(u8, &key.inodeKey(1), native[0].key);
    for (native) |r| try testing.expectEqual(@as(key.ForestSlot, 0), try key.slotTreeKeySlot(r.key));
    // The digest is a function of the description alone.
    const d1 = try b.digest(gpa);
    var b2 = try ImageBuilder.init(gpa, test_cfg);
    defer b2.deinit();
    try testing.expect(d1 != try b2.digest(gpa));
    try testing.expectEqual(d1, try b.digest(gpa));
}

test "coll_seq chains: a same-hash group takes 0, 1, … 255 in call (name) order and the 257th member is the typed overflow; distinct groups are independent" {
    const gpa = testing.allocator;
    var groups: std.AutoHashMapUnmanaged(u64, u16) = .empty;
    defer groups.deinit(gpa);
    for (0..256) |i| try testing.expectEqual(@as(?u8, @intCast(i)), try nextInChain(u64, gpa, &groups, 42));
    try testing.expectEqual(@as(?u8, null), try nextInChain(u64, gpa, &groups, 42));
    try testing.expectEqual(@as(?u8, null), try nextInChain(u64, gpa, &groups, 42));
    try testing.expectEqual(@as(?u8, 0), try nextInChain(u64, gpa, &groups, 43));
    var xgroups: std.AutoHashMapUnmanaged([2]u64, u16) = .empty;
    defer xgroups.deinit(gpa);
    try testing.expectEqual(@as(?u8, 0), try nextInChain([2]u64, gpa, &xgroups, .{ 1, 7 }));
    try testing.expectEqual(@as(?u8, 0), try nextInChain([2]u64, gpa, &xgroups, .{ 2, 7 }));
    try testing.expectEqual(@as(?u8, 1), try nextInChain([2]u64, gpa, &xgroups, .{ 1, 7 }));
}

test "TreeWriter: the ¾ budget (64 KiB ⇒ 46,026; 256 KiB ⇒ 193,482 = splitPartCapacity); greedy chunks; an empty tree is one real leaf; separators and bounds; planned_nodes == written" {
    const gpa = testing.allocator;
    const small = try NodeLayout.new(65536, null);
    try testing.expectEqual(@as(usize, 46_026), usableBudgetOf(small));
    try testing.expectEqual(small.splitPartCapacity(), usableBudgetOf(small));
    const large = try NodeLayout.new(262144, null);
    try testing.expectEqual(@as(usize, 193_482), usableBudgetOf(large));
    try testing.expectEqual(large.splitPartCapacity(), usableBudgetOf(large));
    // Chunking.
    var recs: [10]Record = undefined;
    const v: [100]u8 = @splat(1);
    for (&recs, 0..) |*r, i| r.* = try Record.put(gpa, &key.inodeKey(i + 1), 0, &v);
    defer rec.deinitRecords(gpa, &recs);
    const refs = try refsOf(gpa, &recs);
    defer gpa.free(refs);
    var chunks: std.ArrayList([]const RecordRef) = .empty;
    defer chunks.deinit(gpa);
    try chunkByEncodedLen(gpa, refs, 3 * (rec.RECORD_HEADER_LEN + key.INODE_KEY_LEN + 100), &chunks);
    try testing.expectEqual(@as(usize, 4), chunks.items.len);
    try testing.expectEqual(@as(usize, 3), chunks.items[0].len);
    try testing.expectEqual(@as(usize, 1), chunks.items[3].len);
    chunks.clearRetainingCapacity();
    try chunkByEncodedLen(gpa, &.{}, 100, &chunks);
    try testing.expectEqual(@as(usize, 1), chunks.items.len);
    try testing.expectEqual(@as(usize, 0), chunks.items[0].len);
    // A tiny budget forces a multi-level tree; the images are real nodes.
    var alloc = try alloc_ext.ExtentAllocator.format(gpa, 100, 8, alloc_ext.builder_pending_cap);
    defer alloc.deinit();
    var images: std.ArrayList(node.WrittenNode) = .empty;
    defer {
        for (images.items) |*w| w.deinit(gpa);
        images.deinit(gpa);
    }
    var tw = TreeWriter.init(gpa, small, 1 << 20, &alloc, 500, &images);
    const empty_root = try tw.writeTree(8, &.{});
    try testing.expectEqual(@as(u64, 1 << 20), empty_root.addr);
    try testing.expectEqual(@as(u64, 501), empty_root.seq);
    try testing.expectEqual(@as(u64, 1), tw.nodesWritten());
    try testing.expectEqual(@as(usize, node.NODE_PAGE), images.items[0].bytesWritten());
    const eh = images.items[0].header();
    try testing.expectEqual(@as(u8, 8), eh.tree_id);
    try testing.expectEqual(@as(u8, 0), eh.level);
    try testing.expectEqualStrings("", eh.min_key);
    try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, eh.max_key);
    const root = try tw.writeTree(1, refs);
    try testing.expectEqual(@as(u64, (1 << 20) + 65536), root.addr);
    try testing.expectEqual(@as(u64, 502), root.seq);
    try testing.expectEqual(@as(u64, 2), tw.nodesWritten());
    try testing.expectEqual(@as(u64, 502), tw.nodeSeqWatermark());
    try testing.expectEqual(@as(u64, 1), try TreeWriter.plannedNodes(gpa, small, refs));
    try testing.expectEqual(@as(u64, 1), try TreeWriter.plannedNodes(gpa, small, &.{}));
    // Many records: a two-level tree. 2,000 × (16 + 9 + 100) = 250,000 B
    // over a 46,026 budget (368 records per leaf) ⇒ 6 leaves + 1 root.
    var many: [2000]Record = undefined;
    for (&many, 0..) |*r, i| r.* = try Record.put(gpa, &key.inodeKey(i + 1), 0, &v);
    defer rec.deinitRecords(gpa, &many);
    const many_refs = try refsOf(gpa, &many);
    defer gpa.free(many_refs);
    try testing.expectEqual(@as(u64, 7), try TreeWriter.plannedNodes(gpa, small, many_refs));
    const before = images.items.len;
    const big_root = try tw.writeTree(1, many_refs);
    try testing.expectEqual(@as(usize, before + 7), images.items.len);
    try testing.expectEqual(@as(u64, 509), tw.nodeSeqWatermark());
    try testing.expectEqual(big_root.seq, tw.nodeSeqWatermark());
    const root_img = &images.items[images.items.len - 1];
    try testing.expectEqual(big_root.addr, root_img.node_addr);
    const rh = root_img.header();
    try testing.expectEqual(@as(u8, 1), rh.level);
    try testing.expectEqualStrings("", rh.min_key);
    try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, rh.max_key);
    try testing.expectEqual(@as(usize, 6), root_img.record_count);
    // Leaves: first min "", each next min = successor(prev max), last max
    // = KEY_SPACE_MAX; separators name (addr, seq) of each leaf.
    var prev_max: ?key.KeyBuf = null;
    for (images.items[before .. before + 6], 0..) |*leaf, i| {
        const h = leaf.header();
        try testing.expectEqual(@as(u8, 0), h.level);
        if (prev_max) |pm| try testing.expectEqualSlices(u8, key.keySuccessor(pm.slice()).slice(), h.min_key) else try testing.expectEqualStrings("", h.min_key);
        if (i == 5) try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, h.max_key);
        prev_max = key.KeyBuf.fromSlice(h.max_key);
        try testing.expect(leaf.bytesWritten() <= 65536);
    }
    const ext = try gpa.alloc(u8, 65536);
    defer gpa.free(ext);
    @memset(ext, 0);
    @memcpy(ext[0..root_img.image.len], root_img.image);
    var root_loaded = try node.verifyNodeExtent(gpa, ext, small, big_root.addr, 0, null);
    defer root_loaded.deinit(gpa);
    var view = try root_loaded.bsetAt(gpa, 0);
    defer view.deinit(gpa);
    for (0..view.len()) |i| {
        const sep = view.record(i);
        const iv = try key.InteriorValue.decode(sep.value);
        try testing.expectEqual(images.items[before + i].node_addr, iv.childAddr());
        try testing.expectEqual(images.items[before + i].node_seq, iv.childSeq());
        try testing.expectEqualSlices(u8, images.items[before + i].header().max_key, sep.key);
    }
    // Extents claimed lowest-first, one per node.
    for (images.items, 0..) |*w, i| try testing.expectEqual((1 << 20) + i * 65536, w.node_addr);
    try testing.expectEqual(@as(u64, 100 - 9), alloc.freeExtents());
}

test "digest walks: kind-then-key chained hash excludes writer_claim/writer_term; the set digest is order-independent" {
    const gpa = testing.allocator;
    const claim = try (rec.XattrValue{ .name = WRITER_CLAIM_XATTR, .value = "x" }).encodeAlloc(gpa);
    defer gpa.free(claim);
    const user = try (rec.XattrValue{ .name = "user.a", .value = "x" }).encodeAlloc(gpa);
    defer gpa.free(user);
    try testing.expect(isMountGuardXattr(claim));
    try testing.expect(!isMountGuardXattr(user));
    try testing.expect(!isMountGuardXattr("\xff"));
    var h1: checksum.Hasher = .init();
    digestUserKind(&h1, key.TREE_XATTRS, &.{ .{ .key = "a", .seq = 0, .kind = .put, .value = user }, .{ .key = "b", .seq = 0, .kind = .put, .value = claim } });
    var h2: checksum.Hasher = .init();
    digestUserKind(&h2, key.TREE_XATTRS, &.{.{ .key = "a", .seq = 0, .kind = .put, .value = user }});
    try testing.expectEqual(h1.finalLow64(), h2.finalLow64());
    const a = RecordRef{ .key = "k1", .seq = 0, .kind = .put, .value = "v1" };
    const b = RecordRef{ .key = "k2", .seq = 0, .kind = .put, .value = "v2" };
    const ab = digestKindSet(&.{ a, b });
    const ba = digestKindSet(&.{ b, a });
    try testing.expectEqual(ab.sum, ba.sum);
    try testing.expectEqual(@as(u64, 2), ab.count);
    try testing.expect(ab.sum != digestKindSet(&.{a}).sum);
}

test "StreamWriter packs the same leaf partition, levels and record population as TreeWriter from the same ascending records; an empty stream is one empty root leaf" {
    const gpa = testing.allocator;
    const layout = try NodeLayout.new(65536, null);
    var b = try ImageBuilder.init(gpa, test_cfg);
    defer b.deinit();
    var name_buf: [32]u8 = undefined;
    const d = try b.addDir(ROOT_INO, "d", 0o755, 0, 0);
    for (0..2500) |i| _ = try b.addFile(d, try std.fmt.bufPrint(&name_buf, "f{d}", .{i}), 0o644, 0, 0, i);
    const records = try b.nativeRecords(gpa);
    defer freeOwned(gpa, records);
    const refs = try refsOf(gpa, records);
    defer gpa.free(refs);
    // The whole-tree writer.
    var alloc_a = try alloc_ext.ExtentAllocator.format(gpa, 4096, 0, 64);
    defer alloc_a.deinit();
    var images: std.ArrayList(node.WrittenNode) = .empty;
    defer {
        for (images.items) |*w| w.deinit(gpa);
        images.deinit(gpa);
    }
    var tw = TreeWriter.init(gpa, layout, 0, &alloc_a, 100, &images);
    const root_a = try tw.writeTree(key.KIND_INTERIOR, refs);
    // The stream writer into a RAM sink.
    const Sink = struct {
        gpa: Allocator,
        nodes: std.ArrayList(node.WrittenNode) = .empty,
        next_seq: u64 = 100,
        fn write(ctx: *anyopaque, addr: u64, image: []const u8) anyerror!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const copy = try self.gpa.dupe(u8, image);
            const h = try node.decodeHeaderPage(copy[0..node.NODE_PAGE], null);
            try self.nodes.append(self.gpa, .{ .node_addr = addr, .node_seq = h.node_seq, .record_count = 0, .journal_seq_horizon = 0, .image = copy });
        }
        fn mint(ctx: *anyopaque) anyerror!u64 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.next_seq += 1;
            return self.next_seq;
        }
    };
    var sink: Sink = .{ .gpa = gpa };
    defer {
        for (sink.nodes.items) |*w| w.deinit(gpa);
        sink.nodes.deinit(gpa);
    }
    var alloc_b = try alloc_ext.ExtentAllocator.format(gpa, 4096, 0, 64);
    defer alloc_b.deinit();
    var sw = StreamWriter.init(gpa, layout, 0, &alloc_b, key.KIND_INTERIOR, .{ .ctx = &sink, .write = Sink.write }, .{ .ctx = &sink, .mint = Sink.mint });
    defer sw.deinit();
    for (refs) |r| try sw.append(r.key, r.value);
    const root_b = try sw.finish();
    try testing.expectEqual(@as(u64, refs.len), sw.records_written);
    try testing.expectEqual(images.items.len, sink.nodes.items.len);
    try testing.expectEqual(tw.nodesWritten(), sw.nodes_written);
    try testing.expect(images.items.len > 5);
    // Node by node in write order: the same level, bounds and record keys
    // (node seqs and addresses are each writer's own).
    for (images.items, sink.nodes.items) |*a, *s| {
        const ha = a.header();
        const hs = s.header();
        try testing.expectEqual(ha.level, hs.level);
        try testing.expectEqualSlices(u8, ha.min_key, hs.min_key);
        try testing.expectEqualSlices(u8, ha.max_key, hs.max_key);
        if (ha.level == 0) {
            // The images are the written prefix; a load reads the extent.
            const pa = try gpa.alloc(u8, layout.node_size);
            defer gpa.free(pa);
            @memset(pa, 0);
            @memcpy(pa[0..a.image.len], a.image);
            const ps = try gpa.alloc(u8, layout.node_size);
            defer gpa.free(ps);
            @memset(ps, 0);
            @memcpy(ps[0..s.image.len], s.image);
            var na = try node.verifyNodeExtent(gpa, pa, layout, a.node_addr, 0, null);
            defer na.deinit(gpa);
            var ns = try node.verifyNodeExtent(gpa, ps, layout, s.node_addr, 0, null);
            defer ns.deinit(gpa);
            var ba = try na.bsetAt(gpa, 0);
            defer ba.deinit(gpa);
            var bs = try ns.bsetAt(gpa, 0);
            defer bs.deinit(gpa);
            try testing.expectEqual(ba.len(), bs.len());
            for (0..ba.len()) |i| try testing.expectEqualSlices(u8, ba.record(i).key, bs.record(i).key);
        }
    }
    const ra = images.items[images.items.len - 1].header();
    const rb = sink.nodes.items[sink.nodes.items.len - 1].header();
    try testing.expectEqual(ra.level, rb.level);
    try testing.expectEqual(root_a.addr, images.items[images.items.len - 1].node_addr);
    try testing.expectEqual(root_b.addr, sink.nodes.items[sink.nodes.items.len - 1].node_addr);
    // An empty stream: one empty leaf spanning the key space is the root.
    var empty_sink: Sink = .{ .gpa = gpa };
    defer {
        for (empty_sink.nodes.items) |*w| w.deinit(gpa);
        empty_sink.nodes.deinit(gpa);
    }
    var ew = StreamWriter.init(gpa, layout, 0, &alloc_b, key.KIND_INTERIOR, .{ .ctx = &empty_sink, .write = Sink.write }, .{ .ctx = &empty_sink, .mint = Sink.mint });
    defer ew.deinit();
    _ = try ew.finish();
    try testing.expectEqual(@as(usize, 1), empty_sink.nodes.items.len);
    const eh = empty_sink.nodes.items[0].header();
    try testing.expectEqual(@as(u8, 0), eh.level);
    try testing.expectEqualSlices(u8, "", eh.min_key);
    try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, eh.max_key);
}
