//! The first real filesystem: the POSIX must-implement set over the KV
//! metadata backend, for the single-writer class on one volume (charter
//! §5.1 keys and records, §5.2 journal economy, §6.1 "Inline"; `docs/
//! porting/fuse_handlers_lanes.md` §5 R3 column; `fuse_abi_session.md`
//! §6, §13 KILLPRIV_V2). Rung 3's first mount: every rung-4 layout
//! (staged, packed, striped) is absent, so a file's bytes live INLINE in
//! its layout record and a write past the inline bound is refused.
//!
//! Generic over a `Store` (comptime duck-typed, `assertStore`): the Linux
//! `meta/backend.KvBackend` is the product binding (`handler.BackendFs`);
//! the in-RAM `MemStore` below runs the same POSIX suite on every host.
//!
//! Lock discipline (charter §2): this module is BELOW level (1) — the
//! lifecycle took the op guard — and takes the 4a object locks `I{ino}` /
//! `D{parent,name}` through `dlm.Manager.lockMany` as ONE canonical set per
//! op; an op that discovers its targets mid-flight (unlink's child, rename's
//! victim) reads first WITHOUT guards, locks the full set, then REVALIDATES
//! under the guards (the dentry still names the same child) before staging.
//! The held set travels into the transaction (`KvTx.holdGuards`) and the
//! conveyor drops it at the terminal outcome BEFORE the ack (D-3); a tx
//! abandoned before commit drops it in `deinit`.
//!
//! Records: inode `ino ‖ 01` (`record.InodeValue`), dentry `parent ‖ 02 ‖
//! hash54 ‖ coll` (the GLOBAL child ino), xattr `ino ‖ 03 ‖ hash56 ‖ coll`
//! — user xattrs, `system.symlink` (the target) and `layout` (the inline
//! data) share the kind; the two reserved names are screened from every
//! xattr op. The inline layout record: `InlineLayout{magic "SQZL", version
//! 1, kind 1 = inline, data_len} ‖ data` — rung 4's layout record (an
//! `extern struct` with a canonical total codec, decision 0002 row 2)
//! supersedes it; no release has shipped, so nothing is owed to this shape.
//! `inline_max_bytes` is DERIVED to one page (charter §6.1: raising it was
//! measured — 16 KiB inline −34 % files/s and 59× metadata bytes per file).
//!
//! Times are i64-ns stamps from the coarse realtime clock; the SETATTR echo
//! is ABSORBED through `economy.PendingTimes` (never blanket suppression);
//! nlink/ctime/mtime move only through `economy.stage*`. Readdir cookies are
//! `key.encodeReaddirCookie` (`3 + (hash54 << 8 | coll)`); `.` is offset 1,
//! `..` offset 2 — the parent of a directory is a RAM memo filled by every
//! lookup/mkdir/rename (the KV holds no parent pointer; the kernel never
//! asks about a directory it has not looked up, except the root, whose
//! `..` is itself).
//!
//! KILLPRIV_V2 (fuse_abi_session.md §13): on a WRITE flagged `FUSE_WRITE_
//! KILL_SUIDGID`, a SETATTR flagged `FATTR_KILL_SUIDGID` (truncate, chown)
//! and an O_TRUNC OPEN flagged `FUSE_OPEN_KILL_SUIDGID`: `S_ISUID` is always
//! cleared, `S_ISGID` only when `S_IXGRP` is set (sgid without group-exec is
//! the mandatory-locking marker — preserved), and `security.capability` is
//! dropped if present — folded into the op's ONE commit (`fuse_killpriv_
//! clears`). Without V2 the kernel sends a `GETXATTR("security.capability")`
//! per `write(2)` — half of every write-syscall-bound stream's requests.
//!
//! Unlinked corpses (`nlink == 0`) survive until the kernel's final FORGET
//! (an unlinked-open file is still readable), then join a RECLAIM batch
//! whose destroy is ONE journal entry per `RECLAIM_BATCH` inos
//! (`inos.stageDestroy`; the batched-destroy economy, `unlink ≤ 1.05
//! entries/op`). A forgotten corpse is unreachable by name and unknown to
//! the kernel, so its destroy needs no 4a guard.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const assert = std.debug.assert;

const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const mount_options = @import("../mount_options.zig");
const filesystem = @import("filesystem.zig");
const errors = @import("../../core/errors.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tx = @import("../../meta/tx.zig");
const tree = @import("../../meta/tree.zig");
const dlm = @import("../../meta/dlm.zig");
const economy = @import("../../meta/economy.zig");
const inos = @import("../../meta/inos.zig");
const jc = @import("../../meta/journal_core.zig");

const Ctx = filesystem.Ctx;
const Error = filesystem.Error;
const InodeValue = record.InodeValue;
const S = economy.S;

/// The xattr that carries a file's layout record (charter §5.1).
pub const LAYOUT_XATTR: []const u8 = "layout";
/// The Linux capability xattr KILLPRIV_V2 drops.
pub const CAPS_XATTR: []const u8 = "security.capability";
/// `NAME_MAX`: `f_namelen` and every name op's ENAMETOOLONG bound.
pub const NAME_MAX: usize = record.NAME_MAX;
/// Corpses per destroy entry: the reclaim gather's measured fill (≈ 62 at
/// 100 k-op storms), so `1 + 1/64 + drain ≤ 1.05` entries per unlink.
pub const RECLAIM_BATCH: usize = 64;
/// `statfs.f_frsize`/`f_bsize`: the page — the unit the kernel's `df`
/// arithmetic expects and the inline bound's unit.
pub const STATFS_BSIZE: u32 = 4096;
/// `f_ffree`: inos are a monotone 64-bit watermark, never reused; the
/// report is the remaining watermark space (an over-report, never under).
pub const INO_SPACE: u64 = 1 << 56;

/// `inline_max_bytes`: one page (module doc).
pub fn inlineMaxBytes(page_size: u32) u64 {
    return page_size;
}

/// `renameat2(2)` flags (uapi `linux/fs.h`; `std.os.linux` carries none).
pub const RENAME_NOREPLACE: u32 = 1 << 0;
pub const RENAME_EXCHANGE: u32 = 1 << 1;
pub const RENAME_WHITEOUT: u32 = 1 << 2;
/// `setxattr(2)` flags (uapi `linux/xattr.h`).
pub const XATTR_CREATE: u32 = 0x1;
pub const XATTR_REPLACE: u32 = 0x2;
/// `O_TRUNC` as the kernel's open-flag word carries it.
pub const O_TRUNC: u32 = @bitCast(linux.O{ .TRUNC = true });

// ---------------------------------------------------------------------------
// The inline layout record.
// ---------------------------------------------------------------------------

pub const InlineLayoutWire = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    kind: u8,
    reserved: u8,
    data_len_le: [4]u8,
};

pub const INLINE_LAYOUT_HEADER_LEN: usize = @sizeOf(InlineLayoutWire);
pub const INLINE_LAYOUT_MAGIC = "SQZL".*;
pub const INLINE_LAYOUT_VERSION: u16 = 1;
pub const LAYOUT_KIND_INLINE: u8 = 1;

comptime {
    assert(INLINE_LAYOUT_HEADER_LEN == 12);
}

pub const InlineLayout = struct {
    data: []const u8,

    pub fn encodedLen(self: InlineLayout) usize {
        return INLINE_LAYOUT_HEADER_LEN + self.data.len;
    }

    /// The header for `data_len` bytes that already sit at `out[12..]`
    /// (the write path builds the image in place).
    pub fn writeHeader(out: []u8, data_len: usize) []u8 {
        const n = INLINE_LAYOUT_HEADER_LEN + data_len;
        assert(out.len >= n);
        var w: InlineLayoutWire = .{ .magic = INLINE_LAYOUT_MAGIC, .version_le = undefined, .kind = LAYOUT_KIND_INLINE, .reserved = 0, .data_len_le = undefined };
        std.mem.writeInt(u16, &w.version_le, INLINE_LAYOUT_VERSION, .little);
        std.mem.writeInt(u32, &w.data_len_le, @intCast(data_len), .little);
        out[0..INLINE_LAYOUT_HEADER_LEN].* = std.mem.toBytes(w);
        return out[0..n];
    }

    pub fn encodeInto(self: InlineLayout, out: []u8) []u8 {
        const enc = writeHeader(out, self.data.len);
        @memcpy(enc[INLINE_LAYOUT_HEADER_LEN..], self.data);
        return enc;
    }

    /// Total: exactly the encoder's form decodes (magic, version, kind,
    /// zero reserved, `data_len` ≡ the remainder).
    pub fn decode(bytes: []const u8) record.DecodeError!InlineLayout {
        if (bytes.len < INLINE_LAYOUT_HEADER_LEN) return error.Corrupt;
        const w = std.mem.bytesToValue(InlineLayoutWire, bytes[0..INLINE_LAYOUT_HEADER_LEN]);
        if (!std.mem.eql(u8, &w.magic, &INLINE_LAYOUT_MAGIC)) return error.Corrupt;
        if (std.mem.readInt(u16, &w.version_le, .little) != INLINE_LAYOUT_VERSION) return error.Corrupt;
        if (w.kind != LAYOUT_KIND_INLINE or w.reserved != 0) return error.Corrupt;
        const len = std.mem.readInt(u32, &w.data_len_le, .little);
        if (bytes.len - INLINE_LAYOUT_HEADER_LEN != len) return error.Corrupt;
        return .{ .data = bytes[INLINE_LAYOUT_HEADER_LEN..] };
    }
};

// ---------------------------------------------------------------------------
// KILLPRIV_V2.
// ---------------------------------------------------------------------------

/// `kill_suidgid_mode`: suid always; sgid iff group-exec.
pub fn killSuidgidMode(mode: u32) u32 {
    var m = mode & ~@as(u32, S.ISUID);
    if (m & S.IXGRP != 0) m &= ~@as(u32, S.ISGID);
    return m;
}

// ---------------------------------------------------------------------------
// Instruments.
// ---------------------------------------------------------------------------

/// `fuse_write_beyond_inline_refused`: WRITE/truncate past `inline_max`
/// answered EFBIG — rung 3's honest ceiling, not a bug (rung 4's layouts
/// lift it).
pub var fuse_write_beyond_inline_refused: counters.ShardedCounter = .{};
/// `fuse_killpriv_clears`: KILLPRIV_V2 strips folded into a commit.
pub var fuse_killpriv_clears: counters.ShardedCounter = .{};
/// `fuse_killpriv_negotiated`: 0/1 from the INIT reply.
pub var fuse_killpriv_negotiated: counters.Gauge = .{};
/// `fuse_reclaim_destroys`: corpses destroyed; `fuse_reclaim_drains`:
/// destroy entries committed (the batched-destroy economy's two words).
pub var fuse_reclaim_destroys: counters.ShardedCounter = .{};
pub var fuse_reclaim_drains: counters.ShardedCounter = .{};
/// `fuse_reclaim_queue_overflow`: a forget found the gather full and drove
/// its corpse inline.
pub var fuse_reclaim_queue_overflow: counters.ShardedCounter = .{};
/// A dentry that names an ino whose record is gone (a corpse reachable by
/// name): fsck C9/C10 territory, never a handler outcome (must stay 0).
pub var dentry_without_inode: counters.Tripwire = .init("fuse_dentry_without_inode");

// ---------------------------------------------------------------------------
// The store contract.
// ---------------------------------------------------------------------------

pub fn assertStore(comptime T: type) void {
    comptime {
        for ([_][]const u8{ "StoreError", "coreError", "begin", "abandon", "commit", "lookup", "range", "fsync", "allocateIno", "nextIno", "hashSeed", "nodeSize", "space", "isFailed" }) |name| {
            if (!@hasDecl(T, name)) @compileError(@typeName(T) ++ " is not a kv_fs Store: lacks `" ++ name ++ "`");
        }
    }
}

pub const Options = struct {
    ttls: mount_options.Ttls = .{},
    /// The page: `inline_max_bytes` and `fuse_attr.blksize`.
    page_size: u32 = 4096,
    /// The 4a stripe width (`dlm.resolveWidth` at the daemon).
    dlm_stripes: usize = dlm.dlm_stripes_shipped,
};

/// A held 4a set travelling with a transaction.
const HeldGuards = struct {
    gpa: Allocator,
    io: Io,
    set: dlm.GuardSet,

    fn releaseFn(ctx: *anyopaque) void {
        const self: *HeldGuards = @ptrCast(@alignCast(ctx));
        self.set.release(self.io);
        self.gpa.destroy(self);
    }

    fn guardSet(self: *HeldGuards) tx.GuardSet {
        return tx.GuardSet.init(self, &releaseFn);
    }
};

/// A dentry as found: the record's position plus the child's two inos.
const Found = struct {
    pos: economy.DentryPos,
    local: u64,
    global: u64,

    fn isDir(self: Found) bool {
        return @as(u32, self.pos.file_type) << 12 == S.IFDIR;
    }
};

const ChainBuf = struct {
    keys: [256][]const u8 = undefined,
    len: usize = 0,

    fn slice(self: *const ChainBuf) []const []const u8 {
        return self.keys[0..self.len];
    }
};

pub fn KvFs(comptime Store: type) type {
    assertStore(Store);
    return struct {
        const Self = @This();

        gpa: Allocator,
        io: Io,
        store: *Store,
        ttls: mount_options.Ttls,
        inline_max: u64,
        blksize: u32,
        pending: economy.PendingTimes,
        locks: dlm.Manager,
        /// The `..` memo (module doc).
        parents_lock: jc.SpinLock = .{},
        parents: std.AutoHashMapUnmanaged(u64, u64) = .empty,
        /// The reclaim gather.
        reclaim_lock: jc.SpinLock = .{},
        reclaim: std.ArrayList(u64) = .empty,
        next_fh: std.atomic.Value(u64) = .init(1),
        /// From the INIT reply.
        minor: u32 = abi.FUSE_KERNEL_MINOR_VERSION,
        killpriv_v2: bool = false,

        /// Bind the store (the daemon's mount; `init` is the FUSE op).
        pub fn mount(gpa: Allocator, io: Io, store: *Store, opts: Options) Allocator.Error!Self {
            var self: Self = .{
                .gpa = gpa,
                .io = io,
                .store = store,
                .ttls = opts.ttls,
                .inline_max = inlineMaxBytes(opts.page_size),
                .blksize = opts.page_size,
                .pending = try economy.PendingTimes.init(gpa),
                .locks = undefined,
            };
            errdefer self.pending.deinit();
            self.locks = try dlm.Manager.init(gpa, opts.dlm_stripes);
            errdefer self.locks.deinit();
            try self.reclaim.ensureTotalCapacity(gpa, 4 * RECLAIM_BATCH);
            return self;
        }

        pub fn unmount(self: *Self) void {
            self.reclaim.deinit(self.gpa);
            self.parents.deinit(self.gpa);
            self.locks.deinit();
            self.pending.deinit();
            self.* = undefined;
        }

        // ---- helpers ----

        fn now() u64 {
            return @bitCast(clock.realtimeCoarseNs());
        }

        fn storeErr(e: Store.StoreError) Error {
            return Store.coreError(e);
        }

        fn globalOf(local: u64) u64 {
            return inos.makeGlobalIno(local, 0, inos.ROUTING_WIDTH);
        }

        /// A kernel nodeid → this volume's local ino. A nodeid routing to a
        /// slot this single volume does not hold (garbage, or another set's
        /// ino) is ENOENT — never an alias of a real local record.
        fn localOf(global: u64) Error!u64 {
            const r = inos.routeIno(global, inos.ROUTING_WIDTH);
            if (r.slot != 0) return error.FileNotFound;
            return r.local;
        }

        fn checkName(name: []const u8) Error!void {
            if (name.len == 0) return error.InvalidArgument;
            if (name.len > NAME_MAX) return error.NameTooLong;
        }

        fn isReservedXattr(name: []const u8) bool {
            return std.mem.eql(u8, name, LAYOUT_XATTR) or std.mem.eql(u8, name, economy.SYMLINK_XATTR);
        }

        /// The inode as stored, FOLDED through the pending refinements;
        /// null when absent.
        fn readInodeOpt(self: *Self, ino: u64) Error!?InodeValue {
            const bytes = (self.store.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return storeErr(e)) orelse return null;
            defer self.gpa.free(bytes);
            var v = InodeValue.decode(bytes) catch return error.InputOutput;
            self.pending.fold(ino, &v);
            return v;
        }

        fn readInode(self: *Self, ino: u64) Error!InodeValue {
            return (try self.readInodeOpt(ino)) orelse error.FileNotFound;
        }

        fn readDir(self: *Self, ino: u64) Error!InodeValue {
            const v = try self.readInode(ino);
            if (!economy.isDirMode(v.mode)) return error.NotDir;
            return v;
        }

        fn attrOf(self: *Self, global: u64, v: InodeValue) abi.fuse_attr {
            return codec.attrFromInode(global, v, self.blksize);
        }

        fn entryOf(self: *Self, global: u64, v: InodeValue) filesystem.Entry {
            return .{ .nodeid = global, .entry_ttl_ns = self.ttls.entryTtlFor(economy.isDirMode(v.mode)), .attr_ttl_ns = self.ttls.attr_ns, .attr = self.attrOf(global, v) };
        }

        fn attrReply(self: *Self, global: u64, v: InodeValue) filesystem.Attr {
            return .{ .attr_ttl_ns = self.ttls.attr_ns, .attr = self.attrOf(global, v) };
        }

        /// The live chain of `(parent, hash54(name))`: keys into `buf`, the
        /// matching dentry (by FULL name) if any.
        fn scanChain(self: *Self, parent: u64, name: []const u8, buf: *ChainBuf, kvs: *std.ArrayList(tree.KeyValue)) Error!?Found {
            const r = tx.dentryChainRange(parent, key.dentryNameHash54(name, self.store.hashSeed()));
            self.store.range(self.gpa, key.TREE_DENTRIES, &r.start, &r.end, 256, kvs) catch |e| return storeErr(e);
            var found: ?Found = null;
            buf.len = 0;
            for (kvs.items) |kv| {
                buf.keys[buf.len] = kv.key;
                buf.len += 1;
                const dv = record.DentryValue.decode(kv.value) catch return error.InputOutput;
                if (std.mem.eql(u8, dv.name, name)) {
                    var pos: economy.DentryPos = .{ .key = undefined, .child_ino = dv.child_ino, .file_type = dv.file_type };
                    @memcpy(&pos.key, kv.key[0..key.DENTRY_KEY_LEN]);
                    found = .{ .pos = pos, .local = try localOf(dv.child_ino), .global = dv.child_ino };
                }
            }
            return found;
        }

        fn findDentry(self: *Self, parent: u64, name: []const u8) Error!?Found {
            var buf: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            return self.scanChain(parent, name, &buf, &kvs);
        }

        /// Whether a directory holds any entry.
        fn dirHasEntries(self: *Self, dir: u64) Error!bool {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const start = key.dentryKey(dir, 0, 0);
            const end = key.dentryKey(dir, key.HASH54_MAX, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_DENTRIES, &start, &end, 1, &kvs) catch |e| return storeErr(e);
            return kvs.items.len != 0;
        }

        /// The xattr chain of `(ino, hash56(name))`: the matching record
        /// (by full name) and the chain's occupied `coll_seq`s.
        const XattrHit = struct { k: [key.XATTR_KEY_LEN]u8, value: []u8 };

        fn findXattr(self: *Self, ino: u64, name: []const u8, occupied: *[256]u8, occupied_len: *usize) Error!?XattrHit {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const h = key.xattrNameHash56(name, self.store.hashSeed());
            const start = key.xattrKey(ino, h, 0);
            const end = key.xattrKey(ino, h, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch |e| return storeErr(e);
            occupied_len.* = 0;
            var hit: ?XattrHit = null;
            for (kvs.items) |kv| {
                occupied[occupied_len.*] = kv.key[kv.key.len - 1];
                occupied_len.* += 1;
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (hit == null and std.mem.eql(u8, xv.name, name)) {
                    var k: [key.XATTR_KEY_LEN]u8 = undefined;
                    @memcpy(&k, kv.key[0..key.XATTR_KEY_LEN]);
                    hit = .{ .k = k, .value = try self.gpa.dupe(u8, xv.value) };
                }
            }
            return hit;
        }

        /// The xattr value by name (owned), null when absent.
        fn xattrValue(self: *Self, ino: u64, name: []const u8) Error!?[]u8 {
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const hit = (try self.findXattr(ino, name, &occ, &n)) orelse return null;
            return hit.value;
        }

        /// The key a NEW xattr `name` occupies, or the existing record's.
        fn xattrKeyFor(self: *Self, ino: u64, name: []const u8) Error!struct { k: [key.XATTR_KEY_LEN]u8, existing: ?[]u8 } {
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            if (try self.findXattr(ino, name, &occ, &n)) |hit| return .{ .k = hit.k, .existing = hit.value };
            const coll = key.firstFreeCollSeq(occ[0..n]) orelse return error.NoSpaceLeft;
            return .{ .k = key.xattrKey(ino, key.xattrNameHash56(name, self.store.hashSeed()), coll), .existing = null };
        }

        fn stageXattrPut(self: *Self, t: *tx.KvTx, k: *const [key.XATTR_KEY_LEN]u8, name: []const u8, value: []const u8) Error!void {
            const buf = try self.gpa.alloc(u8, 1 + name.len + value.len);
            defer self.gpa.free(buf);
            const enc = (record.XattrValue{ .name = name, .value = value }).encodeInto(buf) catch return error.NameTooLong;
            try t.stagePut(key.TREE_XATTRS, k, enc);
        }

        fn lockPlan(self: *Self, plan: *const economy.GuardPlan) Error!*HeldGuards {
            const held = try self.gpa.create(HeldGuards);
            errdefer self.gpa.destroy(held);
            held.* = .{ .gpa = self.gpa, .io = self.io, .set = undefined };
            held.set = self.locks.lockManyUncancelable(self.io, plan.inodes(), plan.dentries()) catch return error.InvalidArgument;
            return held;
        }

        fn lockInodes(self: *Self, reqs: []const dlm.InodeReq) Error!*HeldGuards {
            const held = try self.gpa.create(HeldGuards);
            errdefer self.gpa.destroy(held);
            held.* = .{ .gpa = self.gpa, .io = self.io, .set = undefined };
            held.set = self.locks.lockManyUncancelable(self.io, reqs, &.{}) catch return error.InvalidArgument;
            return held;
        }

        /// A transaction carrying `held`: from here the guards drop with the
        /// tx (abandon or terminal outcome), never twice.
        fn beginWith(self: *Self, held: *HeldGuards, site: tx.Site) Error!*tx.KvTx {
            const t = self.store.begin(site) catch |e| {
                HeldGuards.releaseFn(held);
                return e;
            };
            t.holdGuards(held.guardSet());
            return t;
        }

        fn commit(self: *Self, c: *Ctx, t: *tx.KvTx) Error!void {
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            self.store.commit(t) catch |e| return storeErr(e);
        }

        fn rememberParent(self: *Self, dir: u64, parent: u64) void {
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            self.parents.put(self.gpa, dir, parent) catch {};
        }

        fn parentOf(self: *Self, dir: u64) ?u64 {
            if (dir == inos.ROOT_INO) return inos.ROOT_INO;
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            return self.parents.get(dir);
        }

        fn forgetParent(self: *Self, dir: u64) void {
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            _ = self.parents.remove(dir);
        }

        fn regularOpenFlags(self: *const Self) u32 {
            var f: u32 = abi.FOPEN_KEEP_CACHE;
            // FOPEN_NOFLUSH: fuse ≥ 7.35; FOPEN_PARALLEL_DIRECT_WRITES: ≥ 7.38.
            if (self.minor >= 35) f |= abi.FOPEN_NOFLUSH;
            if (self.minor >= 38) f |= abi.FOPEN_PARALLEL_DIRECT_WRITES;
            return f;
        }

        fn mintFh(self: *Self) u64 {
            return self.next_fh.fetchAdd(1, .monotonic);
        }

        /// The inline data record of `ino` (owned), null when never written.
        fn readLayout(self: *Self, ino: u64) Error!?[]u8 {
            return self.xattrValue(ino, LAYOUT_XATTR);
        }

        /// KILLPRIV_V2 fold for a tx that is rewriting `v`: the mode strip
        /// and the caps drop, counted once when anything moved.
        fn killpriv(self: *Self, t: *tx.KvTx, ino: u64, v: *InodeValue) Error!void {
            const stripped = killSuidgidMode(v.mode);
            var moved = stripped != v.mode;
            v.mode = stripped;
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            if (try self.findXattr(ino, CAPS_XATTR, &occ, &n)) |hit| {
                self.gpa.free(hit.value);
                try t.stageDelete(key.TREE_XATTRS, &hit.k);
                moved = true;
            }
            if (moved) fuse_killpriv_clears.inc();
        }

        /// Truncate `ino` to `size` inside `t`: the inline record clipped
        /// (or deleted at 0), the inode `Put` the caller stages.
        fn stageTruncate(self: *Self, t: *tx.KvTx, ino: u64, size: u64) Error!void {
            if (size > self.inline_max) {
                fuse_write_beyond_inline_refused.inc();
                return error.FileTooBig;
            }
            const cur = (try self.readLayout(ino)) orelse return;
            defer self.gpa.free(cur);
            const lay = InlineLayout.decode(cur) catch return error.InputOutput;
            if (size >= lay.data.len) return;
            const k = try self.xattrKeyFor(ino, LAYOUT_XATTR);
            if (k.existing) |e| self.gpa.free(e);
            if (size == 0) return t.stageDelete(key.TREE_XATTRS, &k.k);
            const buf = try self.gpa.alloc(u8, INLINE_LAYOUT_HEADER_LEN + @as(usize, @intCast(size)));
            defer self.gpa.free(buf);
            const enc = (InlineLayout{ .data = lay.data[0..@intCast(size)] }).encodeInto(buf);
            try self.stageXattrPut(t, &k.k, LAYOUT_XATTR, enc);
        }

        // ---- the ops ----

        pub fn init(self: *Self, _: *Ctx, a: filesystem.InitArgs) Error!void {
            self.minor = a.minor;
            self.killpriv_v2 = a.has(abi.FUSE_HANDLE_KILLPRIV_V2);
            fuse_killpriv_negotiated.set(@intFromBool(self.killpriv_v2));
            log.info("kv_fs: FUSE 7.{d}; KILLPRIV_V2 {s}; inline_max {d} B", .{ self.minor, if (self.killpriv_v2) "negotiated" else "absent", self.inline_max });
        }

        pub fn destroy(self: *Self, c: *Ctx, _: filesystem.DestroyArgs) void {
            // The dismount sweep: every gathered corpse destroyed.
            self.reclaimDrain(c) catch |e| log.err("kv_fs: dismount reclaim drain failed: {t}", .{e});
        }

        pub fn lookup(self: *Self, c: *Ctx, a: filesystem.LookupArgs) Error!filesystem.Entry {
            try checkName(a.name);
            if (std.mem.eql(u8, a.name, ".")) {
                const v = try self.readDir(try localOf(a.parent));
                return self.entryOf(a.parent, v);
            }
            if (std.mem.eql(u8, a.name, "..")) {
                const p = self.parentOf(a.parent) orelse return error.FileNotFound;
                const v = try self.readDir(try localOf(p));
                return self.entryOf(p, v);
            }
            c.stage(.meta_backend_start);
            const found = (try self.findDentry(try localOf(a.parent), a.name)) orelse {
                c.stage(.meta_backend_done);
                if (!self.ttls.negativeEnabled()) return error.FileNotFound;
                return filesystem.Entry.negative(self.ttls.negative_ns);
            };
            const v = (try self.readInodeOpt(found.local)) orelse {
                dentry_without_inode.fire("lookup found a dentry whose inode record is absent");
                return error.InputOutput;
            };
            c.stage(.meta_backend_done);
            if (economy.isDirMode(v.mode)) self.rememberParent(found.global, a.parent);
            return self.entryOf(found.global, v);
        }

        pub fn forget(self: *Self, c: *Ctx, a: filesystem.ForgetArgs) void {
            if (!a.evicted) return;
            self.forgetParent(a.ino);
            const local = localOf(a.ino) catch return;
            const v = (self.readInodeOpt(local) catch return) orelse return;
            if (v.nlink != 0) return;
            self.gatherCorpse(c, local);
        }

        /// A forgotten corpse joins the batch; a full batch drains inline
        /// (one entry per `RECLAIM_BATCH`); a gather that cannot grow drives
        /// the corpse alone (never lost).
        fn gatherCorpse(self: *Self, c: *Ctx, ino: u64) void {
            var drain = false;
            {
                self.reclaim_lock.lock();
                defer self.reclaim_lock.unlock();
                self.reclaim.append(self.gpa, ino) catch {
                    fuse_reclaim_queue_overflow.inc();
                    self.destroyCorpses(c, &.{ino}) catch |e| log.err("kv_fs: inline corpse destroy of ino {d} failed: {t}", .{ ino, e });
                    return;
                };
                drain = self.reclaim.items.len >= RECLAIM_BATCH;
            }
            if (drain) self.reclaimDrain(c) catch |e| log.err("kv_fs: reclaim drain failed: {t}", .{e});
        }

        /// Destroy every gathered corpse, `RECLAIM_BATCH` per entry.
        pub fn reclaimDrain(self: *Self, c: *Ctx) Error!void {
            while (true) {
                var batch: [RECLAIM_BATCH]u64 = undefined;
                var n: usize = 0;
                {
                    self.reclaim_lock.lock();
                    defer self.reclaim_lock.unlock();
                    while (n < RECLAIM_BATCH and self.reclaim.items.len > 0) : (n += 1) batch[n] = self.reclaim.pop().?;
                }
                if (n == 0) return;
                try self.destroyCorpses(c, batch[0..n]);
            }
        }

        fn destroyCorpses(self: *Self, c: *Ctx, corpses: []const u64) Error!void {
            const t = try self.store.begin(@src());
            errdefer self.store.abandon(t);
            var destroyed: u64 = 0;
            for (corpses) |ino| {
                // Re-check: a corpse is nlink 0 (a forget of a live inode
                // never reached the gather).
                const v = (try self.readInodeOpt(ino)) orelse continue;
                if (v.nlink != 0) continue;
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                const start = key.xattrKey(ino, 0, 0);
                const end = key.xattrKey(ino, key.HASH56_MAX, std.math.maxInt(u8));
                self.store.range(self.gpa, key.TREE_XATTRS, &start, &end, 1024, &kvs) catch |e| return storeErr(e);
                var xkeys: [1024][]const u8 = undefined;
                for (kvs.items, 0..) |kv, i| xkeys[i] = kv.key;
                try inos.stageDestroy(t, ino, xkeys[0..kvs.items.len]);
                _ = self.pending.retire(ino);
                destroyed += 1;
            }
            if (destroyed == 0) {
                self.store.abandon(t);
                return;
            }
            try self.commit(c, t);
            fuse_reclaim_destroys.add(destroyed);
            fuse_reclaim_drains.inc();
        }

        pub fn getattr(self: *Self, c: *Ctx, a: filesystem.GetattrArgs) Error!filesystem.Attr {
            c.stage(.meta_backend_start);
            const v = try self.readInode(try localOf(a.ino));
            c.stage(.meta_backend_done);
            return self.attrReply(a.ino, v);
        }

        pub fn setattr(self: *Self, c: *Ctx, a: filesystem.SetattrArgs) Error!filesystem.Attr {
            const local = try localOf(a.ino);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
            plan.inode_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            const stored = try self.readInode(local);
            if (a.attr.size) |sz| {
                if (!(stored.mode & S.IFMT == S.IFREG)) return error.InvalidArgument;
                try self.stageTruncate(t, local, sz);
            }
            var req = a.attr.toEconomy();
            // chmod never changes the type: the S_IFMT bits are the record's.
            if (req.mode) |m| req.mode = (stored.mode & S.IFMT) | (m & ~@as(u32, S.IFMT));
            var out = try economy.stageSetattr(t, &self.pending, local, stored, req, now());
            if (out.verdict == .absorbed) {
                self.store.abandon(t);
                return self.attrReply(a.ino, out.value);
            }
            if (a.attr.kill_suidgid) {
                var v = out.value;
                try self.killpriv(t, local, &v);
                if (v.mode != out.value.mode) {
                    out.value = v;
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
                }
            }
            try self.commit(c, t);
            _ = self.pending.retire(local);
            return self.attrReply(a.ino, out.value);
        }

        pub fn readlink(self: *Self, c: *Ctx, a: filesystem.ReadlinkArgs) Error!filesystem.Data {
            const local = try localOf(a.ino);
            const v = try self.readInode(local);
            if (v.mode & S.IFMT != S.IFLNK) return error.InvalidArgument;
            const target = (try self.xattrValue(local, economy.SYMLINK_XATTR)) orelse return error.InputOutput;
            defer self.gpa.free(target);
            if (target.len > c.window.len) return error.NameTooLong;
            @memcpy(c.window[0..target.len], target);
            return .{ .len = target.len };
        }

        /// create/mkdir/mknod/symlink share the parent-side protocol: the
        /// parent guard as the child's type demands + the D-guard, the
        /// parent read and the EEXIST probe under them, ONE entry.
        const NewChild = struct { mode: u32, rdev: u32 = 0, target: ?[]const u8 = null };

        fn createChild(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, child: NewChild) Error!filesystem.Entry {
            try checkName(name);
            const is_dir = economy.isDirMode(child.mode);
            const parent = try localOf(parent_global);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = parent, .mode = if (is_dir) .exclusive else .shared };
            plan.inode_len = 1;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = name, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            const pv = try self.readDir(parent);
            var chain: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            if (try self.scanChain(parent, name, &chain, &kvs) != null) return error.PathAlreadyExists;
            const local = self.store.allocateIno();
            const global = globalOf(local);
            const stamp = now();
            const out = if (child.target) |target|
                economy.stageSymlink(t, .{ .hash_seed = self.store.hashSeed(), .now = stamp, .parent = parent, .parent_value = pv, .name = name, .uid = c.uid, .gid = c.gid, .local_ino = local, .global_ino = global, .target = target, .chain = chain.slice() }) catch |e| return switch (e) {
                    error.SymlinkTargetTooLong, error.NameTooLong => error.NameTooLong,
                    error.DentryChainOverflow => error.NoSpaceLeft,
                    error.OutOfMemory => error.OutOfMemory,
                }
            else
                economy.stageCreate(t, .{ .hash_seed = self.store.hashSeed(), .now = stamp, .parent = parent, .parent_value = pv, .name = name, .mode = child.mode, .uid = c.uid, .gid = c.gid, .rdev = child.rdev, .local_ino = local, .global_ino = global, .chain = chain.slice() }) catch |e| return switch (e) {
                    error.NameTooLong => error.NameTooLong,
                    error.DentryChainOverflow => error.NoSpaceLeft,
                    error.OutOfMemory => error.OutOfMemory,
                };
            try self.commit(c, t);
            if (is_dir) self.rememberParent(global, parent_global);
            return self.entryOf(global, out.child);
        }

        pub fn symlink(self: *Self, c: *Ctx, a: filesystem.SymlinkArgs) Error!filesystem.Entry {
            return self.createChild(c, a.parent, a.name, .{ .mode = S.IFLNK | 0o777, .target = a.link });
        }

        pub fn mknod(self: *Self, c: *Ctx, a: filesystem.MknodArgs) Error!filesystem.Entry {
            // The kernel already applied the umask (DONT_MASK is not advertised).
            return self.createChild(c, a.parent, a.name, .{ .mode = a.mode, .rdev = a.rdev });
        }

        pub fn mkdir(self: *Self, c: *Ctx, a: filesystem.MkdirArgs) Error!filesystem.Entry {
            return self.createChild(c, a.parent, a.name, .{ .mode = (a.mode & ~@as(u32, S.IFMT)) | S.IFDIR });
        }

        pub fn create(self: *Self, c: *Ctx, a: filesystem.CreateArgs) Error!filesystem.Created {
            const e = try self.createChild(c, a.parent, a.name, .{ .mode = (a.mode & ~@as(u32, S.IFMT)) | S.IFREG });
            return .{ .entry = e, .fh = self.mintFh(), .open_flags = self.regularOpenFlags() };
        }

        /// unlink/rmdir: find (no guards) → lock the full set → revalidate
        /// → stage → commit.
        fn removeChild(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, want_dir: bool) Error!void {
            try checkName(name);
            const parent = try localOf(parent_global);
            const probe = (try self.findDentry(parent, name)) orelse return error.FileNotFound;
            if (want_dir and !probe.isDir()) return error.NotDir;
            if (!want_dir and probe.isDir()) return error.IsDir;
            var plan: economy.GuardPlan = .{};
            const shared = economy.unlinkParentShared(want_dir, parent, probe.local);
            plan.inode_reqs[0] = .{ .ino = parent, .mode = if (shared) .shared else .exclusive };
            plan.inode_reqs[1] = .{ .ino = probe.local, .mode = .exclusive };
            plan.inode_len = 2;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = name, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            // Revalidate under the guards: the same name still names the same child.
            const found = (try self.findDentry(parent, name)) orelse return error.FileNotFound;
            if (found.global != probe.global) return error.FileNotFound;
            const cv = try self.readInode(found.local);
            if (want_dir and try self.dirHasEntries(found.local)) return error.DirectoryNotEmpty;
            const pv: ?InodeValue = if (shared) null else try self.readDir(parent);
            const req: economy.Unlink = .{ .now = now(), .parent = parent, .name = name, .pos = found.pos, .child = found.local, .child_value = cv, .is_dir = want_dir, .parent_value = pv };
            _ = if (want_dir) try economy.stageRmdir(t, req) else try economy.stageUnlink(t, req);
            try self.commit(c, t);
            if (want_dir) self.forgetParent(found.global);
        }

        pub fn unlink(self: *Self, c: *Ctx, a: filesystem.UnlinkArgs) Error!void {
            return self.removeChild(c, a.parent, a.name, false);
        }

        pub fn rmdir(self: *Self, c: *Ctx, a: filesystem.UnlinkArgs) Error!void {
            return self.removeChild(c, a.parent, a.name, true);
        }

        pub fn rename(self: *Self, c: *Ctx, a: filesystem.RenameArgs) Error!void {
            try checkName(a.name);
            try checkName(a.newname);
            const flags: economy.RenameFlags = .{ .noreplace = a.flags & RENAME_NOREPLACE != 0, .exchange = a.flags & RENAME_EXCHANGE != 0 };
            const whiteout = a.flags & RENAME_WHITEOUT != 0;
            if (a.flags & ~@as(u32, RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0) return error.InvalidArgument;
            if ((flags.noreplace and flags.exchange) or (whiteout and flags.exchange)) return error.InvalidArgument;
            const old_parent = try localOf(a.parent);
            const new_parent = try localOf(a.newparent);
            // Phase 1: discover both ends without guards.
            const old0 = (try self.findDentry(old_parent, a.name)) orelse return error.FileNotFound;
            const new0 = try self.findDentry(new_parent, a.newname);
            if (flags.exchange and new0 == null) return error.FileNotFound;
            // The conservative set: both parents exclusive (≥ what the
            // staged records require), both inodes, both names.
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = old_parent, .mode = .exclusive };
            plan.inode_reqs[1] = .{ .ino = new_parent, .mode = .exclusive };
            plan.inode_reqs[2] = .{ .ino = old0.local, .mode = .exclusive };
            plan.inode_len = 3;
            if (new0) |n| {
                plan.inode_reqs[3] = .{ .ino = n.local, .mode = .exclusive };
                plan.inode_len = 4;
            }
            plan.dentry_reqs[0] = .{ .parent = old_parent, .name = a.name, .mode = .exclusive };
            plan.dentry_reqs[1] = .{ .parent = new_parent, .name = a.newname, .mode = .exclusive };
            plan.dentry_len = 2;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            // Phase 2: revalidate and read under the guards.
            var old_chain: ChainBuf = .{};
            var old_kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &old_kvs);
            const old = (try self.scanChain(old_parent, a.name, &old_chain, &old_kvs)) orelse return error.FileNotFound;
            if (old.global != old0.global) return error.FileNotFound;
            var new_chain: ChainBuf = .{};
            var new_kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &new_kvs);
            const new = try self.scanChain(new_parent, a.newname, &new_chain, &new_kvs);
            if ((new == null) != (new0 == null) or (new != null and new.?.global != new0.?.global)) return error.FileNotFound;
            if (new) |n| {
                if (!flags.exchange) {
                    // POSIX: a directory replaces only a directory and vice versa.
                    if (old.isDir() and !n.isDir()) return error.NotDir;
                    if (!old.isDir() and n.isDir()) return error.IsDir;
                }
            }
            const old_pv = try self.readDir(old_parent);
            const new_pv: InodeValue = if (old_parent == new_parent) old_pv else try self.readDir(new_parent);
            const dest_value: ?InodeValue = if (new) |n| try self.readInode(n.local) else null;
            const dest_has_entries = if (new) |n| (n.isDir() and try self.dirHasEntries(n.local)) else false;
            var wo: ?economy.Whiteout = null;
            if (whiteout) {
                const l = self.store.allocateIno();
                wo = .{ .local = l, .global = globalOf(l) };
            }
            _ = economy.stageRename(t, .{
                .hash_seed = self.store.hashSeed(),
                .now = now(),
                .old_parent = old_parent,
                .old_name = a.name,
                .new_parent = new_parent,
                .new_name = a.newname,
                .flags = flags,
                .old_pos = old.pos,
                .new_pos = if (new) |n| n.pos else null,
                .new_chain = new_chain.slice(),
                .old_chain = old_chain.slice(),
                .src_local = old.local,
                .dest_local = if (new) |n| n.local else null,
                .old_parent_value = old_pv,
                .new_parent_value = new_pv,
                .dest_value = dest_value,
                .dest_has_entries = dest_has_entries,
                .whiteout = wo,
            }) catch |e| return switch (e) {
                error.InvalidFlags => error.InvalidArgument,
                error.SourceNotFound => error.FileNotFound,
                error.TargetExists => error.PathAlreadyExists,
                error.TargetNotEmpty => error.DirectoryNotEmpty,
                error.NameTooLong => error.NameTooLong,
                error.DentryChainOverflow => error.NoSpaceLeft,
                error.OutOfMemory => error.OutOfMemory,
            };
            try self.commit(c, t);
            if (old.isDir()) self.rememberParent(old.global, a.newparent);
            if (new) |n| {
                if (flags.exchange) {
                    if (n.isDir()) self.rememberParent(n.global, a.parent);
                } else if (n.isDir()) self.forgetParent(n.global);
            }
        }

        pub fn link(self: *Self, c: *Ctx, a: filesystem.LinkArgs) Error!filesystem.Entry {
            try checkName(a.newname);
            const parent = try localOf(a.newparent);
            const child = try localOf(a.ino);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = parent, .mode = .exclusive };
            plan.inode_reqs[1] = .{ .ino = child, .mode = .exclusive };
            plan.inode_len = 2;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = a.newname, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            const pv = try self.readDir(parent);
            const cv = try self.readInode(child);
            if (economy.isDirMode(cv.mode)) return error.AccessDenied;
            var chain: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            if (try self.scanChain(parent, a.newname, &chain, &kvs) != null) return error.PathAlreadyExists;
            const out = economy.stageLink(t, .{ .hash_seed = self.store.hashSeed(), .now = now(), .parent = parent, .parent_value = pv, .name = a.newname, .child = child, .global_child = a.ino, .child_value = cv, .chain = chain.slice() }) catch |e| return switch (e) {
                error.TooManyLinks => error.TooManyLinks,
                error.NameTooLong => error.NameTooLong,
                error.DentryChainOverflow => error.NoSpaceLeft,
                error.OutOfMemory => error.OutOfMemory,
            };
            try self.commit(c, t);
            _ = self.pending.retire(child);
            return self.entryOf(a.ino, out.child);
        }

        pub fn open(self: *Self, c: *Ctx, a: filesystem.OpenArgs) Error!filesystem.Opened {
            const local = try localOf(a.ino);
            const v = try self.readInode(local);
            if (economy.isDirMode(v.mode)) return error.IsDir;
            // ATOMIC_O_TRUNC: the truncate rides the open (no separate SETATTR).
            if (a.flags & O_TRUNC != 0 and v.mode & S.IFMT == S.IFREG) {
                var plan: economy.GuardPlan = .{};
                plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
                plan.inode_len = 1;
                const held = try self.lockPlan(&plan);
                const t = try self.beginWith(held, @src());
                errdefer self.store.abandon(t);
                var stored = try self.readInode(local);
                try self.stageTruncate(t, local, 0);
                stored.size = 0;
                const stamp = now();
                stored.mtime = stamp;
                stored.ctime = stamp;
                if (a.open_flags & abi.FUSE_OPEN_KILL_SUIDGID != 0) try self.killpriv(t, local, &stored);
                try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &stored.encode());
                try self.commit(c, t);
                _ = self.pending.retire(local);
            }
            return .{ .fh = self.mintFh(), .open_flags = self.regularOpenFlags() };
        }

        pub fn read(self: *Self, c: *Ctx, a: filesystem.ReadArgs) Error!filesystem.Data {
            const local = try localOf(a.ino);
            c.stage(.meta_backend_start);
            const v = try self.readInode(local);
            if (a.offset >= v.size) return .{ .len = 0 };
            const want: usize = @intCast(@min(@as(u64, a.size), v.size - a.offset));
            const n = @min(want, c.window.len);
            @memset(c.window[0..n], 0);
            if (try self.readLayout(local)) |bytes| {
                defer self.gpa.free(bytes);
                const lay = InlineLayout.decode(bytes) catch return error.InputOutput;
                if (a.offset < lay.data.len) {
                    const avail = lay.data[@intCast(a.offset)..];
                    const take = @min(n, avail.len);
                    @memcpy(c.window[0..take], avail[0..take]);
                }
            }
            c.stage(.meta_backend_done);
            return .{ .len = n };
        }

        pub fn write(self: *Self, c: *Ctx, a: filesystem.WriteArgs) Error!filesystem.Written {
            const local = try localOf(a.ino);
            const end = a.offset + a.data.len;
            if (end > self.inline_max) {
                fuse_write_beyond_inline_refused.inc();
                return error.FileTooBig;
            }
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
            plan.inode_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            var v = try self.readInode(local);
            if (v.mode & S.IFMT != S.IFREG) return error.InvalidArgument;
            // The new image: the old bytes zero-extended to max(old, end), the write overlaid.
            const old = try self.readLayout(local);
            defer if (old) |o| self.gpa.free(o);
            const old_data: []const u8 = if (old) |o| (InlineLayout.decode(o) catch return error.InputOutput).data else &.{};
            const new_len = @max(old_data.len, end);
            const buf = try self.gpa.alloc(u8, INLINE_LAYOUT_HEADER_LEN + new_len);
            defer self.gpa.free(buf);
            const data = buf[INLINE_LAYOUT_HEADER_LEN..];
            @memset(data, 0);
            @memcpy(data[0..old_data.len], old_data);
            @memcpy(data[@intCast(a.offset)..@intCast(end)], a.data);
            const k = try self.xattrKeyFor(local, LAYOUT_XATTR);
            if (k.existing) |e| self.gpa.free(e);
            const enc = InlineLayout.writeHeader(buf, data.len);
            try self.stageXattrPut(t, &k.k, LAYOUT_XATTR, enc);
            const stamp = now();
            v.size = @max(v.size, end);
            v.mtime = stamp;
            if (economy.laterThan(stamp, v.ctime)) v.ctime = stamp;
            if (a.write_flags & abi.FUSE_WRITE_KILL_SUIDGID != 0) try self.killpriv(t, local, &v);
            try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
            try self.commit(c, t);
            _ = self.pending.retire(local);
            return .{ .size = @intCast(a.data.len) };
        }

        /// Nothing is deferred: every write is a landed journal entry, so a
        /// FLUSH has no latched error to carry.
        pub fn flush(_: *Self, _: *Ctx, _: filesystem.FlushArgs) Error!void {}

        pub fn release(_: *Self, _: *Ctx, _: filesystem.ReleaseArgs) Error!void {}

        /// The trailing coalesced barrier (every acked entry durable).
        pub fn fsync(self: *Self, c: *Ctx, _: filesystem.FsyncArgs) Error!void {
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            self.store.fsync() catch |e| return storeErr(e);
        }

        pub fn fsyncdir(self: *Self, c: *Ctx, a: filesystem.FsyncArgs) Error!void {
            return self.fsync(c, a);
        }

        pub fn opendir(self: *Self, _: *Ctx, a: filesystem.OpenArgs) Error!filesystem.Opened {
            _ = try self.readDir(try localOf(a.ino));
            return .{ .fh = self.mintFh(), .open_flags = 0 };
        }

        pub fn releasedir(_: *Self, _: *Ctx, _: filesystem.ReleaseArgs) Error!void {}

        pub fn readdir(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs) Error!filesystem.Data {
            return self.listDir(c, a, false);
        }

        pub fn readdirplus(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs) Error!filesystem.Data {
            return self.listDir(c, a, true);
        }

        /// Entries per range call: a page of 24+name-byte dirents is ≈ 100
        /// entries; one scan fills a 4 KiB window, a 64 KiB `getdents`
        /// buffer takes a handful.
        const READDIR_SCAN = 128;

        fn listDir(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs, plus: bool) Error!filesystem.Data {
            const dir = try localOf(a.ino);
            const dv = try self.readDir(dir);
            const pos = key.decodeReaddirCookie(a.offset) catch return error.InvalidArgument;
            var plain = codec.DirentWriter.init(c.window, a.size);
            var rich = codec.DirentPlusWriter.init(c.window, a.size);
            // `.` and `..`: nodeid 0 in the plus form (the kernel skips their
            // dentry linkage); the `..` ino is the memo's, else the dir's own.
            if (pos == .start) {
                if (!(try self.emit(&plain, &rich, plus, a.ino, key.READDIR_OFFSET_DOT, dv, ".", true))) return .{ .len = self.written(&plain, &rich, plus) };
            }
            if (pos == .start or pos == .after_dot) {
                const p = self.parentOf(a.ino) orelse a.ino;
                const pv = (try self.readInodeOpt(try localOf(p))) orelse dv;
                if (!(try self.emit(&plain, &rich, plus, p, key.READDIR_OFFSET_DOTDOT, pv, "..", true))) return .{ .len = self.written(&plain, &rich, plus) };
            }
            var cursor: ?[key.DENTRY_KEY_LEN]u8 = switch (pos) {
                .after_entry => |e| nextDentryKey(dir, e.hash54, e.coll_seq),
                else => key.dentryKey(dir, 0, 0),
            };
            const end = key.dentryKey(dir, key.HASH54_MAX, std.math.maxInt(u8));
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            scan: while (cursor) |start| {
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                self.store.range(self.gpa, key.TREE_DENTRIES, &start, &end, READDIR_SCAN, &kvs) catch |e| return storeErr(e);
                if (kvs.items.len == 0) break;
                var last_hash: u64 = 0;
                var last_coll: u8 = 0;
                for (kvs.items) |kv| {
                    const dk = key.DentryKey.decode(kv.key) catch return error.InputOutput;
                    last_hash = dk.hash54();
                    last_coll = dk.collSeq();
                    const dent = record.DentryValue.decode(kv.value) catch return error.InputOutput;
                    const cookie = key.encodeReaddirCookie(last_hash, last_coll);
                    const child_local = localOf(dent.child_ino) catch {
                        dentry_without_inode.fire("readdir found a dentry naming a foreign-slot ino");
                        continue;
                    };
                    const cv = (try self.readInodeOpt(child_local)) orelse {
                        dentry_without_inode.fire("readdir found a dentry whose inode record is absent");
                        continue;
                    };
                    if (plus and economy.isDirMode(cv.mode)) self.rememberParent(dent.child_ino, a.ino);
                    if (!(try self.emit(&plain, &rich, plus, dent.child_ino, cookie, cv, dent.name, false))) break :scan;
                }
                if (kvs.items.len < READDIR_SCAN) break;
                cursor = nextDentryKey(dir, last_hash, last_coll);
            }
            return .{ .len = self.written(&plain, &rich, plus) };
        }

        /// The smallest dentry key of `dir` strictly after `(hash54, coll)`;
        /// null past the last one (the key stays 17 bytes — a `keySuccessor`
        /// would lengthen it past the kind's shape).
        fn nextDentryKey(dir: u64, hash54: u64, coll: u8) ?[key.DENTRY_KEY_LEN]u8 {
            const suffix = key.dentryKeySuffix(hash54, coll);
            if (suffix == key.dentryKeySuffix(key.HASH54_MAX, std.math.maxInt(u8))) return null;
            const next = suffix + 1;
            return key.dentryKey(dir, next >> 8, @truncate(next));
        }

        /// One dirent into whichever writer is live; false when it did not fit.
        fn emit(self: *Self, plain: *codec.DirentWriter, rich: *codec.DirentPlusWriter, plus: bool, global: u64, cookie: u64, v: InodeValue, name: []const u8, dot: bool) Error!bool {
            if (plus) {
                var e = self.entryOf(global, v);
                if (dot) e.nodeid = 0;
                rich.add(e, cookie, name) catch |err| return switch (err) {
                    error.Full => false,
                    error.EmptyName, error.NameTooLong => error.InputOutput,
                };
            } else {
                plain.add(global, cookie, v.mode, name) catch |err| return switch (err) {
                    error.Full => false,
                    error.EmptyName, error.NameTooLong => error.InputOutput,
                };
            }
            return true;
        }

        fn written(_: *Self, plain: *const codec.DirentWriter, rich: *const codec.DirentPlusWriter, plus: bool) usize {
            return if (plus) rich.len else plain.len;
        }

        pub fn statfs(self: *Self, _: *Ctx, _: filesystem.StatfsArgs) Error!filesystem.Statfs {
            const sp = self.store.space();
            const used_inos = self.store.nextIno() -| inos.LOCAL_INO_BASE;
            return .{
                .blocks = sp.total_bytes / STATFS_BSIZE,
                .bfree = sp.free_bytes / STATFS_BSIZE,
                .bavail = sp.free_bytes / STATFS_BSIZE,
                .files = INO_SPACE,
                .ffree = INO_SPACE -| used_inos,
                .bsize = STATFS_BSIZE,
                .namelen = @intCast(NAME_MAX),
                .frsize = STATFS_BSIZE,
            };
        }

        pub fn setxattr(self: *Self, c: *Ctx, a: filesystem.SetxattrArgs) Error!void {
            try checkName(a.name);
            if (isReservedXattr(a.name)) return error.NotSupported;
            if (a.value.len > record.xattrValueCap(self.store.nodeSize())) return error.ValueTooLarge;
            const local = try localOf(a.ino);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
            plan.inode_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            var v = try self.readInode(local);
            const k = try self.xattrKeyFor(local, a.name);
            if (k.existing) |e| self.gpa.free(e);
            if (a.flags & XATTR_CREATE != 0 and k.existing != null) return error.PathAlreadyExists;
            if (a.flags & XATTR_REPLACE != 0 and k.existing == null) return error.NoAttribute;
            try self.stageXattrPut(t, &k.k, a.name, a.value);
            v.ctime = now();
            try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
            try self.commit(c, t);
            _ = self.pending.retire(local);
        }

        pub fn getxattr(self: *Self, c: *Ctx, a: filesystem.GetxattrArgs) Error!filesystem.Xattr {
            try checkName(a.name);
            if (isReservedXattr(a.name)) return error.NoAttribute;
            const value = (try self.xattrValue(try localOf(a.ino), a.name)) orelse return error.NoAttribute;
            defer self.gpa.free(value);
            if (a.size == 0) return .{ .size = @intCast(value.len) };
            if (value.len > a.size or value.len > c.window.len) return error.BufferTooSmall;
            @memcpy(c.window[0..value.len], value);
            return .{ .data = value.len };
        }

        pub fn listxattr(self: *Self, c: *Ctx, a: filesystem.ListxattrArgs) Error!filesystem.Xattr {
            const local = try localOf(a.ino);
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const start = key.xattrKey(local, 0, 0);
            const end = key.xattrKey(local, key.HASH56_MAX, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_XATTRS, &start, &end, 4096, &kvs) catch |e| return storeErr(e);
            var total: usize = 0;
            for (kvs.items) |kv| {
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (isReservedXattr(xv.name)) continue;
                total += xv.name.len + 1;
            }
            if (a.size == 0) return .{ .size = @intCast(total) };
            if (total > a.size or total > c.window.len) return error.BufferTooSmall;
            var off: usize = 0;
            for (kvs.items) |kv| {
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (isReservedXattr(xv.name)) continue;
                @memcpy(c.window[off..][0..xv.name.len], xv.name);
                c.window[off + xv.name.len] = 0;
                off += xv.name.len + 1;
            }
            return .{ .data = off };
        }

        pub fn removexattr(self: *Self, c: *Ctx, a: filesystem.RemovexattrArgs) Error!void {
            try checkName(a.name);
            if (isReservedXattr(a.name)) return error.NoAttribute;
            const local = try localOf(a.ino);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
            plan.inode_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            errdefer self.store.abandon(t);
            var v = try self.readInode(local);
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const hit = (try self.findXattr(local, a.name, &occ, &n)) orelse return error.NoAttribute;
            self.gpa.free(hit.value);
            try t.stageDelete(key.TREE_XATTRS, &hit.k);
            v.ctime = now();
            try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
            try self.commit(c, t);
            _ = self.pending.retire(local);
        }
    };
}

// ---------------------------------------------------------------------------
// Tests. The in-RAM store runs the POSIX mini-suite through the lifecycle
// on every host; the Linux KV backend runs it through the device path.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// An in-RAM store: a sorted `(kind, key) → value` table that applies a
/// transaction's records on commit with the fold algebra's semantics (Put
/// upserts, Delete removes, a Delta overwrites its masked fields on an
/// existing base and folds to nothing on an absent one).
pub const MemStore = struct {
    pub const StoreError = error{ OutOfMemory, Io };

    gpa: Allocator,
    lock: jc.SpinLock = .{},
    entries: std.ArrayList(Row) = .empty,
    /// Starts past the base so no test ino is identity-mapped between its
    /// local and global forms (`(local − 2) · W + 2`): a global/local mix-up
    /// in a handler is a loud ENOENT here, not a coincidence.
    next_ino: u64 = inos.LOCAL_INO_BASE + 3,
    seed: u64 = 0x5EED_0F_7E57,
    node_size_: usize = 65536,
    commits: u64 = 0,
    fsyncs: u64 = 0,

    const Row = struct { kind: u8, k: []u8, v: []u8 };

    pub fn init(gpa: Allocator) Allocator.Error!MemStore {
        var self: MemStore = .{ .gpa = gpa };
        // The root directory, as `format` writes it.
        const root: InodeValue = .{ .mode = S.IFDIR | 0o755, .nlink = 2, .atime = 1, .mtime = 1, .ctime = 1 };
        try self.upsert(key.TREE_INODES, &key.inodeKey(inos.ROOT_INO), &root.encode());
        return self;
    }

    pub fn deinit(self: *MemStore) void {
        for (self.entries.items) |r| {
            self.gpa.free(r.k);
            self.gpa.free(r.v);
        }
        self.entries.deinit(self.gpa);
    }

    pub fn coreError(e: StoreError) errors.Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.InputOutput,
        };
    }

    fn order(kind: u8, k: []const u8, r: Row) std.math.Order {
        if (kind != r.kind) return std.math.order(kind, r.kind);
        return key.order(k, r.k);
    }

    /// Index of `(kind, k)` or where it would go.
    fn find(self: *MemStore, kind: u8, k: []const u8) struct { idx: usize, hit: bool } {
        var lo: usize = 0;
        var hi: usize = self.entries.items.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            switch (order(kind, k, self.entries.items[mid])) {
                .lt => hi = mid,
                .gt => lo = mid + 1,
                .eq => return .{ .idx = mid, .hit = true },
            }
        }
        return .{ .idx = lo, .hit = false };
    }

    fn upsert(self: *MemStore, kind: u8, k: []const u8, v: []const u8) Allocator.Error!void {
        const f = self.find(kind, k);
        const vd = try self.gpa.dupe(u8, v);
        if (f.hit) {
            self.gpa.free(self.entries.items[f.idx].v);
            self.entries.items[f.idx].v = vd;
            return;
        }
        errdefer self.gpa.free(vd);
        const kd = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(kd);
        try self.entries.insert(self.gpa, f.idx, .{ .kind = kind, .k = kd, .v = vd });
    }

    fn remove(self: *MemStore, kind: u8, k: []const u8) void {
        const f = self.find(kind, k);
        if (!f.hit) return;
        const r = self.entries.orderedRemove(f.idx);
        self.gpa.free(r.k);
        self.gpa.free(r.v);
    }

    pub fn begin(self: *MemStore, site: tx.Site) Allocator.Error!*tx.KvTx {
        const t = try self.gpa.create(tx.KvTx);
        t.* = tx.KvTx.init(self.gpa, site);
        return t;
    }

    pub fn abandon(self: *MemStore, t: *tx.KvTx) void {
        t.deinit();
        self.gpa.destroy(t);
    }

    /// Apply, then drop the guards with the tx (the terminal outcome).
    pub fn commit(self: *MemStore, t: *tx.KvTx) StoreError!void {
        defer self.abandon(t);
        self.lock.lock();
        defer self.lock.unlock();
        for (t.staged.items) |s| {
            switch (s.kind) {
                .put => try self.upsert(s.tree_id, s.key, s.value),
                .delete => self.remove(s.tree_id, s.key),
                .delta => {
                    const f = self.find(s.tree_id, s.key);
                    if (!f.hit) continue;
                    var base = InodeValue.decode(self.entries.items[f.idx].v) catch return error.Io;
                    const d = record.InodeDelta.decode(s.value) catch return error.Io;
                    d.apply(&base);
                    const enc = base.encode();
                    try self.upsert(s.tree_id, s.key, &enc);
                },
            }
        }
        self.commits += 1;
    }

    pub fn lookup(self: *MemStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        self.lock.lock();
        defer self.lock.unlock();
        const f = self.find(kind, k);
        if (!f.hit) return null;
        return try gpa.dupe(u8, self.entries.items[f.idx].v);
    }

    pub fn range(self: *MemStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        self.lock.lock();
        defer self.lock.unlock();
        var i = self.find(kind, start).idx;
        var n: usize = 0;
        while (i < self.entries.items.len and n < max) : (i += 1) {
            const r = self.entries.items[i];
            if (r.kind != kind or key.lessThan(end, r.k)) break;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, r.k), .value = try gpa.dupe(u8, r.v) });
            n += 1;
        }
    }

    pub fn fsync(self: *MemStore) StoreError!void {
        self.fsyncs += 1;
    }

    pub fn allocateIno(self: *MemStore) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const i = self.next_ino;
        self.next_ino += 1;
        return i;
    }

    pub fn nextIno(self: *const MemStore) u64 {
        return self.next_ino;
    }

    pub fn hashSeed(self: *const MemStore) u64 {
        return self.seed;
    }

    pub fn nodeSize(self: *const MemStore) usize {
        return self.node_size_;
    }

    pub const Space = struct { total_bytes: u64, free_bytes: u64 };

    pub fn space(_: *const MemStore) Space {
        return .{ .total_bytes = 64 << 20, .free_bytes = 48 << 20 };
    }

    pub fn isFailed(_: *const MemStore) bool {
        return false;
    }

    /// Records of one kind (a test's census).
    pub fn countKind(self: *MemStore, kind: u8) usize {
        var n: usize = 0;
        for (self.entries.items) |r| {
            if (r.kind == kind) n += 1;
        }
        return n;
    }
};

pub const MemFs = KvFs(MemStore);

test "inline layout record: encode∘decode = id; every other length or stamp refuses" {
    var buf: [64]u8 = undefined;
    const enc = (InlineLayout{ .data = "hello" }).encodeInto(&buf);
    try testing.expectEqual(@as(usize, 17), enc.len);
    try testing.expectEqualStrings("hello", (try InlineLayout.decode(enc)).data);
    try testing.expectError(error.Corrupt, InlineLayout.decode(enc[0 .. enc.len - 1]));
    try testing.expectError(error.Corrupt, InlineLayout.decode(enc[0..11]));
    var bad = buf;
    bad[0] = 'X';
    try testing.expectError(error.Corrupt, InlineLayout.decode(bad[0..17]));
    var bad2 = buf;
    bad2[7] = 1;
    try testing.expectError(error.Corrupt, InlineLayout.decode(bad2[0..17]));
    const empty = (InlineLayout{ .data = "" }).encodeInto(&buf);
    try testing.expectEqual(@as(usize, 0), (try InlineLayout.decode(empty)).data.len);
    try testing.expectEqual(@as(u64, 4096), inlineMaxBytes(4096));
    try testing.expectEqual(@as(u64, 16384), inlineMaxBytes(16384));
}

test "KILLPRIV_V2 mode law: suid always; sgid iff group-exec" {
    try testing.expectEqual(@as(u32, 0o755), killSuidgidMode(0o4755));
    try testing.expectEqual(@as(u32, 0o755), killSuidgidMode(0o6755));
    try testing.expectEqual(@as(u32, 0o2745), killSuidgidMode(0o6745));
    try testing.expectEqual(@as(u32, 0o644), killSuidgidMode(0o644));
}

test "the must-implement set is bound; the rung-4 ops default to ENOSYS" {
    try testing.expect(filesystem.Filesystem.implementsMustSet(MemFs));
    try testing.expect(!@hasDecl(MemFs, "fallocate"));
    try testing.expect(!@hasDecl(MemFs, "access"));
}

// ---------------------------------------------------------------------------
// The POSIX mini-suite through `lifecycle.dispatch` over the loopback.
// ---------------------------------------------------------------------------

const lifecycle = @import("lifecycle.zig");
const seam = @import("transport_seam.zig");
const op_guard = @import("op_guard.zig");
const watchdog = @import("watchdog.zig");
const open_table = @import("open_table.zig");
const Loopback = seam.Loopback;
const LoopDispatcher = lifecycle.Dispatcher(Loopback);

/// One mounted MemFs behind a dispatcher; every call is a real `dispatch`
/// on slot 0 with a fresh unique, and the captured reply comes back.
const Harness = struct {
    store: MemStore,
    fs: MemFs,
    lb: Loopback,
    reg: watchdog.Registry,
    guards: op_guard.Table,
    opens: open_table.OpenTable,
    d: LoopDispatcher,
    unique: u64 = 2,

    const slot = Loopback.slot(0);
    const window_bytes = 8192;

    fn init(self: *Harness) !void {
        self.store = try MemStore.init(testing.allocator);
        self.fs = try MemFs.mount(testing.allocator, testing.io, &self.store, .{ .dlm_stripes = 64 });
        self.lb = try Loopback.init(testing.allocator, 1, window_bytes);
        self.reg = try watchdog.Registry.init(testing.allocator, 1);
        self.guards = try op_guard.Table.init(testing.allocator, 64);
        self.opens = try open_table.OpenTable.init(testing.allocator, 64);
        self.unique = 2;
        self.d = LoopDispatcher.init(&self.lb, .{ .fs = filesystem.Filesystem.of(MemFs, &self.fs), .io = testing.io, .registry = &self.reg, .guards = &self.guards, .opens = &self.opens });
    }

    fn deinit(self: *Harness) void {
        self.opens.deinit();
        self.guards.deinit();
        self.reg.deinit();
        self.lb.deinit();
        self.fs.unmount();
        self.store.deinit();
    }

    fn call(self: *Harness, req: codec.Request, nodeid: u64) Loopback.Captured {
        const u = self.unique;
        self.unique += abi.FUSE_REQ_ID_STEP;
        const before = self.lb.replyCount(slot);
        self.d.dispatch(req, .{ .unique = u, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 7 }, slot, null);
        std.debug.assert(self.lb.replyCount(slot) == before + 1);
        const c = self.lb.captured(slot).?;
        std.debug.assert(c.header.unique == u);
        return c;
    }

    /// A no-reply op (FORGET/BATCH_FORGET/INTERRUPT).
    fn fire(self: *Harness, req: codec.Request, nodeid: u64) void {
        const before = self.lb.replyCount(slot);
        self.d.dispatch(req, .{ .unique = self.unique, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 7 }, slot, null);
        self.unique += abi.FUSE_REQ_ID_STEP;
        std.debug.assert(self.lb.replyCount(slot) == before);
    }

    fn errnoOf(c: Loopback.Captured) ?linux.E {
        return c.errno();
    }

    fn entry(self: *Harness, c: Loopback.Captured) !abi.fuse_entry_out {
        _ = self;
        if (c.errno()) |e| {
            std.debug.print("expected an entry, got errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        return abi.decodeExact(abi.fuse_entry_out, c.outBytes()) orelse error.TestBadReply;
    }

    fn attr(self: *Harness, c: Loopback.Captured) !abi.fuse_attr {
        _ = self;
        if (c.errno()) |e| {
            std.debug.print("expected attrs, got errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        return (abi.decodeExact(abi.fuse_attr_out, c.outBytes()) orelse return error.TestBadReply).attr;
    }

    fn lookup(self: *Harness, parent: u64, name: []const u8) Loopback.Captured {
        return self.call(.{ .lookup = .{ .name = name } }, parent);
    }

    fn mkdir(self: *Harness, parent: u64, name: []const u8) !u64 {
        return (try self.entry(self.call(.{ .mkdir = .{ .name = name, .mode = 0o755, .umask = 0 } }, parent))).nodeid;
    }

    /// CREATE: the entry's ino (the handle is open until `release`).
    fn create(self: *Harness, parent: u64, name: []const u8, mode: u32) !u64 {
        const c = self.call(.{ .create = .{ .name = name, .flags = 0, .mode = mode, .umask = 0, .open_flags = 0 } }, parent);
        if (c.errno()) |e| {
            std.debug.print("create: errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        const e = abi.decodePrefix(abi.fuse_entry_out, c.outBytes()) orelse return error.TestBadReply;
        return e.nodeid;
    }

    fn write(self: *Harness, ino: u64, offset: u64, data: []const u8, write_flags: u32) Loopback.Captured {
        const in: abi.fuse_write_in = .{ .fh = 1, .offset = offset, .size = @intCast(data.len), .write_flags = write_flags, .lock_owner = 0, .flags = 0, .padding = 0 };
        return self.call(.{ .write = .{ .in = in, .data = data } }, ino);
    }

    fn read(self: *Harness, ino: u64, offset: u64, size: u32) Loopback.Captured {
        const in: abi.fuse_read_in = .{ .fh = 1, .offset = offset, .size = size, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        return self.call(.{ .read = in }, ino);
    }

    fn getattr(self: *Harness, ino: u64) !abi.fuse_attr {
        return self.attr(self.call(.{ .getattr = .{ .flags = 0, .fh = null } }, ino));
    }

    fn setattr(self: *Harness, ino: u64, in: abi.fuse_setattr_in) Loopback.Captured {
        return self.call(.{ .setattr = in }, ino);
    }

    fn release(self: *Harness, ino: u64) void {
        _ = self.call(.{ .release = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, ino);
    }

    fn forget(self: *Harness, ino: u64, n: u64) void {
        self.fire(.{ .forget = .{ .nlookup = n } }, ino);
    }

    /// Every name a READDIR of `dir` yields from `offset`, joined by ','.
    fn readdirNames(self: *Harness, dir: u64, offset: u64, buf: []u8) ![]const u8 {
        const in: abi.fuse_read_in = .{ .fh = 1, .offset = offset, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        const c = self.call(.{ .readdir = in }, dir);
        if (c.errno()) |e| {
            std.debug.print("readdir: errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        var w: std.Io.Writer = .fixed(buf);
        var off: usize = 0;
        var first = true;
        while (off + abi.fuse_dirent.NAME_OFFSET <= c.body.len) {
            const d = abi.decodePrefix(abi.fuse_dirent, c.body[off..]).?;
            const name = c.body[off + abi.fuse_dirent.NAME_OFFSET ..][0..d.namelen];
            if (!first) try w.writeByte(',');
            first = false;
            try w.writeAll(name);
            off += abi.fuse_dirent.recordLen(d.namelen);
        }
        return w.buffered();
    }

    /// The cookie READDIR stamped on entry `name` (the continuation offset).
    fn readdirCookie(self: *Harness, dir: u64, name: []const u8) !u64 {
        const in: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        const c = self.call(.{ .readdir = in }, dir);
        var off: usize = 0;
        while (off + abi.fuse_dirent.NAME_OFFSET <= c.body.len) {
            const d = abi.decodePrefix(abi.fuse_dirent, c.body[off..]).?;
            const n = c.body[off + abi.fuse_dirent.NAME_OFFSET ..][0..d.namelen];
            if (std.mem.eql(u8, n, name)) return d.off;
            off += abi.fuse_dirent.recordLen(d.namelen);
        }
        return error.TestNameNotListed;
    }
};

test "POSIX: create → write → read → stat; an overlay write grows the file; a write past the inline bound is EFBIG and counted; truncate clips, deletes at 0 and zero-extends" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    try testing.expect(f >= 2);
    try testing.expectEqual(@as(u32, 1), h.opens.get(f).?.count);
    const a0 = try h.getattr(f);
    try testing.expectEqual(@as(u64, 0), a0.size);
    try testing.expectEqual(@as(u32, S.IFREG | 0o644), a0.mode);
    try testing.expectEqual(@as(u32, 1), a0.nlink);
    try testing.expectEqual(@as(u32, 1000), a0.uid);
    var payload: [100]u8 = undefined;
    for (&payload, 0..) |*b, i| b.* = @intCast(i);
    const w = h.write(f, 0, &payload, 0);
    try testing.expectEqual(@as(u32, 100), (abi.decodeExact(abi.fuse_write_out, w.outBytes()) orelse return error.TestBadReply).size);
    const r = h.read(f, 0, 4096);
    try testing.expectEqualSlices(u8, &payload, r.body);
    // Overlay at 50: the file grows to 150; bytes 0..50 keep, 50..150 are the new write.
    var second: [100]u8 = @splat(0xEE);
    _ = h.write(f, 50, &second, 0);
    const a1 = try h.getattr(f);
    try testing.expectEqual(@as(u64, 150), a1.size);
    try testing.expect(a1.mtime * std.time.ns_per_s + a1.mtimensec >= a0.mtime * std.time.ns_per_s + a0.mtimensec);
    const r2 = h.read(f, 0, 4096);
    try testing.expectEqual(@as(usize, 150), r2.body.len);
    try testing.expectEqualSlices(u8, payload[0..50], r2.body[0..50]);
    try testing.expectEqualSlices(u8, &second, r2.body[50..150]);
    // A partial read inside the file and one past EOF.
    try testing.expectEqualSlices(u8, payload[10..20], h.read(f, 10, 10).body);
    try testing.expectEqual(@as(usize, 0), h.read(f, 150, 10).body.len);
    // Past the inline bound: EFBIG, counted, nothing changed.
    const refused0 = fuse_write_beyond_inline_refused.load();
    const far = h.write(f, 4000, &payload, 0);
    try testing.expectEqual(linux.E.FBIG, far.errno().?);
    try testing.expectEqual(refused0 + 1, fuse_write_beyond_inline_refused.load());
    try testing.expectEqual(@as(u64, 150), (try h.getattr(f)).size);
    // Truncate to 30: the data clips.
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE;
    sa.size = 30;
    const t1 = try h.attr(h.setattr(f, sa));
    try testing.expectEqual(@as(u64, 30), t1.size);
    try testing.expectEqualSlices(u8, payload[0..30], h.read(f, 0, 4096).body);
    // Extend to 50 by truncate: zeros past the data.
    sa.size = 50;
    _ = h.setattr(f, sa);
    const ext = h.read(f, 0, 4096);
    try testing.expectEqual(@as(usize, 50), ext.body.len);
    try testing.expectEqualSlices(u8, payload[0..30], ext.body[0..30]);
    try testing.expect(std.mem.allEqual(u8, ext.body[30..50], 0));
    // Truncate to 0: the layout record is gone.
    sa.size = 0;
    _ = h.setattr(f, sa);
    try testing.expectEqual(@as(usize, 0), h.read(f, 0, 4096).body.len);
    try testing.expectEqual(@as(?[]u8, null), try h.fs.readLayout(try MemFs.localOf(f)));
    // Truncate past the inline bound refuses.
    sa.size = 8192;
    try testing.expectEqual(linux.E.FBIG, h.setattr(f, sa).errno().?);
    h.release(f);
    try testing.expectEqual(@as(u32, 0), h.opens.get(f).?.count);
}

test "POSIX: mkdir/readdir/rmdir — dot entries, cookie continuation, ENOTEMPTY, a removed directory looks up negative; lookup of . and .." {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const d = try h.mkdir(1, "d");
    const root_after = try h.getattr(1);
    try testing.expectEqual(@as(u32, 3), root_after.nlink);
    try testing.expectEqual(@as(u32, 2), (try h.getattr(d)).nlink);
    const a = try h.create(d, "a", 0o600);
    const b = try h.create(d, "b", 0o600);
    h.release(a);
    h.release(b);
    var buf: [256]u8 = undefined;
    const names = try h.readdirNames(d, 0, &buf);
    try testing.expect(std.mem.startsWith(u8, names, ".,..,"));
    try testing.expect(std.mem.indexOf(u8, names, "a") != null and std.mem.indexOf(u8, names, "b") != null);
    try testing.expectEqual(@as(usize, 4), std.mem.count(u8, names, ",") + 1);
    // Continuation from the cookie of the first real entry yields only the other.
    const first_name = names[5..6];
    const cookie = try h.readdirCookie(d, first_name);
    try testing.expect(cookie >= key.READDIR_COOKIE_BIAS);
    const rest = try h.readdirNames(d, cookie, &buf);
    try testing.expectEqual(@as(usize, 1), rest.len);
    try testing.expect(!std.mem.eql(u8, rest, first_name));
    // From offset 1 (`.` consumed): `..` then the entries.
    try testing.expect(std.mem.startsWith(u8, try h.readdirNames(d, key.READDIR_OFFSET_DOT, &buf), "..,"));
    // A bad cookie is EINVAL.
    const bad: abi.fuse_read_in = .{ .fh = 1, .offset = std.math.maxInt(u64), .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    try testing.expectEqual(linux.E.INVAL, h.call(.{ .readdir = bad }, d).errno().?);
    // rmdir refuses a non-empty directory; unlink refuses a directory.
    try testing.expectEqual(linux.E.NOTEMPTY, h.call(.{ .rmdir = .{ .name = "d" } }, 1).errno().?);
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .unlink = .{ .name = "d" } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .rmdir = .{ .name = "a" } }, d).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "a" } }, d).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "b" } }, d).errno());
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .unlink = .{ .name = "a" } }, d).errno().?);
    // `.` and `..` resolve through the memo.
    try testing.expectEqual(d, (try h.entry(h.lookup(d, "."))).nodeid);
    try testing.expectEqual(@as(u64, 1), (try h.entry(h.lookup(d, ".."))).nodeid);
    try testing.expectEqual(@as(u64, 1), (try h.entry(h.lookup(1, ".."))).nodeid);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rmdir = .{ .name = "d" } }, 1).errno());
    try testing.expectEqual(@as(u32, 2), (try h.getattr(1)).nlink);
    // Negative entry: nodeid 0 with the negative TTL.
    const neg = try h.entry(h.lookup(1, "d"));
    try testing.expectEqual(@as(u64, 0), neg.nodeid);
    try testing.expectEqual(@as(u64, 1), neg.entry_valid);
    // ENAMETOOLONG on a 256-byte name; EEXIST on a duplicate.
    const long: [256]u8 = @splat('x');
    try testing.expectEqual(linux.E.NAMETOOLONG, h.lookup(1, &long).errno().?);
    _ = try h.mkdir(1, "e");
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .mkdir = .{ .name = "e", .mode = 0o755, .umask = 0 } }, 1).errno().?);
}

test "POSIX: rename — plain, replace, NOREPLACE → EEXIST, EXCHANGE, a directory move updates `..`, a name across directories" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const x = try h.create(1, "x", 0o644);
    h.release(x);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "x", .newname = "y" } }, 1).errno());
    try testing.expectEqual(@as(u64, 0), (try h.entry(h.lookup(1, "x"))).nodeid);
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "y"))).nodeid);
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "x", .newname = "q" } }, 1).errno().?);
    // Replace: z's inode drops to nlink 0 (a corpse until FORGET).
    const z = try h.create(1, "z", 0o644);
    h.release(z);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "y", .newname = "z" } }, 1).errno());
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "z"))).nodeid);
    try testing.expectEqual(@as(u32, 0), (try h.getattr(z)).nlink);
    // NOREPLACE over an existing name.
    const w = try h.create(1, "w", 0o644);
    h.release(w);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_NOREPLACE, .name = "w", .newname = "z" } }, 1).errno().?);
    // EXCHANGE swaps the two names' inos.
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_EXCHANGE, .name = "w", .newname = "z" } }, 1).errno());
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "w"))).nodeid);
    try testing.expectEqual(w, (try h.entry(h.lookup(1, "z"))).nodeid);
    try testing.expectEqual(linux.E.INVAL, h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_EXCHANGE | RENAME_NOREPLACE, .name = "w", .newname = "z" } }, 1).errno().?);
    // A directory move: both parents' nlink shift and `..` follows.
    const d1 = try h.mkdir(1, "d1");
    const d2 = try h.mkdir(1, "d2");
    const sub = try h.mkdir(d1, "sub");
    try testing.expectEqual(@as(u32, 3), (try h.getattr(d1)).nlink);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = d2, .flags = 0, .name = "sub", .newname = "moved" } }, d1).errno());
    try testing.expectEqual(@as(u32, 2), (try h.getattr(d1)).nlink);
    try testing.expectEqual(@as(u32, 3), (try h.getattr(d2)).nlink);
    try testing.expectEqual(d2, (try h.entry(h.lookup(sub, ".."))).nodeid);
    try testing.expectEqual(sub, (try h.entry(h.lookup(d2, "moved"))).nodeid);
    // A directory cannot replace a file and vice versa.
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "w", .newname = "d1" } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "d1", .newname = "w" } }, 1).errno().?);
}

test "POSIX: symlink/readlink and link/unlink — nlink moves through the economy, the target rides the create entry, a directory cannot be linked" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const l = (try h.entry(h.call(.{ .symlink = .{ .name = "l", .link = "/target/path" } }, 1))).nodeid;
    const la = try h.getattr(l);
    try testing.expectEqual(@as(u32, S.IFLNK | 0o777), la.mode);
    try testing.expectEqual(@as(u64, 12), la.size);
    const rl = h.call(.readlink, l);
    try testing.expectEqualStrings("/target/path", rl.body);
    const long_target: [4096]u8 = @splat('t');
    try testing.expectEqual(linux.E.NAMETOOLONG, h.call(.{ .symlink = .{ .name = "l2", .link = &long_target } }, 1).errno().?);
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    try testing.expectEqual(linux.E.INVAL, h.call(.readlink, f).errno().?);
    const linked = try h.entry(h.call(.{ .link = .{ .oldnodeid = f, .newname = "f2" } }, 1));
    try testing.expectEqual(f, linked.nodeid);
    try testing.expectEqual(@as(u32, 2), linked.attr.nlink);
    try testing.expectEqual(@as(u32, 2), (try h.getattr(f)).nlink);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .link = .{ .oldnodeid = f, .newname = "f2" } }, 1).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    try testing.expectEqual(@as(u32, 1), (try h.getattr(f)).nlink);
    try testing.expectEqual(f, (try h.entry(h.lookup(1, "f2"))).nodeid);
    const d = try h.mkdir(1, "d");
    try testing.expectEqual(linux.E.ACCES, h.call(.{ .link = .{ .oldnodeid = d, .newname = "dlink" } }, 1).errno().?);
    // mknod: a fifo with the mode and rdev the kernel sent.
    const fifo = try h.entry(h.call(.{ .mknod = .{ .name = "p", .mode = S.IFIFO | 0o600, .rdev = 0, .umask = 0 } }, 1));
    try testing.expectEqual(@as(u32, S.IFIFO | 0o600), fifo.attr.mode);
    const dev = try h.entry(h.call(.{ .mknod = .{ .name = "c", .mode = S.IFCHR | 0o600, .rdev = 0x0501, .umask = 0 } }, 1));
    try testing.expectEqual(@as(u32, 0x0501), dev.attr.rdev);
}

test "POSIX: xattrs — set/get/list/remove, the size probe, ERANGE, CREATE/REPLACE flags, E2BIG, the reserved names are screened" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    _ = h.write(f, 0, "data", 0);
    h.release(f);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.a", .value = "alpha", .flags = 0 } }, f).errno());
    const probe = h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f);
    try testing.expectEqual(@as(u32, 5), (abi.decodeExact(abi.fuse_getxattr_out, probe.outBytes()) orelse return error.TestBadReply).size);
    try testing.expectEqual(linux.E.RANGE, h.call(.{ .getxattr = .{ .name = "user.a", .size = 3 } }, f).errno().?);
    try testing.expectEqualStrings("alpha", h.call(.{ .getxattr = .{ .name = "user.a", .size = 64 } }, f).body);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .setxattr = .{ .name = "user.a", .value = "x", .flags = XATTR_CREATE } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .setxattr = .{ .name = "user.b", .value = "x", .flags = XATTR_REPLACE } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.a", .value = "beta", .flags = XATTR_REPLACE } }, f).errno());
    try testing.expectEqualStrings("beta", h.call(.{ .getxattr = .{ .name = "user.a", .size = 64 } }, f).body);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.b", .value = "", .flags = 0 } }, f).errno());
    // The list hides `layout` and `system.symlink`.
    const lp = h.call(.{ .listxattr = .{ .size = 0 } }, f);
    try testing.expectEqual(@as(u32, 14), (abi.decodeExact(abi.fuse_getxattr_out, lp.outBytes()) orelse return error.TestBadReply).size);
    const list = h.call(.{ .listxattr = .{ .size = 256 } }, f);
    try testing.expectEqual(@as(usize, 14), list.body.len);
    try testing.expect(std.mem.indexOf(u8, list.body, "user.a\x00") != null);
    try testing.expect(std.mem.indexOf(u8, list.body, "user.b\x00") != null);
    try testing.expect(std.mem.indexOf(u8, list.body, "layout") == null);
    try testing.expectEqual(linux.E.RANGE, h.call(.{ .listxattr = .{ .size = 5 } }, f).errno().?);
    // Reserved names: never readable, never settable, never removable as user data.
    try testing.expectEqual(linux.E.OPNOTSUPP, h.call(.{ .setxattr = .{ .name = "layout", .value = "x", .flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "layout", .size = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .removexattr = .{ .name = "system.symlink" } }, f).errno().?);
    // E2BIG past the record cap; removal; a second removal is ENODATA.
    const huge: [70000]u8 = @splat('v');
    try testing.expectEqual(linux.E.@"2BIG", h.call(.{ .setxattr = .{ .name = "user.big", .value = &huge, .flags = 0 } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .removexattr = .{ .name = "user.a" } }, f).errno());
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .removexattr = .{ .name = "user.a" } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f).errno().?);
    // The data record is untouched by the xattr traffic.
    try testing.expectEqualStrings("data", h.read(f, 0, 64).body);
}

test "POSIX: fsync reaches the store's barrier; statfs reports the heap in pages, NAME_MAX and the ino space; opendir refuses a file; open refuses a directory" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsync = .{ .fh = 1, .datasync = false } }, f).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsyncdir = .{ .fh = 1, .datasync = true } }, 1).errno());
    try testing.expectEqual(@as(u64, 2), h.store.fsyncs);
    const st = abi.decodeExact(abi.fuse_statfs_out, h.call(.statfs, 1).outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(@as(u64, (64 << 20) / 4096), st.st.blocks);
    try testing.expectEqual(@as(u64, (48 << 20) / 4096), st.st.bfree);
    try testing.expectEqual(@as(u32, 255), st.st.namelen);
    try testing.expectEqual(@as(u32, 4096), st.st.bsize);
    try testing.expect(st.st.ffree < st.st.files);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .opendir = .{ .flags = 0, .open_flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .open = .{ .flags = 0, .open_flags = 0 } }, 1).errno().?);
    const od = h.call(.{ .opendir = .{ .flags = 0, .open_flags = 0 } }, 1);
    try testing.expectEqual(@as(?linux.E, null), od.errno());
    _ = h.call(.{ .releasedir = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, 1);
    // ENOSYS for the rung-4 and never ops; access under default_permissions.
    try testing.expectEqual(linux.E.NOSYS, h.call(.{ .access = .{ .mask = 4 } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOSYS, h.call(.{ .fallocate = abi.zeroed(abi.fuse_fallocate_in) }, f).errno().?);
    h.release(f);
}

test "unlinked-open file: readable until the final FORGET; the forget gathers the corpse; the drain destroys inode + layout in ONE commit; open_count_stranded stays 0 and the open table closes" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const inodes0 = h.store.countKind(key.TREE_INODES);
    const xattrs0 = h.store.countKind(key.TREE_XATTRS);
    const f = try h.create(1, "f", 0o644);
    _ = h.write(f, 0, "payload", 0);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    // Open and unlinked: still readable, nlink 0.
    try testing.expectEqual(@as(u32, 0), (try h.getattr(f)).nlink);
    try testing.expectEqualStrings("payload", h.read(f, 0, 64).body);
    try testing.expectEqual(inodes0 + 1, h.store.countKind(key.TREE_INODES));
    h.release(f);
    const commits0 = h.store.commits;
    const stranded0 = counters.tripwireCount("open_count_stranded");
    // The CREATE counted one lookup reference; the final FORGET evicts it.
    h.forget(f, 1);
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
    try testing.expectEqual(@as(?open_table.Entry, null), h.opens.get(f));
    // Gathered, not yet destroyed (the batch is 64).
    try testing.expectEqual(inodes0 + 1, h.store.countKind(key.TREE_INODES));
    try testing.expectEqual(@as(usize, 1), h.fs.reclaim.items.len);
    var tr = @import("op_trace.zig").OpTrace.begin(1, .other);
    var win: [16]u8 = undefined;
    var c: Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &tr, .wd_slot = null, .window = &win };
    try h.fs.reclaimDrain(&c);
    tr.finish();
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(inodes0, h.store.countKind(key.TREE_INODES));
    try testing.expectEqual(xattrs0, h.store.countKind(key.TREE_XATTRS));
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, f).errno().?);
    try testing.expectEqual(@as(usize, 0), h.fs.reclaim.items.len);
    // A forget of a LIVE inode never gathers.
    const g = try h.create(1, "g", 0o644);
    h.release(g);
    h.forget(g, 1);
    try testing.expectEqual(@as(usize, 0), h.fs.reclaim.items.len);
    try testing.expectEqual(@as(u64, 0), h.opens.openCount());
}

test "the SETATTR echo is absorbed (zero commits) while real news commits; nlookup closure: every entry reply is one reference and the batch forget returns them all" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const stranded0 = counters.tripwireCount("open_count_stranded");
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    const a = try h.getattr(f);
    const commits0 = h.store.commits;
    const absorbed0 = economy.stats.meta_kv_times_echo_absorbed.load();
    // The kernel's writeback-cache echo: times-only, mtime == stored, ctime ahead.
    var echo = abi.zeroed(abi.fuse_setattr_in);
    echo.valid = abi.FATTR_MTIME | abi.FATTR_CTIME;
    echo.mtime = a.mtime;
    echo.mtimensec = a.mtimensec;
    echo.ctime = a.ctime + 1;
    echo.ctimensec = 0;
    const r = try h.attr(h.setattr(f, echo));
    try testing.expectEqual(commits0, h.store.commits);
    try testing.expectEqual(absorbed0 + 1, economy.stats.meta_kv_times_echo_absorbed.load());
    try testing.expectEqual(a.ctime + 1, r.ctime);
    // The folded view serves the refined ctime; the stored record still carries the old one.
    try testing.expectEqual(a.ctime + 1, (try h.getattr(f)).ctime);
    // chmod is real news: one commit, the mode and a bumped ctime.
    var chmod = abi.zeroed(abi.fuse_setattr_in);
    chmod.valid = abi.FATTR_MODE;
    chmod.mode = 0o600;
    const m = try h.attr(h.setattr(f, chmod));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(@as(u32, S.IFREG | 0o600), m.mode);
    // nlookup closure: three lookups + the create = 4 references; a batch
    // returning 3 keeps the entry, the last one evicts.
    _ = try h.entry(h.lookup(1, "f"));
    _ = try h.entry(h.lookup(1, "f"));
    _ = try h.entry(h.lookup(1, "f"));
    try testing.expectEqual(@as(u64, 4), h.opens.get(f).?.lookups);
    const entries = [_]abi.fuse_forget_one{ .{ .nodeid = f, .nlookup = 2 }, .{ .nodeid = f, .nlookup = 1 } };
    h.fire(.{ .batch_forget = .{ .entries = &entries } }, 0);
    try testing.expectEqual(@as(u64, 1), h.opens.get(f).?.lookups);
    h.forget(f, 1);
    try testing.expectEqual(@as(?open_table.Entry, null), h.opens.get(f));
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
}

test "KILLPRIV_V2: a flagged write clears suid (and sgid only with group-exec) and drops security.capability in the write's commit; an unflagged write clears nothing; a flagged truncate and an O_TRUNC open fold the strip too" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o6755);
    try testing.expectEqual(@as(u32, S.IFREG | 0o6755), (try h.getattr(f)).mode);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = CAPS_XATTR, .value = "caps", .flags = 0 } }, f).errno());
    const clears0 = fuse_killpriv_clears.load();
    const commits0 = h.store.commits;
    _ = h.write(f, 0, "x", 0);
    try testing.expectEqual(@as(u32, S.IFREG | 0o6755), (try h.getattr(f)).mode);
    try testing.expectEqual(clears0, fuse_killpriv_clears.load());
    _ = h.write(f, 0, "y", abi.FUSE_WRITE_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, S.IFREG | 0o755), (try h.getattr(f)).mode);
    try testing.expectEqual(clears0 + 1, fuse_killpriv_clears.load());
    try testing.expectEqual(commits0 + 2, h.store.commits);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = CAPS_XATTR, .size = 0 } }, f).errno().?);
    // sgid without group-exec survives (the mandatory-locking marker).
    const g = try h.create(1, "g", 0o2745);
    _ = h.write(g, 0, "y", abi.FUSE_WRITE_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, S.IFREG | 0o2745), (try h.getattr(g)).mode);
    // A flagged truncate (chown/truncate's FATTR_KILL_SUIDGID).
    const t = try h.create(1, "t", 0o4711);
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE | abi.FATTR_KILL_SUIDGID;
    sa.size = 0;
    const ta = try h.attr(h.setattr(t, sa));
    try testing.expectEqual(@as(u32, S.IFREG | 0o711), ta.mode);
    // An O_TRUNC open with FUSE_OPEN_KILL_SUIDGID truncates and strips in one commit.
    const o = try h.create(1, "o", 0o4755);
    _ = h.write(o, 0, "content", 0);
    h.release(o);
    const commits1 = h.store.commits;
    const op = h.call(.{ .open = .{ .flags = O_TRUNC, .open_flags = abi.FUSE_OPEN_KILL_SUIDGID } }, o);
    try testing.expectEqual(@as(?linux.E, null), op.errno());
    try testing.expectEqual(commits1 + 1, h.store.commits);
    const oa = try h.getattr(o);
    try testing.expectEqual(@as(u64, 0), oa.size);
    try testing.expectEqual(@as(u32, S.IFREG | 0o755), oa.mode);
    try testing.expectEqual(@as(usize, 0), h.read(o, 0, 64).body.len);
    h.release(o);
    h.release(f);
    h.release(g);
    h.release(t);
}

test "READDIRPLUS lists entries with attrs and both TTL classes, counts a lookup reference per entry, and the dots carry nodeid 0" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const d = try h.mkdir(1, "d");
    const a = try h.create(d, "a", 0o644);
    h.release(a);
    const sub = try h.mkdir(d, "sub");
    const l0 = h.opens.get(a).?.lookups;
    const in: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    const c = h.call(.{ .readdirplus = in }, d);
    try testing.expectEqual(@as(?linux.E, null), c.errno());
    var off: usize = 0;
    var n: usize = 0;
    var seen_sub = false;
    while (off + abi.fuse_direntplus.NAME_OFFSET <= c.body.len) : (n += 1) {
        const rec = abi.decodePrefix(abi.fuse_direntplus, c.body[off..]).?;
        const name = c.body[off + abi.fuse_direntplus.NAME_OFFSET ..][0..rec.dirent.namelen];
        if (std.mem.eql(u8, name, ".") or std.mem.eql(u8, name, "..")) {
            try testing.expectEqual(@as(u64, 0), rec.entry_out.nodeid);
        } else if (std.mem.eql(u8, name, "sub")) {
            seen_sub = true;
            try testing.expectEqual(sub, rec.entry_out.nodeid);
            try testing.expectEqual(@as(u32, S.IFDIR | 0o755), rec.entry_out.attr.mode);
            try testing.expectEqual(@as(u32, 4), rec.dirent.type);
            try testing.expectEqual(@as(u64, 1), rec.entry_out.entry_valid);
            try testing.expectEqual(@as(u64, 1), rec.entry_out.attr_valid);
        }
        off += abi.fuse_direntplus.recordLen(rec.dirent.namelen);
    }
    try testing.expectEqual(@as(usize, 4), n);
    try testing.expect(seen_sub);
    try testing.expectEqual(l0 + 1, h.opens.get(a).?.lookups);
    try testing.expectEqual(d, (try h.entry(h.lookup(sub, ".."))).nodeid);
}

test "every reply's errno is a row of the ONE table" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const probes = [_]struct { req: codec.Request, nodeid: u64, want: errors.Error }{
        .{ .req = .{ .lookup = .{ .name = "" } }, .nodeid = 1, .want = error.InvalidArgument },
        .{ .req = .{ .getattr = .{ .flags = 0, .fh = null } }, .nodeid = 999, .want = error.FileNotFound },
        .{ .req = .{ .rmdir = .{ .name = "nope" } }, .nodeid = 1, .want = error.FileNotFound },
        .{ .req = .{ .opendir = .{ .flags = 0, .open_flags = 0 } }, .nodeid = 999, .want = error.FileNotFound },
        .{ .req = .{ .removexattr = .{ .name = "user.x" } }, .nodeid = 1, .want = error.NoAttribute },
        .{ .req = .{ .bmap = .{ .block = 0, .blocksize = 4096 } }, .nodeid = 1, .want = error.NotImplemented },
    };
    for (probes) |p| {
        const c = h.call(p.req, p.nodeid);
        try testing.expectEqual(errors.toErrno(p.want), c.errno().?);
    }
}
