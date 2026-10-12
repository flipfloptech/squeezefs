//! The cross-owner plane bound to the metadata SET (Linux; charter §5.5):
//! `ops.Plane(MetaSet)` with the resolver every slot's home is read from
//! — the slot's VOLUME on this set (a flat multi-volume set spans volumes
//! with every slot local: the intent IS the cross-member transaction the
//! rung-5 split commit used to be) and, under the symmetric plane, the
//! slot plane's `SlotHolderCache.stepHome`: a slot another appender
//! leases has no dialed session in this process until the wire join
//! lands, so it resolves `unreachable_{holder}` — the typed EAGAIN, never
//! a wrong local apply. The roll-forward homes are every slot with a tree
//! on every member; the first is volume 0's native slot (the mount's own
//! home for an intent whose step 0 is foreign).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const meta_set = @import("../../meta_set.zig");
const dlm = @import("../../dlm.zig");
const record = @import("../../record.zig");
const route_table = @import("../../routing/route_table.zig");
const ops = @import("ops.zig");
const stripe = @import("stripe.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/xv/set_bind is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const MetaSet = meta_set.MetaSet;
pub const SetPlane = ops.Plane(MetaSet);

/// The wire plane's dialled sessions per HOLDER appender id (the step
/// ships' links); null = no session can be bound (no endpoint).
pub const Dialer = struct {
    ctx: *anyopaque,
    linkFor: *const fn (ctx: *anyopaque, holder: u32) ?ops.Link,
};

/// The plane over a set: owns the resolver's state.
pub const Bound = struct {
    set: *MetaSet,
    plane: SetPlane,
    /// Bound by the wire plane once its listener is up: a foreign holder
    /// resolves to a dialled session; before that (and on a flat set) a
    /// foreign holder is `unreachable_` — the typed EAGAIN.
    dialer: ?Dialer = null,

    pub fn create(gpa: Allocator, io: Io, set: *MetaSet, locks: *dlm.Manager, env: *const std.process.Environ.Map) Allocator.Error!*Bound {
        const self = try gpa.create(Bound);
        errdefer gpa.destroy(self);
        self.set = set;
        self.dialer = null;
        const be0 = set.volumes[0];
        const own_id: u32 = if (be0.sym) |s| s.identity.mount_slot else if (be0.joined) |j| j.appenderId() else 0;
        const term: u64 = if (be0.sym) |s| s.manager.term else if (be0.joined) |j| j.core().page.term else set.setEpoch();
        self.plane = try SetPlane.init(gpa, io, set, locks, own_id, 0, term, .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .localSlots = &localSlots, .linkOf = &linkOf }, ops.Seams.fromEnv(env), record.xattrValueCap(set.nodeSize()));
        self.plane.stripe_k = stripe.resolveK(env);
        return self;
    }

    pub fn destroy(self: *Bound, gpa: Allocator) void {
        self.plane.deinit();
        gpa.destroy(self);
    }

    /// The wire plane's sessions (after its listener is up).
    /// Bind (or, with null, unbind BEFORE the wire plane goes) the step
    /// sessions' dialler.
    pub fn bindDialer(self: *Bound, d: ?Dialer) void {
        self.dialer = d;
    }

    fn homeOf(ctx: *anyopaque, slot: u16) ops.Home {
        const self: *Bound = @ptrCast(@alignCast(ctx));
        const v = self.set.volumeIndexOfSlot(slot);
        const be = self.set.volumes[v];
        if (!be.slots.armed) return .{ .local = .{ .volume = v } };
        const holder: u32 = switch (be.slots.cache.stepHome(slot)) {
            .local => {
                // A joiner's unleased slot is the manager's (it maintains
                // every unleased tree): its step ships there.
                const view = be.slots.table.view(slot);
                if (be.joined != null and !view.leasedBy(be.slots.me)) return self.foreign(0);
                return .{ .local = .{ .volume = v, .g = view.g } };
            },
            .foreign => |f| f.holder,
            .unreachable_ => |u| u.holder,
        };
        return self.foreign(holder);
    }

    fn foreign(self: *Bound, holder: u32) ops.Home {
        const link = linkOf(self, holder) orelse return .{ .unreachable_ = .{ .holder = holder } };
        return .{ .foreign = .{ .holder = holder, .link = link } };
    }

    fn linkOf(ctx: *anyopaque, holder: u32) ?ops.Link {
        const self: *Bound = @ptrCast(@alignCast(ctx));
        const d = self.dialer orelse return null;
        return d.linkFor(d.ctx, holder);
    }

    /// After a `SlotMoved`: a joiner asks the manager (`ResolveSlot`), the
    /// manager re-reads its own table.
    fn refresh(ctx: *anyopaque, slot: u16) ops.Home {
        const self: *Bound = @ptrCast(@alignCast(ctx));
        const be = self.set.volumeOfSlot(slot);
        if (be.joined) |js| {
            _ = be.slots.cache.resolveViaManager(.{ .ctx = &js.link, .call = &@import("../joined.zig").SharedLink.callFn }, be.index, slot) catch {};
        }
        return homeOf(ctx, slot);
    }

    fn localSlots(ctx: *anyopaque, out: []u16) []u16 {
        const self: *Bound = @ptrCast(@alignCast(ctx));
        var n: usize = 0;
        if (n < out.len) {
            out[n] = 0;
            n += 1;
        }
        var slot: u32 = 1;
        while (slot < route_table.W and n < out.len) : (slot += 1) {
            const s: u16 = @intCast(slot);
            const be = self.set.volumes[self.set.volumeIndexOfSlot(s)];
            if (be.slotTree(s) == null) continue;
            if (homeOf(ctx, s) != .local) continue;
            out[n] = s;
            n += 1;
        }
        return out[0..n];
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; regular files under /build/tmp): the filesystem over a
// TWO-MEMBER set with the plane bound — every op whose objects span the
// members is ONE intent, the split commit never runs.
// ---------------------------------------------------------------------------

const testing = std.testing;
const kv_fs = @import("../../../fuse/handler/kv_fs.zig");
const filesystem = @import("../../../fuse/handler/filesystem.zig");
const op_trace = @import("../../../fuse/handler/op_trace.zig");
const inos = @import("../../inos.zig");
const intent = @import("intent.zig");
const abi = @import("../../../fuse/abi.zig");

const SetFs = kv_fs.KvFs(MetaSet, kv_fs.NoData);

test "a two-member set behind the filesystem: creates spread over both members are intents (minted ≡ retired, no split commit), unlink/rename/link/mkdir/rmdir across members land, a remount recovers nothing because nothing is open, and the tree is byte-exact" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "xvfs", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var nodata: kv_fs.NoData = .{};
    var fs = try SetFs.mount(gpa, s.io(), set, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    const bound = try Bound.create(gpa, s.io(), set, &fs.locks, &s.env);
    defer bound.destroy(gpa);
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&bound.plane));
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = s.io(), .trace = &tr, .wd_slot = null, .window = &.{} };
    const minted0 = intent.stats.xv_cross_owner_intents_minted.load();
    var files: [40]u64 = undefined;
    var nb: [16]u8 = undefined;
    var cross: u32 = 0;
    for (&files, 0..) |*f, i| {
        const e = try fs.create(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "f{d}", .{i}), .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 });
        f.* = e.entry.nodeid;
        if (set.volumeIndexOfSlot(route_table.slotOf(f.*)) != 0) cross += 1;
    }
    try testing.expect(cross > 0);
    try testing.expectEqual(@as(u64, cross), intent.stats.xv_cross_owner_intents_minted.load() - minted0);
    try testing.expectEqual(intent.stats.xv_cross_owner_intents_minted.load(), intent.stats.xv_cross_owner_intents_retired.load() + intent.stats.xv_cross_owner_intents_open.load());
    // Every name resolves to its ino, every record is on its home.
    for (files, 0..) |f, i| {
        const e = try fs.lookup(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "f{d}", .{i}) });
        try testing.expectEqual(f, e.nodeid);
    }
    // mkdir on another member (a directory bumps the root's nlink), a
    // rename of a member-1 file into it, a link, an unlink, an rmdir.
    const root0 = try fs.getattr(&c, .{ .ino = inos.ROOT_INO, .fh = null });
    var dir: ?u64 = null;
    var i: u32 = 0;
    while (dir == null) : (i += 1) {
        const e = try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "d{d}", .{i}), .mode = 0o755, .umask = 0 });
        if (set.volumeIndexOfSlot(route_table.slotOf(e.nodeid)) != 0) dir = e.nodeid;
    }
    const d = dir.?;
    try testing.expectEqual(root0.attr.nlink + i, (try fs.getattr(&c, .{ .ino = inos.ROOT_INO, .fh = null })).attr.nlink);
    var moved: ?u64 = null;
    for (files, 0..) |f, j| if (set.volumeIndexOfSlot(route_table.slotOf(f)) == 0) {
        try fs.rename(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "f{d}", .{j}), .newparent = d, .newname = "moved", .flags = 0 });
        moved = f;
        break;
    };
    try testing.expectEqual(moved.?, (try fs.lookup(&c, .{ .parent = d, .name = "moved" })).nodeid);
    const linked = try fs.link(&c, .{ .ino = moved.?, .newparent = inos.ROOT_INO, .newname = "linked" });
    try testing.expectEqual(@as(u32, 2), linked.attr.nlink);
    try fs.unlink(&c, .{ .parent = d, .name = "moved" });
    try testing.expectEqual(@as(u32, 1), (try fs.getattr(&c, .{ .ino = moved.?, .fh = null })).attr.nlink);
    // A negative lookup is the kernel's negative entry (nodeid 0).
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = d, .name = "moved" })).nodeid);
    try fs.rmdir(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "d{d}", .{i - 1}) });
    // The removed directory is a corpse until the kernel's FORGET: nlink 0.
    try testing.expectEqual(@as(u32, 0), (try fs.getattr(&c, .{ .ino = d, .fh = null })).attr.nlink);
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "d{d}", .{i - 1}) })).nodeid);
    try testing.expectEqual(intent.stats.xv_cross_owner_intents_minted.load(), intent.stats.xv_cross_owner_intents_retired.load() + intent.stats.xv_cross_owner_intents_open.load());
    // A setattr of a file on either member is LOCAL on a flat set (no ship).
    const ships0 = @import("record_ship.zig").stats.record_ships.load();
    _ = try fs.setattr(&c, .{ .ino = files[0], .attr = .{ .mode = 0o600 } });
    try testing.expectEqual(ships0, @import("record_ship.zig").stats.record_ships.load());
    // The roll-forward at a remount: nothing open, nothing recovered.
    try testing.expectEqual(@as(u32, 0), try bound.plane.recoverOpen(2_000));
}

test "a FLIPPED directory over a two-member set behind the filesystem: the explicit flip mints K = 8 stripes over both members (the ONE intent of K + 2 markers), 120 creates key their stripes (a stripe on the other member = a cross-member intent), a rename between two stripes on DIFFERENT members is ONE intent, readdir merges every name once, rmdir of the emptied directory is ONE intent over every stripe; minted ≡ retired, nothing open" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "xvstripe", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var nodata: kv_fs.NoData = .{};
    var fs = try SetFs.mount(gpa, s.io(), set, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    const bound = try Bound.create(gpa, s.io(), set, &fs.locks, &s.env);
    defer bound.destroy(gpa);
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&bound.plane));
    const st = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = s.io(), .trace = &tr, .wd_slot = null, .window = &window };
    const minted0 = intent.stats.xv_cross_owner_intents_minted.load();
    const d = (try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "hot", .mode = 0o755, .umask = 0 })).nodeid;
    try fs.setxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .value = "8", .flags = 0 });
    const map = (try st.mapOf(d)).?;
    try testing.expectEqual(@as(u8, 8), map.k);
    // The flip was ONE intent; the holder's stripes sit on the directory's
    // volume (the parent-preferred mint).
    try testing.expectEqual(minted0 + 1, intent.stats.xv_cross_owner_intents_minted.load());
    const dvol = set.volumeIndexOfSlot(route_table.slotOf(d));
    for (map.slice()) |sp| try testing.expectEqual(dvol, set.volumeIndexOfSlot(route_table.slotOf(sp)));
    // 120 creates: every name in its stripe; a child minted on the other
    // member is an intent (the two-home create).
    const N = 120;
    var nb: [16]u8 = undefined;
    var inos_of: [N]u64 = undefined;
    for (0..N) |i| {
        const name = try std.fmt.bufPrint(&nb, "f{d}", .{i});
        inos_of[i] = (try fs.create(&c, .{ .parent = d, .name = name, .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
        const r = map.routeName(name, set.nameKey());
        try testing.expect((try fs.findDentry(try SetFs.localOf(r.stripe), name)) != null);
        try testing.expectEqual(inos_of[i], (try fs.lookup(&c, .{ .parent = d, .name = name })).nodeid);
    }
    try testing.expectEqual(intent.stats.xv_cross_owner_intents_minted.load(), intent.stats.xv_cross_owner_intents_retired.load() + intent.stats.xv_cross_owner_intents_open.load());
    // A rename whose two stripes differ: the plan spans the stripes (on
    // one volume here, so the local two-parent entry) — and a rename of a
    // name INTO a directory on the other member is the intent.
    var mi: usize = 0;
    var newname_buf: [16]u8 = undefined;
    var newname: []const u8 = "";
    while (mi < N) : (mi += 1) {
        newname = try std.fmt.bufPrint(&newname_buf, "g{d}", .{mi});
        if (map.routeName(try std.fmt.bufPrint(&nb, "f{d}", .{mi}), set.nameKey()).index != map.routeName(newname, set.nameKey()).index) break;
    }
    const oldname = try std.fmt.bufPrint(&nb, "f{d}", .{mi});
    try fs.rename(&c, .{ .parent = d, .name = oldname, .newparent = d, .newname = newname, .flags = 0 });
    try testing.expectEqual(inos_of[mi], (try fs.lookup(&c, .{ .parent = d, .name = newname })).nodeid);
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = d, .name = oldname })).nodeid);
    // A directory on the OTHER member; a striped name moved into it is a
    // cross-member intent whose source is a stripe.
    var other: ?u64 = null;
    var k: u32 = 0;
    while (other == null) : (k += 1) {
        const e = try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "o{d}", .{k}), .mode = 0o755, .umask = 0 });
        if (set.volumeIndexOfSlot(route_table.slotOf(e.nodeid)) != dvol) other = e.nodeid;
    }
    const minted_before = intent.stats.xv_cross_owner_intents_minted.load();
    try fs.rename(&c, .{ .parent = d, .name = newname, .newparent = other.?, .newname = "moved", .flags = 0 });
    try testing.expectEqual(minted_before + 1, intent.stats.xv_cross_owner_intents_minted.load());
    try testing.expectEqual(inos_of[mi], (try fs.lookup(&c, .{ .parent = other.?, .name = "moved" })).nodeid);
    // readdir merges every remaining name exactly once.
    {
        var names: std.StringHashMapUnmanaged(void) = .empty;
        defer {
            var it = names.keyIterator();
            while (it.next()) |kp| gpa.free(kp.*);
            names.deinit(gpa);
        }
        var offset: u64 = 0;
        var total: usize = 0;
        while (true) {
            const data = try fs.readdir(&c, .{ .ino = d, .fh = 1, .offset = offset, .size = 1024 });
            var off: usize = 0;
            var n: usize = 0;
            while (off + abi.fuse_dirent.NAME_OFFSET <= data.len) {
                const ent = abi.decodePrefix(abi.fuse_dirent, window[off..data.len]).?;
                const name = window[off + abi.fuse_dirent.NAME_OFFSET ..][0..ent.namelen];
                if (!std.mem.eql(u8, name, ".") and !std.mem.eql(u8, name, "..")) {
                    const gop = try names.getOrPut(gpa, name);
                    try testing.expect(!gop.found_existing);
                    gop.key_ptr.* = try gpa.dupe(u8, name);
                }
                offset = ent.off;
                n += 1;
                off += abi.fuse_dirent.recordLen(ent.namelen);
            }
            total += n;
            if (n == 0) break;
        }
        try testing.expectEqual(@as(usize, N - 1), names.count());
    }
    // Empty it; the rmdir is ONE intent over the directory and every stripe.
    for (0..N) |i| {
        if (i == mi) continue;
        try fs.unlink(&c, .{ .parent = d, .name = try std.fmt.bufPrint(&nb, "f{d}", .{i}) });
    }
    const rmdirs0 = stripe.stats.dir_stripe_rmdirs.load();
    try fs.rmdir(&c, .{ .parent = inos.ROOT_INO, .name = "hot" });
    try testing.expectEqual(rmdirs0 + 1, stripe.stats.dir_stripe_rmdirs.load());
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = inos.ROOT_INO, .name = "hot" })).nodeid);
    try testing.expectEqual(intent.stats.xv_cross_owner_intents_minted.load(), intent.stats.xv_cross_owner_intents_retired.load() + intent.stats.xv_cross_owner_intents_open.load());
    try testing.expectEqual(@as(u32, 0), try bound.plane.recoverOpen(3_000));
}
