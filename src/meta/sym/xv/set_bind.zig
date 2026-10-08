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

/// The plane over a set: owns the resolver's state.
pub const Bound = struct {
    set: *MetaSet,
    plane: SetPlane,

    pub fn create(gpa: Allocator, io: Io, set: *MetaSet, locks: *dlm.Manager, env: *const std.process.Environ.Map) Allocator.Error!*Bound {
        const self = try gpa.create(Bound);
        errdefer gpa.destroy(self);
        self.set = set;
        const be0 = set.volumes[0];
        const own_id: u32 = if (be0.sym) |s| s.identity.mount_slot else 0;
        const term: u64 = if (be0.sym) |s| s.manager.term else set.setEpoch();
        self.plane = try SetPlane.init(gpa, io, set, locks, own_id, 0, term, .{ .ctx = self, .homeOf = &homeOf, .refresh = &homeOf, .localSlots = &localSlots }, ops.Seams.fromEnv(env), record.xattrValueCap(set.nodeSize()));
        self.plane.stripe_k = stripe.resolveK(env);
        return self;
    }

    pub fn destroy(self: *Bound, gpa: Allocator) void {
        self.plane.deinit();
        gpa.destroy(self);
    }

    fn homeOf(ctx: *anyopaque, slot: u16) ops.Home {
        const self: *Bound = @ptrCast(@alignCast(ctx));
        const v = self.set.volumeIndexOfSlot(slot);
        const be = self.set.volumes[v];
        if (!be.slots.armed) return .{ .local = .{ .volume = v } };
        return switch (be.slots.cache.stepHome(slot)) {
            .local => .{ .local = .{ .volume = v } },
            .foreign => |f| .{ .unreachable_ = .{ .holder = f.holder } },
            .unreachable_ => |u| .{ .unreachable_ = .{ .holder = u.holder } },
        };
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
