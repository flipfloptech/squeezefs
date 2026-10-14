//! THE ONE APPLIER (charter §5.5 "applied by ONE applier under the
//! holder's door"; `docs/porting/sym_tokens_intents_striping.md` §2.2):
//! every step — the live path's local step, the shipped step served at
//! its holder, every roll-forward's replay and every compensation's
//! inverse — enters here, so "idempotent is a property of one code path
//! rather than a claim about two". ONE whole-tx entry per step; the
//! intent RIDER (the `Put` of the record on step 0, the `Delete` at the
//! retirement) staged beside it when the rider's home is the step's slot
//! — "the intent can never disagree with the first effect, and it costs
//! no extra entry"; the rider is committed even when the witness declines
//! the effect. The witness is read AT THE HOLDER off its own records
//! (this store IS the holder's); the outcome is `applied | already_applied
//! | foreign_skipped` with the post-image a count/create step answers.
//!
//! Generic over the `Store` contract `fuse/handler/kv_fs.zig` names
//! (`begin/abandon/commit/lookup/range/nameKey/fsync`): the product's
//! `MetaSet` (the commit door inside its commit), the portable `MemStore`
//! in the contracts. The store's commit takes the DOOR — a `SlotBusy`
//! there is the retryable class the dispatcher re-resolves on.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const key = @import("../../key.zig");
const record = @import("../../record.zig");
const tx = @import("../../tx.zig");
const tree = @import("../../tree.zig");
const economy = @import("../../economy.zig");
const route_table = @import("../../routing/route_table.zig");
const intent = @import("intent.zig");
const steps = @import("steps.zig");

const assert = std.debug.assert;
const S = economy.S;
pub const InodeValue = record.InodeValue;
pub const Step = steps.Step;
pub const Outcome = steps.Outcome;

pub const stats = struct {
    /// Steps this applier applied (own and served alike).
    pub var xv_steps_applied: counters.ShardedCounter = .{};
    /// Steps whose witness said "already done" (a replay, a resend).
    pub var xv_steps_already_applied: counters.ShardedCounter = .{};
    /// Steps whose witness named another outcome — a live plan stops and
    /// compensates (a racing creator's name landed first: the op's
    /// EEXIST/ENOENT); a roll-forward retires. A counter, not a tripwire:
    /// the race is legal, only its FREQUENCY is a posture word.
    pub var xv_steps_foreign_skipped: counters.ShardedCounter = .{};
    /// An insert into a parent whose record reads `nlink 0` or is absent:
    /// refused as a witness so the plan stops (live) or retires (replay).
    pub var xv_cross_owner_dying_parent_refusals: counters.ShardedCounter = .{};
    /// A minted-but-never-named child destroyed by a compensation.
    pub var xv_cross_owner_mints_destroyed: counters.ShardedCounter = .{};
};

pub const Error = error{
    OutOfMemory,
    Io,
    Corrupt,
    /// The door refused: another appender leases the step's slot (or the
    /// slot left this volume) — the retryable class.
    SlotBusy,
    /// The door's first touch was deferred: nothing written, retry.
    Deferred,
    ValueTooLarge,
    NoSpace,
    /// The dentry chain of the name is full (the 257th same-hash name).
    ChainFull,
};

pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.Corrupt => error.InputOutput,
        error.SlotBusy => error.SlotMoved,
        error.Deferred => error.Deferred,
        error.ValueTooLarge => error.ValueTooLarge,
        error.NoSpace, error.ChainFull => error.NoSpaceLeft,
    };
}

/// The intent record riding a step's entry.
pub const Rider = union(enum) {
    none,
    /// Step 0: the record's `Put` (the encoded image).
    put: struct { home: u64, tx_id: u64, image: []const u8 },
    /// The retirement (or the ABORTING flip): the exact key.
    delete: struct { home: u64, tx_id: u64 },

    pub fn homeOf(self: Rider) ?u64 {
        return switch (self) {
            .none => null,
            .put => |p| p.home,
            .delete => |d| d.home,
        };
    }
};

pub fn assertStore(comptime T: type) void {
    comptime {
        for ([_][]const u8{ "StoreError", "coreError", "begin", "abandon", "commit", "lookup", "range", "fsync", "nameKey" }) |name| {
            if (!@hasDecl(T, name)) @compileError(@typeName(T) ++ " is not an xv Store: lacks `" ++ name ++ "`");
        }
    }
}

/// A dentry as found by the chain scan.
pub const Found = struct {
    key: [key.DENTRY_KEY_LEN]u8,
    child: u64,
    file_type: u8,
};

pub fn Applier(comptime Store: type) type {
    assertStore(Store);
    return struct {
        const Self = @This();

        gpa: Allocator,
        io: Io,
        store: *Store,

        pub fn init(gpa: Allocator, io: Io, store: *Store) Self {
            return .{ .gpa = gpa, .io = io, .store = store };
        }

        fn storeErr(e: Store.StoreError) Error {
            return switch (Store.coreError(e)) {
                error.OutOfMemory => error.OutOfMemory,
                error.SlotMoved => error.SlotBusy,
                error.Deferred => error.Deferred,
                error.ValueTooLarge => error.ValueTooLarge,
                error.NoSpaceLeft => error.NoSpace,
                else => error.Io,
            };
        }

        // ---- the holder's own records ----

        /// The record under KEY ino `k` (no pending-times fold: the step
        /// witnesses durable state; the holder's `PendingTimes` are its
        /// FUSE layer's and drain to the same record).
        pub fn readInodeKey(self: *Self, k: u64) Error!?InodeValue {
            const bytes = (self.store.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(k)) catch |e| return storeErr(e)) orelse return null;
            defer self.gpa.free(bytes);
            return InodeValue.decode(bytes) catch error.Corrupt;
        }

        /// The record of GLOBAL ino `global`.
        pub fn readInode(self: *Self, global: u64) Error!?InodeValue {
            return self.readInodeKey(route_table.effectiveIno(global));
        }

        /// The live chain of `(parent, hash54(name))`: the matching dentry
        /// (by FULL name) and every chain key (for the insert's probe).
        pub fn scanChain(self: *Self, parent_key: u64, name: []const u8, kvs: *std.ArrayList(tree.KeyValue)) Error!?Found {
            const r = tx.dentryChainRange(parent_key, key.dentryNameHash54(name, self.store.nameKey()));
            self.store.range(self.gpa, key.TREE_DENTRIES, &r.start, &r.end, 256, kvs) catch |e| return storeErr(e);
            var found: ?Found = null;
            for (kvs.items) |kv| {
                const dv = record.DentryValue.decode(kv.value) catch return error.Corrupt;
                if (std.mem.eql(u8, dv.name, name)) {
                    var f: Found = .{ .key = undefined, .child = dv.child_ino, .file_type = dv.file_type };
                    @memcpy(&f.key, kv.key[0..key.DENTRY_KEY_LEN]);
                    found = f;
                }
            }
            return found;
        }

        pub fn findDentry(self: *Self, parent: u64, name: []const u8) Error!?Found {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            return self.scanChain(route_table.effectiveIno(parent), name, &kvs);
        }

        /// Whether a directory (KEY ino) holds any dentry — a NUL-led
        /// marker counts too; the striping layer screens those itself.
        pub fn dirHasEntries(self: *Self, dir_key: u64) Error!bool {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const start = key.dentryKey(dir_key, 0, 0);
            const end = key.dentryKey(dir_key, key.HASH54_MAX, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_DENTRIES, &start, &end, 1, &kvs) catch |e| return storeErr(e);
            return kvs.items.len != 0;
        }

        // ---- the rider ----

        fn stageRider(t: *tx.KvTx, rider: Rider) Error!void {
            switch (rider) {
                .none => {},
                .put => |p| {
                    var vbuf: [record.RECORD_VALUE_CAP_CEILING + 1 + intent.NAME_LEN]u8 = undefined;
                    if (intent.storedValueLen(p.image.len) > vbuf.len) return error.ValueTooLarge;
                    const v = intent.storedValue(p.tx_id, p.image, &vbuf);
                    try t.stagePut(key.TREE_XATTRS, &intent.recordKey(p.home, p.tx_id), v);
                },
                .delete => |d| try t.stageDelete(key.TREE_XATTRS, &intent.recordKey(d.home, d.tx_id)),
            }
        }

        /// The rider alone: the intent written before any foreign step
        /// (step 0 is another appender's), or the retirement / the ABORTING
        /// flip as its own entry.
        pub fn commitRiderAlone(self: *Self, rider: Rider, guards: tx.GuardSet) Error!void {
            assert(rider != .none);
            const t = self.store.begin(@src()) catch return error.OutOfMemory;
            var owned = true;
            errdefer if (owned) self.store.abandon(t);
            try stageRider(t, rider);
            // A control record no read token carries.
            t.token_quiet = true;
            t.holdGuards(guards);
            owned = false;
            self.store.commit(t) catch |e| return storeErr(e);
        }

        /// The initiator's barrier (its ring durable before any later
        /// participant's entry is submitted).
        pub fn barrier(self: *Self) Error!void {
            self.store.fsync() catch |e| return storeErr(e);
        }

        /// The intent record's stored bytes under `home`, if any (owned by
        /// the caller — the decoded steps' names borrow them).
        pub fn readIntentBytes(self: *Self, home: u64, tx_id: u64) Error!?[]u8 {
            return self.store.lookup(self.gpa, key.TREE_XATTRS, &intent.recordKey(home, tx_id)) catch |e| return storeErr(e);
        }

        // ---- the apply ----

        /// Apply `step` under the caller's guards (`guards` travels into
        /// the entry; `.none` when the op's set is held elsewhere), with
        /// the rider beside it. `now` stamps the parent updates.
        pub fn apply(self: *Self, step: Step, rider: Rider, guards: tx.GuardSet, now: u64) Error!Outcome {
            const t = self.store.begin(@src()) catch return error.OutOfMemory;
            var owned = true;
            errdefer if (owned) self.store.abandon(t);
            const out = try self.stageStep(t, step, now);
            try stageRider(t, rider);
            if (t.isEmpty()) {
                // Nothing to commit: the witness declined and no rider.
                owned = false;
                var g = guards;
                g.release();
                self.store.abandon(t);
            } else {
                t.holdGuards(guards);
                owned = false;
                self.store.commit(t) catch |e| return storeErr(e);
            }
            switch (out.status) {
                .applied => stats.xv_steps_applied.inc(),
                .already_applied => stats.xv_steps_already_applied.inc(),
                .foreign_skipped => stats.xv_steps_foreign_skipped.inc(),
            }
            return out;
        }

        fn stageStep(self: *Self, t: *tx.KvTx, step: Step, now: u64) Error!Outcome {
            switch (step) {
                .remove_dentry => |s| {
                    const parent_key = route_table.effectiveIno(s.parent);
                    const found = (try self.findDentry(s.parent, s.name)) orelse return .{ .status = .already_applied };
                    if (found.child != s.expect_child) return .{ .status = .foreign_skipped };
                    t.noteName(parent_key, s.name);
                    try t.stageDelete(key.TREE_DENTRIES, &found.key);
                    try self.stageParent(t, parent_key, s.parent_update, -1, now);
                    return .{ .status = .applied };
                },
                .insert_dentry => |s| {
                    const parent_key = route_table.effectiveIno(s.parent);
                    var kvs: std.ArrayList(tree.KeyValue) = .empty;
                    defer tree.deinitKeyValues(self.gpa, &kvs);
                    if (try self.scanChain(parent_key, s.name, &kvs)) |found| {
                        return .{ .status = if (found.child == s.child) .already_applied else .foreign_skipped };
                    }
                    // A dying parent (nlink 0, or no record) never takes a
                    // name: a live plan stops and compensates, a replay
                    // retires instead of erroring at every cadence pass.
                    const pv = try self.readInodeKey(parent_key);
                    if (pv == null or pv.?.nlink == 0 or !economy.isDirMode(pv.?.mode)) {
                        stats.xv_cross_owner_dying_parent_refusals.inc();
                        return .{ .status = .foreign_skipped };
                    }
                    var chain: [256][]const u8 = undefined;
                    for (kvs.items, 0..) |kv, i| chain[i] = kv.key;
                    const dk = tx.dentryInsertKey(t, self.store.nameKey(), parent_key, s.name, chain[0..kvs.items.len]) catch |e| return switch (e) {
                        error.NameTooLong => error.Corrupt,
                        error.DentryChainOverflow => error.ChainFull,
                    };
                    var vbuf: [@sizeOf(record.DentryValueHeader) + record.NAME_MAX]u8 = undefined;
                    const dv = (record.DentryValue{ .child_ino = s.child, .file_type = @intCast(s.ft_bits >> 12), .name = s.name }).encodeInto(&vbuf) catch return error.Corrupt;
                    t.noteName(parent_key, s.name);
                    try t.stagePut(key.TREE_DENTRIES, &dk, dv);
                    try self.stageParentWith(t, parent_key, pv.?, s.parent_update, 1, now);
                    return .{ .status = .applied };
                },
                .set_nlink => |s| {
                    const k = route_table.effectiveIno(s.ino);
                    var v = (try self.readInodeKey(k)) orelse return .{ .status = .foreign_skipped };
                    if (v.nlink == s.post and s.pre != s.post) return .{ .status = .already_applied, .inode = v };
                    if (v.nlink != s.pre) return .{ .status = .foreign_skipped };
                    v.nlink = s.post;
                    // The ctime bumps MONOTONE over the stored view.
                    if (s.ctime) |c| if (economy.laterThan(c, v.ctime)) {
                        v.ctime = c;
                    };
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode());
                    return .{ .status = .applied, .inode = v };
                },
                .touch_ctime => |s| {
                    const k = route_table.effectiveIno(s.ino);
                    const v = (try self.readInodeKey(k)) orelse return .{ .status = .foreign_skipped };
                    // Forward-only: a replay over a later stamp is a no-op.
                    if (!economy.laterThan(s.ctime, v.ctime)) return .{ .status = .already_applied, .inode = v };
                    try t.stageDelta(key.TREE_INODES, &key.inodeKey(k), record.InodeDelta.ctime(s.ctime));
                    var post = v;
                    post.ctime = s.ctime;
                    return .{ .status = .applied, .inode = post };
                },
                .mint_inode => |s| {
                    const k = route_table.effectiveIno(s.ino);
                    if (try self.readInodeKey(k)) |v| return .{ .status = .already_applied, .inode = v };
                    const v: InodeValue = .{ .mode = s.mode, .uid = s.uid, .gid = s.gid, .nlink = 1, .rdev = s.rdev, .atime = now, .mtime = now, .ctime = now };
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode());
                    return .{ .status = .applied, .inode = v };
                },
                .create_inode => |s| {
                    const k = route_table.effectiveIno(s.ino);
                    // "Never EEXIST": a replayed create answers the stored
                    // record — times = ts_ns, so every apply answers the same.
                    if (try self.readInodeKey(k)) |v| return .{ .status = .already_applied, .inode = v };
                    const v: InodeValue = .{ .mode = s.mode, .uid = s.uid, .gid = s.gid, .nlink = if (economy.isDirMode(s.mode)) 2 else 1, .rdev = s.rdev, .size = s.size, .atime = s.ts_ns, .mtime = s.ts_ns, .ctime = s.ts_ns };
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode());
                    return .{ .status = .applied, .inode = v };
                },
            }
        }

        fn stageParent(self: *Self, t: *tx.KvTx, parent_key: u64, pu: steps.ParentUpdate, dir_delta: i32, now: u64) Error!void {
            if (pu == .none) return;
            if (pu == .shared_times) {
                _ = try economy.stageParentUpdate(t, parent_key, true, 0, now, null);
                return;
            }
            const pv = (try self.readInodeKey(parent_key)) orelse return;
            try self.stageParentWith(t, parent_key, pv, pu, dir_delta, now);
        }

        /// The parent update with the record already in hand: `SharedTimes`
        /// = the Δtime merge; `ExclusiveTimes[Bump]` = the RMW `Put`, the
        /// bump's direction read off the kind (`dir_delta` = +1 insert, −1
        /// remove).
        fn stageParentWith(_: *Self, t: *tx.KvTx, parent_key: u64, pv: InodeValue, pu: steps.ParentUpdate, dir_delta: i32, now: u64) Error!void {
            switch (pu) {
                .none => {},
                .shared_times => _ = try economy.stageParentUpdate(t, parent_key, true, 0, now, null),
                .exclusive_times => _ = try economy.stageParentUpdate(t, parent_key, false, 0, now, pv),
                .exclusive_times_bump => _ = try economy.stageParentUpdate(t, parent_key, false, dir_delta, now, pv),
            }
        }

        /// A minted child the plan never named: the compensation's
        /// destroy — the inode record alone (a fresh mint has no xattr, no
        /// layout, no block).
        pub fn destroyUnnamed(self: *Self, global: u64, guards: tx.GuardSet) Error!void {
            const k = route_table.effectiveIno(global);
            const t = self.store.begin(@src()) catch return error.OutOfMemory;
            var owned = true;
            errdefer if (owned) self.store.abandon(t);
            try t.stageDelete(key.TREE_INODES, &key.inodeKey(k));
            t.holdGuards(guards);
            owned = false;
            self.store.commit(t) catch |e| return storeErr(e);
            stats.xv_cross_owner_mints_destroyed.inc();
        }

        /// Every intent under `home`, ids ascending (the roll-forward
        /// scan; caller frees each key/value).
        pub fn scanHome(self: *Self, home: u64, out: *std.ArrayList(tree.KeyValue)) Error!void {
            const r = intent.homeRange(home);
            self.store.range(self.gpa, key.TREE_XATTRS, &r.start, &r.end, intent.XV_MAX_STEPS * 64, out) catch |e| return storeErr(e);
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: every kind's witness idempotency over the portable MemStore.
// ---------------------------------------------------------------------------

const testing = std.testing;
const kv_fs = @import("../../../fuse/handler/kv_fs.zig");
const inos = @import("../../inos.zig");

const MemApplier = Applier(kv_fs.MemStore);

fn gino(local: u64, slot: u16) u64 {
    return inos.makeGlobalIno(local, slot, inos.ROUTING_WIDTH);
}

fn seedDir(a: *MemApplier, dir_global: u64) !void {
    const t = try a.store.begin(@src());
    try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(dir_global)), &(InodeValue{ .mode = S.IFDIR | 0o755, .nlink = 2, .mtime = 1, .ctime = 1 }).encode());
    try a.store.commit(t);
}

test "every step kind is idempotent under its witness (apply twice ⇒ once) and answers `foreign_skipped` when the witness names another outcome; the rider rides the entry and is committed even when the witness declines" {
    const gpa = testing.allocator;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var a = MemApplier.init(gpa, testing.io, &store);
    const dir = gino(2, 0);
    const child = gino(100, 3);
    try seedDir(&a, dir);
    const applied0 = stats.xv_steps_applied.load();
    const already0 = stats.xv_steps_already_applied.load();
    // CreateInode with the rider: twice ⇒ one record, the same times.
    const cr: Step = .{ .create_inode = .{ .ino = child, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .rdev = 0, .size = 0, .ts_ns = 500 } };
    const rider: Rider = .{ .put = .{ .home = intent.homeIno(3), .tx_id = 77, .image = "img" } };
    const o1 = try a.apply(cr, rider, .none, 9);
    try testing.expectEqual(steps.Status.applied, o1.status);
    try testing.expectEqual(@as(u64, 500), o1.inode.?.mtime);
    const o2 = try a.apply(cr, .none, .none, 9);
    try testing.expectEqual(steps.Status.already_applied, o2.status);
    try testing.expectEqual(@as(u64, 500), o2.inode.?.mtime);
    try testing.expectEqual(@as(usize, 1), store.countKind(key.TREE_XATTRS));
    // InsertDentry: free ⇒ applied (the parent's nlink bumps for a dir);
    // present → child ⇒ already; present → other ⇒ foreign.
    const ins: Step = .{ .insert_dentry = .{ .parent = dir, .name = "f", .child = child, .ft_bits = S.IFREG, .parent_update = .shared_times } };
    try testing.expectEqual(steps.Status.applied, (try a.apply(ins, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.already_applied, (try a.apply(ins, .none, .none, 9)).status);
    var other = ins;
    other.insert_dentry.child = gino(101, 3);
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(other, .none, .none, 9)).status);
    try testing.expectEqual(@as(u64, 9), (try a.readInode(dir)).?.mtime);
    const sub = gino(102, 3);
    try testing.expectEqual(steps.Status.applied, (try a.apply(.{ .create_inode = .{ .ino = sub, .mode = S.IFDIR | 0o755, .uid = 0, .gid = 0, .rdev = 0, .size = 0, .ts_ns = 1 } }, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.applied, (try a.apply(.{ .insert_dentry = .{ .parent = dir, .name = "d", .child = sub, .ft_bits = S.IFDIR, .parent_update = .exclusive_times_bump } }, .none, .none, 10)).status);
    try testing.expectEqual(@as(u32, 3), (try a.readInode(dir)).?.nlink);
    // SetNlink: CAS on pre; post-image on a replay; neither ⇒ foreign.
    const sn: Step = .{ .set_nlink = .{ .ino = child, .pre = 1, .post = 2, .ctime = 600 } };
    const s1 = try a.apply(sn, .none, .none, 9);
    try testing.expectEqual(steps.Status.applied, s1.status);
    try testing.expectEqual(@as(u32, 2), s1.inode.?.nlink);
    try testing.expectEqual(@as(u64, 600), s1.inode.?.ctime);
    const s2 = try a.apply(sn, .none, .none, 9);
    try testing.expectEqual(steps.Status.already_applied, s2.status);
    try testing.expectEqual(@as(u32, 2), s2.inode.?.nlink);
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(.{ .set_nlink = .{ .ino = child, .pre = 5, .post = 6, .ctime = null } }, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(.{ .set_nlink = .{ .ino = gino(999, 3), .pre = 1, .post = 0, .ctime = null } }, .none, .none, 9)).status);
    // TouchCtime: forward-only.
    try testing.expectEqual(steps.Status.applied, (try a.apply(.{ .touch_ctime = .{ .ino = child, .ctime = 700 } }, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.already_applied, (try a.apply(.{ .touch_ctime = .{ .ino = child, .ctime = 650 } }, .none, .none, 9)).status);
    try testing.expectEqual(@as(u64, 700), (try a.readInode(child)).?.ctime);
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(.{ .touch_ctime = .{ .ino = gino(999, 3), .ctime = 1 } }, .none, .none, 9)).status);
    // MintInode: twice ⇒ once.
    const w = gino(103, 0);
    try testing.expectEqual(steps.Status.applied, (try a.apply(.{ .mint_inode = .{ .ino = w, .mode = S.IFCHR, .uid = 0, .gid = 0, .rdev = 0 } }, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.already_applied, (try a.apply(.{ .mint_inode = .{ .ino = w, .mode = S.IFCHR, .uid = 0, .gid = 0, .rdev = 0 } }, .none, .none, 9)).status);
    // RemoveDentry: names expect ⇒ applied; absent ⇒ already; other ⇒ foreign.
    const rm: Step = .{ .remove_dentry = .{ .parent = dir, .name = "f", .expect_child = child, .parent_update = .shared_times } };
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(.{ .remove_dentry = .{ .parent = dir, .name = "f", .expect_child = gino(5, 1), .parent_update = .none } }, .none, .none, 9)).status);
    try testing.expectEqual(steps.Status.applied, (try a.apply(rm, .none, .none, 11)).status);
    try testing.expectEqual(steps.Status.already_applied, (try a.apply(rm, .none, .none, 11)).status);
    try testing.expect((try a.findDentry(dir, "f")) == null);
    // The directory remove drops the parent's nlink back to 2.
    try testing.expectEqual(steps.Status.applied, (try a.apply(.{ .remove_dentry = .{ .parent = dir, .name = "d", .expect_child = sub, .parent_update = .exclusive_times_bump } }, .none, .none, 12)).status);
    try testing.expectEqual(@as(u32, 2), (try a.readInode(dir)).?.nlink);
    // A dying parent refuses the insert as a witness.
    const dying0 = stats.xv_cross_owner_dying_parent_refusals.load();
    try testing.expectEqual(steps.Status.foreign_skipped, (try a.apply(.{ .insert_dentry = .{ .parent = gino(777, 0), .name = "x", .child = child, .ft_bits = S.IFREG, .parent_update = .none } }, .none, .none, 9)).status);
    try testing.expectEqual(dying0 + 1, stats.xv_cross_owner_dying_parent_refusals.load());
    // A declined witness with a rider commits the rider alone.
    const xattrs0 = store.countKind(key.TREE_XATTRS);
    try testing.expectEqual(steps.Status.already_applied, (try a.apply(rm, .{ .delete = .{ .home = intent.homeIno(3), .tx_id = 77 } }, .none, 11)).status);
    try testing.expectEqual(xattrs0 - 1, store.countKind(key.TREE_XATTRS));
    // The counters: applied and already-applied moved; foreign fired.
    try testing.expect(stats.xv_steps_applied.load() > applied0);
    try testing.expect(stats.xv_steps_already_applied.load() > already0);
    // destroyUnnamed removes the record.
    try a.destroyUnnamed(w, .none);
    try testing.expect((try a.readInode(w)) == null);
    // The intent read/scan helpers.
    var out: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
    var img_buf: [512]u8 = undefined;
    const sr = [_]intent.StepRec{cr.toRec()};
    const img = try intent.encode(.create, 0, 78, 0, &sr, 16 << 10, &img_buf);
    try a.commitRiderAlone(.{ .put = .{ .home = intent.homeIno(3), .tx_id = 78, .image = img } }, .none);
    const stored = (try a.readIntentBytes(intent.homeIno(3), 78)).?;
    defer gpa.free(stored);
    try testing.expectEqual(@as(u64, 78), (try intent.decodeStored(&intent.recordKey(intent.homeIno(3), 78), stored, &out)).tx_id);
    try testing.expect((try a.readIntentBytes(intent.homeIno(3), 79)) == null);
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    try a.scanHome(intent.homeIno(3), &kvs);
    try testing.expectEqual(@as(usize, 1), kvs.items.len);
    try a.barrier();
}
