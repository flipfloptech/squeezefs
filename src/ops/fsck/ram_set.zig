//! The portable test fixture: a RAM set (the kv_fs `MemStore` as the store,
//! one `BlockAllocator` per data volume, a RAM block device per volume, a
//! 4a manager) built record by record through the SAME codecs and
//! publish site the filesystem uses, with the defect injectors each class's
//! red test needs. Test-only: imported inside `test` blocks alone.

const std = @import("std");
const Allocator = std.mem.Allocator;
const testing = std.testing;

const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tree = @import("../../meta/tree.zig");
const tx = @import("../../meta/tx.zig");
const inos = @import("../../meta/inos.zig");
const dlm = @import("../../meta/dlm.zig");
const economy = @import("../../meta/economy.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const layout = @import("../../data/layout.zig");
const block_refs = @import("../../data/block_refs.zig");
const block_alloc = @import("../../data/block_alloc.zig");
const kvmap = @import("../../data/kvmap.zig");
const kv_fs = @import("../../fuse/handler/kv_fs.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");
const classes = @import("classes.zig");
const scrub = @import("scrub.zig");

pub const MemStore = kv_fs.MemStore;
pub const Error = census.Error;

pub const CHUNK: u64 = 64 * 1024;
pub const VOL_A: u64 = 0x10;
pub const VOL_B: u64 = 0x20;
pub const BLOCKS: u64 = 64;
pub const MEM_BUDGET: u64 = 1 << 30;

/// A store wrapper that can fail one range read (the torn-node seam) and
/// otherwise forwards to the `MemStore`.
pub const FaultStore = struct {
    inner: *MemStore,
    /// A range of this kind reaching this key fails with `Io` (null = inert).
    tear_at: ?struct { kind: u8, from: key.KeyBuf } = null,

    pub const StoreError = MemStore.StoreError;

    pub fn range(self: *FaultStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        // A torn node fails every range that reaches it.
        if (self.tear_at) |t| if (t.kind == kind and !key.lessThan(end, t.from.slice())) return error.Io;
        return self.inner.range(gpa, kind, start, end, max, out);
    }
    pub fn lookup(self: *FaultStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        return self.inner.lookup(gpa, kind, k);
    }
    pub fn begin(self: *FaultStore, site: tx.Site) Allocator.Error!*tx.KvTx {
        return self.inner.begin(site);
    }
    pub fn commit(self: *FaultStore, t: *tx.KvTx) StoreError!void {
        return self.inner.commit(t);
    }
    pub fn abandon(self: *FaultStore, t: *tx.KvTx) void {
        self.inner.abandon(t);
    }
    pub fn nameKey(self: *const FaultStore) *const key.NameKey {
        return self.inner.nameKey();
    }
};

pub const RamSet = struct {
    gpa: Allocator,
    mem: MemStore,
    fault: FaultStore,
    allocs: [2]*block_alloc.BlockAllocator,
    devs: [2][]u8,
    vols: [2]classes.Volume,
    xforms: [2]scrub.VolumeXform,
    scope: census.SlotScope,
    locks: dlm.Manager,
    geo: layout.Geometry,
    /// The next native local to mint (slot 0; the root is 1): a CAS word so
    /// the storm's lanes mint concurrently.
    next_local: std.atomic.Value(u64) = .init(inos.LOCAL_INO_BASE),
    /// The open-intent set the tests arm (global inos).
    intents: std.AutoHashMapUnmanaged(u64, void) = .empty,
    /// The pack-open ledger the tests arm: `(vol_tag, block_idx)`.
    open_packs: std.AutoHashMapUnmanaged(u64, void) = .empty,

    pub fn init(gpa: Allocator) !*RamSet {
        const self = try gpa.create(RamSet);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .mem = try MemStore.init(gpa), .fault = undefined, .allocs = undefined, .devs = undefined, .vols = undefined, .xforms = undefined, .scope = census.SlotScope.init(gpa), .locks = try dlm.Manager.init(gpa, 64), .geo = .{ .chunk = CHUNK } };
        self.fault = .{ .inner = &self.mem };
        for (&self.allocs, [_]u64{ VOL_A, VOL_B }, 0..) |*a, tag, i| {
            a.* = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = tag, .vol = @intCast(i), .block_size = CHUNK, .capacity_bytes = BLOCKS * CHUNK });
            self.devs[i] = try gpa.alloc(u8, @intCast(BLOCKS * CHUNK));
            @memset(self.devs[i], 0);
            self.vols[i] = .{ .vol_tag = tag, .alloc = a.* };
            self.xforms[i] = .{ .vol_tag = tag, .cfg = .{ .block_size = @intCast(CHUNK) }, .face = null };
        }
        // Slot 0 is this mount's; the era floor is raised above every
        // fixture mint by `freezeEra` once the fixture is built.
        try self.scope.leased.put(gpa, 0, {});
        try self.scope.era_floor.put(gpa, 0, inos.LOCAL_INO_BASE);
        return self;
    }

    pub fn deinit(self: *RamSet) void {
        const gpa = self.gpa;
        for (self.allocs) |a| a.deinit();
        for (self.devs) |d| gpa.free(d);
        self.scope.deinit();
        self.locks.deinit();
        self.intents.deinit(gpa);
        self.open_packs.deinit(gpa);
        self.mem.deinit();
        gpa.destroy(self);
    }

    /// Everything minted so far is prior-era (the next mount's view).
    pub fn freezeEra(self: *RamSet) !void {
        try self.scope.era_floor.put(self.gpa, 0, self.next_local.load(.acquire));
    }

    pub fn store(self: *RamSet) census.Store {
        return census.Store.from(FaultStore, &self.fault);
    }

    pub fn host(self: *RamSet, mode: finding.Mode) classes.Host {
        return .{
            .gpa = self.gpa,
            .io = testing.io,
            .store = self.store(),
            .geo = self.geo,
            .name_key = self.mem.nameKey(),
            .volumes = &self.vols,
            .scope = &self.scope,
            .mode = mode,
            .dlm = if (mode == .online) &self.locks else null,
            .pack = .{ .ctx = self, .isOpenPack = &isOpenPackFn },
            .intents = .{ .ctx = self, .isOpen = &intentOpenFn },
            .reader = .{ .ctx = self, .read = &readFn },
            .mem_budget = MEM_BUDGET,
            .settle_ns = 0,
        };
    }

    fn isOpenPackFn(ctx: ?*anyopaque, vol_tag: u64, block_idx: u64) bool {
        const self: *RamSet = @ptrCast(@alignCast(ctx.?));
        return self.open_packs.contains(vol_tag ^ (block_idx << 8));
    }

    fn intentOpenFn(ctx: ?*anyopaque, g: u64) bool {
        const self: *RamSet = @ptrCast(@alignCast(ctx.?));
        return self.intents.contains(g);
    }

    fn readFn(ctx: *anyopaque, vol_tag: u64, off: u64, dest: []u8) Error!void {
        const self: *RamSet = @ptrCast(@alignCast(ctx));
        const d = self.devs[if (vol_tag == VOL_A) 0 else 1];
        if (off + dest.len > d.len) return error.Io;
        @memcpy(dest, d[@intCast(off)..][0..dest.len]);
    }

    pub fn verifier(self: *RamSet) !scrub.Verifier {
        return scrub.Verifier.init(self.gpa, .{ .ctx = self, .read = &readFn }, CHUNK, &self.xforms);
    }

    fn allocOf(self: *RamSet, vol_tag: u64) *block_alloc.BlockAllocator {
        return self.allocs[if (vol_tag == VOL_A) 0 else 1];
    }

    // ---- the builders (every record through the product codecs) ----

    pub fn global(l: u64) u64 {
        return route_table.globalOfEffective(l);
    }

    pub fn local(global_: u64) u64 {
        return route_table.effectiveIno(global_);
    }

    fn mint(self: *RamSet) u64 {
        return self.mintAtomic();
    }

    pub fn mintAtomic(self: *RamSet) u64 {
        return self.next_local.fetchAdd(1, .acq_rel);
    }

    fn commitTx(self: *RamSet, t: *tx.KvTx) !void {
        try self.mem.commit(t);
    }

    fn putInode(self: *RamSet, t: *tx.KvTx, l: u64, v: record.InodeValue) !void {
        _ = self;
        try t.stagePut(key.TREE_INODES, &key.inodeKey(l), &v.encode());
    }

    fn dentryKeyFor(self: *RamSet, parent_local: u64, name: []const u8) [key.DENTRY_KEY_LEN]u8 {
        return key.dentryKey(parent_local, key.dentryNameHash54(name, self.mem.nameKey()), 0);
    }

    fn putDentry(self: *RamSet, t: *tx.KvTx, parent_local: u64, name: []const u8, child_global: u64, mode: u32) !void {
        const dv: record.DentryValue = .{ .child_ino = child_global, .file_type = economy.ftByte(mode), .name = name };
        var buf: [300]u8 = undefined;
        try t.stagePut(key.TREE_DENTRIES, &self.dentryKeyFor(parent_local, name), try dv.encodeInto(&buf));
    }

    /// A directory under `parent` (global): the record, the dentry, the
    /// parent's `nlink + 1`. Returns the GLOBAL ino.
    pub fn mkdir(self: *RamSet, parent: u64, name: []const u8) !u64 {
        const l = self.mint();
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2, .ctime = 7 });
        try self.putDentry(t, local(parent), name, global(l), economy.S.IFDIR);
        var pv = (try self.readInode(local(parent))).?;
        pv.nlink += 1;
        try self.putInode(t, local(parent), pv);
        try self.commitTx(t);
        return global(l);
    }

    /// A striped file whose inline map binds `blocks` (block index i →
    /// block `blocks[i]` of `vol_tag`): the inode, the dentry, the layout
    /// + its references through `publishLayout`, the allocator tracking
    /// each block at refcount 1 (+1 per further referencer), the device
    /// bytes patterned. Returns the GLOBAL ino.
    pub fn mkfile(self: *RamSet, parent: u64, name: []const u8, vol_tag: u64, blocks: []const u64) !u64 {
        const l = self.mint();
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .size = CHUNK * blocks.len, .ctime = 7 });
        try self.putDentry(t, local(parent), name, global(l), economy.S.IFREG);
        var entries: std.ArrayList(layout.MapEntryWire) = .empty;
        defer entries.deinit(self.gpa);
        var proofs: std.ArrayList(layout.DmaProof) = .empty;
        defer proofs.deinit(self.gpa);
        for (blocks, 0..) |b, i| {
            const m = layout.Mapping.wholeBlock(vol_tag, b * CHUNK, 0);
            try entries.append(self.gpa, (layout.MapEntry{ .block = @intCast(i), .mapping = m }).encode());
            try proofs.append(self.gpa, .{ .vol_tag = vol_tag, .offset = b * CHUNK, .len = CHUNK, .completed_ns = 1 });
            self.trackBlock(vol_tag, b);
            @memset(self.devs[if (vol_tag == VOL_A) 0 else 1][@intCast(b * CHUNK)..][0..@intCast(CHUNK)], @truncate(b + 1));
        }
        const lay = layout.Layout.stripedInline(CHUNK * blocks.len, entries.items);
        try layout.publishLayout(t, .{ .ino = l, .name_key = self.mem.nameKey(), .prev = null, .layout = lay, .proofs = proofs.items }, classes.refs_seam, self.geo);
        try self.commitTx(t);
        return global(l);
    }

    /// `mkdir` minted in guest slot `slot` at raw local `raw`: the directory's
    /// record AND the names it will hold live in the slot's tree (a lessee's
    /// directory under this mount's). Returns the global.
    pub fn mkdirGuest(self: *RamSet, slot: u16, raw: u64, parent: u64, name: []const u8) !u64 {
        const l = key.guestLocalIno(slot, raw);
        const g = inos.makeGlobalIno(raw, slot, inos.ROUTING_WIDTH);
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2, .ctime = 7 });
        try self.putDentry(t, local(parent), name, g, economy.S.IFDIR);
        var pv = (try self.readInode(local(parent))).?;
        pv.nlink += 1;
        try self.putInode(t, local(parent), pv);
        try self.commitTx(t);
        return g;
    }

    /// `mkfile` minted in guest slot `slot` at raw local `raw` (a lessee's
    /// record under this mount's directory): the records keyed by the
    /// guest key ino, the dentry naming the GLOBAL ino. Returns the global.
    pub fn mkfileGuest(self: *RamSet, slot: u16, raw: u64, parent: u64, name: []const u8, vol_tag: u64, blocks: []const u64) !u64 {
        const l = key.guestLocalIno(slot, raw);
        const g = inos.makeGlobalIno(raw, slot, inos.ROUTING_WIDTH);
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .size = CHUNK * blocks.len, .ctime = 7 });
        try self.putDentry(t, local(parent), name, g, economy.S.IFREG);
        var entries: std.ArrayList(layout.MapEntryWire) = .empty;
        defer entries.deinit(self.gpa);
        var proofs: std.ArrayList(layout.DmaProof) = .empty;
        defer proofs.deinit(self.gpa);
        for (blocks, 0..) |b, i| {
            const m = layout.Mapping.wholeBlock(vol_tag, b * CHUNK, 0);
            try entries.append(self.gpa, (layout.MapEntry{ .block = @intCast(i), .mapping = m }).encode());
            try proofs.append(self.gpa, .{ .vol_tag = vol_tag, .offset = b * CHUNK, .len = CHUNK, .completed_ns = 1 });
            self.trackBlock(vol_tag, b);
            @memset(self.devs[if (vol_tag == VOL_A) 0 else 1][@intCast(b * CHUNK)..][0..@intCast(CHUNK)], @truncate(b + 1));
        }
        const lay = layout.Layout.stripedInline(CHUNK * blocks.len, entries.items);
        try layout.publishLayout(t, .{ .ino = l, .name_key = self.mem.nameKey(), .prev = null, .layout = lay, .proofs = proofs.items }, classes.refs_seam, self.geo);
        try self.commitTx(t);
        return g;
    }

    /// A packed small file: ONE tenant window `[rel_off, rel_off + len)`
    /// of block `blk` on `vol_tag`.
    pub fn mkpacked(self: *RamSet, parent: u64, name: []const u8, vol_tag: u64, blk: u64, rel_off: u32, len: u32) !u64 {
        const l = self.mint();
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .size = len, .ctime = 7 });
        try self.putDentry(t, local(parent), name, global(l), economy.S.IFREG);
        const m = layout.Mapping.tenant(vol_tag, blk * CHUNK, 0, rel_off, len);
        const file_id: layout.FileId = @splat(@as(u8, @truncate(l)));
        // Packed is reached only by promotion of a staged file: the staged
        // record first, the tenant as its promotion (one tx, the later Put wins).
        const staged = layout.Layout.staged(len, file_id, null);
        try layout.publishLayout(t, .{ .ino = l, .name_key = self.mem.nameKey(), .prev = null, .layout = staged, .proofs = &.{} }, classes.refs_seam, self.geo);
        const lay = layout.Layout.packedTenant(len, file_id, m);
        self.trackBlock(vol_tag, blk);
        try layout.publishLayout(t, .{ .ino = l, .name_key = self.mem.nameKey(), .prev = staged, .layout = lay, .proofs = &.{.{ .vol_tag = vol_tag, .offset = blk * CHUNK, .len = CHUNK, .completed_ns = 1 }} }, classes.refs_seam, self.geo);
        try self.commitTx(t);
        return global(l);
    }

    /// The allocator tracks `blk` (one more referencer).
    pub fn trackBlock(self: *RamSet, vol_tag: u64, blk: u64) void {
        const a = self.allocOf(vol_tag);
        if (a.refcount(blk) == null) {
            _ = a.allocateSpecificBlock(blk) catch unreachable;
            a.publishBlock(blk);
        } else {
            _ = a.incrementRefcount(blk);
        }
    }

    /// A hard link `name` under `parent` to `child`: the dentry + `nlink + 1`.
    pub fn link(self: *RamSet, parent: u64, name: []const u8, child: u64) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        var v = (try self.readInode(local(child))).?;
        v.nlink += 1;
        try self.putInode(t, local(child), v);
        try self.putDentry(t, local(parent), name, child, v.mode);
        try self.commitTx(t);
    }

    pub fn readInode(self: *RamSet, l: u64) !?record.InodeValue {
        const v = (try self.mem.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(l))) orelse return null;
        defer self.gpa.free(v);
        return try record.InodeValue.decode(v);
    }

    pub fn count(self: *RamSet, kind: u8) usize {
        return self.mem.countKind(kind);
    }

    // ---- the defect injectors ----

    /// C9: drop the name, keep the live record.
    pub fn orphan(self: *RamSet, parent: u64, name: []const u8) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try t.stageDelete(key.TREE_DENTRIES, &self.dentryKeyFor(local(parent), name));
        try self.commitTx(t);
    }

    /// C10: the record's nlink set to `n` with no name change.
    pub fn setNlink(self: *RamSet, child: u64, n: u32) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        var v = (try self.readInode(local(child))).?;
        v.nlink = n;
        try self.putInode(t, local(child), v);
        try self.commitTx(t);
    }

    /// C10 dangling: a name under `parent` naming a child with no record.
    pub fn dangle(self: *RamSet, parent: u64, name: []const u8, child_global: u64) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putDentry(t, local(parent), name, child_global, economy.S.IFREG);
        try self.commitTx(t);
    }

    /// C2 leaked: the allocator tracks a block nothing references.
    pub fn leak(self: *RamSet, vol_tag: u64, blk: u64) void {
        self.trackBlock(vol_tag, blk);
    }

    /// C2 lost: the allocator forgets a referenced block (its content stays).
    pub fn untrack(self: *RamSet, vol_tag: u64, blk: u64) void {
        const a = self.allocOf(vol_tag);
        while (a.refcount(blk) != null) _ = a.freeBlock(blk) catch unreachable;
    }

    /// C3: the wrong refcount.
    pub fn setRefcount(self: *RamSet, vol_tag: u64, blk: u64, n: u32) void {
        self.allocOf(vol_tag).fsckSetRefcount(blk, n);
    }

    /// C8 (durable < derived): drop the reference record of `(child, index)`.
    pub fn dropRef(self: *RamSet, child: u64, index: u32, vol_tag: u64, blk: u64) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try t.stageDelete(key.TREE_BLOCK_REFS, &key.blockRefKey(vol_tag, blk, local(child), index));
        try self.commitTx(t);
    }

    /// C8 (durable > derived): a stray reference record.
    pub fn strayRef(self: *RamSet, owner_local: u64, index: u32, vol_tag: u64, blk: u64) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = vol_tag, .block_idx = blk, .owner_ino = owner_local, .block_index = index })});
        try self.commitTx(t);
    }

    /// C1 semantic: an inode record whose value is one byte short.
    pub fn tearInode(self: *RamSet, child: u64) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        const v = (try self.readInode(local(child))).?;
        const enc = v.encode();
        try t.stagePut(key.TREE_INODES, &key.inodeKey(local(child)), enc[0 .. enc.len - 1]);
        try self.commitTx(t);
    }

    /// C1 raw key: a kind-1 row whose key is the inode key plus a byte.
    pub fn rawKey(self: *RamSet) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        var k: [key.INODE_KEY_LEN + 1]u8 = undefined;
        k[0..key.INODE_KEY_LEN].* = key.inodeKey(99);
        k[key.INODE_KEY_LEN] = 0;
        try t.stagePut(key.TREE_INODES, &k, &(record.InodeValue{ .mode = economy.S.IFREG, .nlink = 1 }).encode());
        try self.commitTx(t);
    }

    /// C1 torn: the dentry walk fails from `parent`'s first key.
    pub fn tearAt(self: *RamSet, parent: u64) void {
        self.fault.tear_at = .{ .kind = key.TREE_DENTRIES, .from = key.KeyBuf.fromSlice(&key.dentryKey(local(parent), 0, 0)) };
    }

    /// C7: flip the device bytes of `blk`.
    pub fn corrupt(self: *RamSet, vol_tag: u64, blk: u64) void {
        const d = self.devs[if (vol_tag == VOL_A) 0 else 1];
        for (d[@intCast(blk * CHUNK)..][0..64]) |*b| b.* ^= 0xFF;
    }

    /// C12 overrun: a packed tenant whose window breaks the law (`rel_off`
    /// off the grain) — written RAW (the codec refuses it).
    pub fn overrunTenant(self: *RamSet, parent: u64, name: []const u8, vol_tag: u64, blk: u64) !u64 {
        const l = self.mint();
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .size = 100, .ctime = 7 });
        try self.putDentry(t, local(parent), name, global(l), economy.S.IFREG);
        var m = layout.Mapping.tenant(vol_tag, blk * CHUNK, 0, 4096, 100);
        const lay = layout.Layout.packedTenant(100, @splat(1), m);
        var buf: [256]u8 = undefined;
        const enc = lay.encodeInto(&buf);
        self.trackBlock(vol_tag, blk);
        // Patch the window: rel_off 4097 (off the grain) in place.
        m.rel_off = 4097;
        const off = layout.LAYOUT_HEADER_LEN + 16;
        enc[off..][0..layout.MAPPING_WIRE_LEN].* = std.mem.toBytes(m.encode());
        const xv: record.XattrValue = .{ .name = layout.LAYOUT_XATTR_NAME, .value = enc };
        var xbuf: [300]u8 = undefined;
        try t.stagePut(key.TREE_XATTRS, &key.xattrKey(l, key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.mem.nameKey()), 0), try xv.encodeInto(&xbuf));
        // The base IS referenced: its durable record exists as any publish's would.
        try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = vol_tag, .block_idx = blk, .owner_ino = l, .block_index = 0 })});
        try self.commitTx(t);
        return global(l);
    }

    /// C11 (a): kind-7 records under an owner with no kvmap head.
    pub fn orphanMapRecords(self: *RamSet, owner_local: u64, n: u32) !void {
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        var i: u32 = 0;
        while (i < n) : (i += 1) try kvmap.stagePut(t, owner_local, kvmap.Rec.point(i, layout.Mapping.wholeBlock(VOL_A, (40 + i) * CHUNK, 0)));
        try self.commitTx(t);
    }

    /// C11 (b): a sweep-less kvmap head with a size and no records.
    pub fn emptyKvmapHead(self: *RamSet, parent: u64, name: []const u8) !u64 {
        const l = self.mint();
        const t = try self.mem.begin(@src());
        errdefer self.mem.abandon(t);
        try self.putInode(t, l, .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .size = CHUNK * 100, .ctime = 7 });
        try self.putDentry(t, local(parent), name, global(l), economy.S.IFREG);
        const lay = layout.Layout.stripedKvmap(CHUNK * 100, null, 1);
        try layout.publishLayout(t, .{ .ino = l, .name_key = self.mem.nameKey(), .prev = null, .layout = lay, .proofs = &.{} }, classes.refs_seam, self.geo);
        try self.commitTx(t);
        return global(l);
    }
};

/// Run the engine over the set and return the report (the caller owns it).
pub fn run(set: *RamSet, mode: finding.Mode, opts: anytype) !finding.Report {
    var h = set.host(mode);
    return runHost(set, &h, opts);
}

/// A run that confirms findings is LOUD by contract (`fsck_findings` and
/// the class tripwires fire; C8's verdict site logs its owners). The
/// tripwires rate-limit to one line per site per second, so the exact line
/// count is a clock's: the budget is armed wide with every site's budget
/// reset so each run's first fire logs, and the run asserts the loudness
/// happened whenever it reported a finding, then drains the budget.
pub fn runHost(set: *RamSet, h: *classes.Host, opts: anytype) !finding.Report {
    const runner = @import("runner.zig");
    const block_refs_stats = @import("../../data/block_refs.zig").stats;
    const bitmap_stats = @import("../../data/sym_planes/data_bitmap.zig").stats;
    const log = @import("../../core/log.zig");
    const fsck = @import("../fsck.zig");
    var v = try set.verifier();
    defer v.deinit();
    var o: runner.Options = .{};
    inline for (@typeInfo(@TypeOf(opts)).@"struct".field_names) |name| @field(o, name) = @field(opts, name);
    fsck.stats.fsck_findings.resetLogBudgetForTest();
    fsck.stats.fsck_tenant_overlap_findings.resetLogBudgetForTest();
    block_refs_stats.meta_kv_block_refs_drift.resetLogBudgetForTest();
    bitmap_stats.data_alloc_bitmap_drift.resetLogBudgetForTest();
    log.testing.expectErrorLogs(256);
    const r = try runner.run(h, o, &v);
    if (r.hasFindings()) try testing.expect(log.testing.capturedErrorLogs() >= 1);
    log.testing.expectErrorLogs(0);
    return r;
}

/// The classes the report names, in order.
pub fn classesOf(gpa: Allocator, r: finding.Report) ![]finding.Class {
    const out = try gpa.alloc(finding.Class, r.findings.len);
    for (r.findings, out) |f, *o| o.* = f.class();
    return out;
}

pub fn expectClasses(r: finding.Report, want: []const finding.Class) !void {
    if (r.findings.len != want.len) {
        std.debug.print("\nfindings ({d}):\n", .{r.findings.len});
        for (r.findings) |f| std.debug.print("  {f}\n", .{f});
        return error.TestExpectedEqual;
    }
    for (r.findings, want) |f, w| if (f.class() != w) {
        std.debug.print("\nfindings:\n", .{});
        for (r.findings) |x| std.debug.print("  {f}\n", .{x});
        return error.TestExpectedEqual;
    };
}
