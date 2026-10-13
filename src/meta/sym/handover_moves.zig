//! The slot MOVE contracts over two real members in one process (Linux;
//! decision 0003 §X): A — the manager, its set and its wire plane over a
//! file-backed symmetric volume under /build/tmp — and B, a joined member
//! dialled over real TCP, each with its own lease table, door and slot
//! plane. Every contract here holds a handover at one of its steps (the
//! plane's `SQUEEZEFS_TEST_HANDOVER_HOLD` seam, set on the plane in
//! process) while the other member touches the moving slot, and judges
//! what that touch may see. Test-only.

const std = @import("std");
const builtin = @import("builtin");
const testing = std.testing;
const key = @import("../key.zig");
const dlm = @import("../dlm.zig");
const meta_set = @import("../meta_set.zig");
const backend = @import("../backend.zig");
const xv = @import("xv.zig");
const wire_plane = @import("wire_plane.zig");
const acquire = @import("slots/acquire.zig");
const lt = @import("slots/lease_table.zig");
const log = @import("../../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/handover_moves is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const MetaSet = meta_set.MetaSet;
const KvBackend = backend.KvBackend;

/// A rotor slot the manager holds (never its native slot).
fn managerRotorSlot(gpa: std.mem.Allocator, be: *KvBackend) !u16 {
    var held: std.ArrayList(u16) = .empty;
    defer held.deinit(gpa);
    try be.slots.table.heldBy(lt.MANAGER, &held);
    for (held.items) |s| if (be.stamp.native_slot != s) return s;
    return error.NoRotorSlot;
}

/// One record of `slot` committed by `be` (its lessee) under a fresh key.
fn commitRecord(be: *KvBackend, slot: u16, value: []const u8) ![key.INODE_KEY_LEN]u8 {
    const local = try be.allocateInoInSlot(slot);
    const k = key.inodeKey(key.guestLocalIno(slot, local));
    const t = try be.begin(@src());
    try t.stagePut(key.TREE_INODES, &k, value);
    try be.commit(t);
    return k;
}

test "linux: THE ACCEPTER'S INSTALL OF A WIRE GRANT (0003 §X.5; found by the online fsck row — B's create into the moved directory read its parent as absent, `dying parent` → EEXIST, ENOENT, and a minted empty tree shadowing the transferred root): a commit at B into a slot A leases mints no tree at B (the door refuses it first); while B's accept is held between the two halves of its install, B's table never names B for a slot whose transferred tree is not yet in place; a stale tree B held for the slot is replaced by the grant's root; after the install every record A wrote is B's to read" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "accept-install", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    const plane = (try wire_plane.WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
    defer plane.stop();
    const be0 = set.volumes[0];
    // A writes a record into one of its rotor slots and checkpoints: tree
    // 0's projection names A's lease, the tree is on the device.
    const slot = try managerRotorSlot(gpa, be0);
    const k = try commitRecord(be0, slot, "written by A");
    try be0.checkpointNow();
    // B joins (another process's shape: its own set over the same volume).
    const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-accept-install-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();
    try testing.expect(bj.slots.table.view(slot).leasedBy(lt.MANAGER));
    // A commit at B staging a record of A's slot: refused at B's door, and
    // no tree of the slot minted at B on the way (it would shadow the root
    // a later grant transfers).
    {
        const t = try b.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(key.guestLocalIno(slot, 1 << 20)), "B's stray");
        try testing.expectError(error.SlotBusy, b.commit(t));
        try testing.expect(bj.slotTree(slot) == null);
    }
    // A stale tree of the slot at B (an empty one: what that mint made) —
    // the install replaces it, loud.
    _ = try bj.mintGuestTree(slot);
    acquire.stats.slot_install_shadow_trees.resetLogBudgetForTest();
    const shadows0 = acquire.stats.slot_install_shadow_trees.load();
    // A offers the slot to B; B accepts, its install held mid-way.
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
    bj.slots.seam.hold = .park_mid_install;
    const Accept = struct {
        /// The accept runs on B's carriage thread; the shadow's loud
        /// tripwire is that thread's one expected error log.
        fn run(p: anytype, sl: u16, fired: *bool) void {
            log.testing.expectErrorLogs(1);
            const sink = p.carriageSink();
            sink.offered(sink.ctx, sl, lt.MANAGER);
            fired.* = log.testing.capturedErrorLogs() == 1;
            log.testing.expectErrorLogs(0);
        }
    };
    var fired = false;
    const th = try std.Thread.spawn(.{}, Accept.run, .{ bj.slots, slot, &fired });
    bj.slots.seam.reached.waitUncancelable(s.io());
    {
        // Held between the halves: the transferred tree is in place (the
        // record A wrote reads through B's own tree of the slot) and B's
        // word does not name B yet — every reader still routes to A.
        const named = bj.slots.table.view(slot).leasedBy(me);
        const seen = try b.lookup(gpa, key.TREE_INODES, &k);
        defer if (seen) |v| gpa.free(v);
        const visible = if (seen) |v| std.mem.eql(u8, v, "written by A") else false;
        try testing.expect(!named);
        try testing.expect(visible);
    }
    bj.slots.seam.release.set(s.io());
    th.join();
    try testing.expect(fired);
    try testing.expectEqual(shadows0 + 1, acquire.stats.slot_install_shadow_trees.load());
    try testing.expect(bj.slots.table.view(slot).leasedBy(me));
    try testing.expect(be0.slots.table.view(slot).leasedBy(me));
    const after = (try b.lookup(gpa, key.TREE_INODES, &k)) orelse return error.TestRecordLost;
    defer gpa.free(after);
    try testing.expectEqualStrings("written by A", after);
}

test "linux: THE CACHE BARRIER AT A LEASE MOVE (0003 §X.5; found by the online fsck row — every name B created in the directory it took by handover was gone after B's unmount: a wire lessee appends frames into its leaves IN PLACE under an unchanged root pointer, and the manager re-adopting the released slot opened it off the leaf image it had cached before the grant): B takes a slot from A, writes into it and checkpoints, leaves cleanly (the slot back to A); A reads every record B wrote, and A's later write lands beside them" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "move-cache", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    const plane = (try wire_plane.WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
    defer plane.stop();
    const be0 = set.volumes[0];
    const slot = try managerRotorSlot(gpa, be0);
    const k_a = try commitRecord(be0, slot, "written by A");
    try be0.checkpointNow();
    // A's cache holds the slot's leaf image from here on.
    {
        const v = (try set.lookup(gpa, key.TREE_INODES, &k_a)).?;
        gpa.free(v);
    }
    var k_b: [key.INODE_KEY_LEN]u8 = undefined;
    {
        const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-move-cache-b", .endpoint = plane.advertised } });
        defer b.close();
        const bj = b.volumes[0];
        const me = bj.joined.?.appenderId();
        try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
        const sink = bj.slots.carriageSink();
        sink.offered(sink.ctx, slot, lt.MANAGER);
        try testing.expect(bj.slots.table.view(slot).leasedBy(me));
        // B's record lands in the transferred tree and B's checkpoint folds
        // it into the leaf (in place: the root pointer stays the one A
        // released).
        k_b = try commitRecord(bj, slot, "written by B");
        try bj.checkpointNow();
    }
    // B left cleanly: the slot is the manager's again, its tree adopted at
    // the root B released.
    try testing.expect(be0.slots.table.view(slot).state == .unleased);
    const seen_b = (try set.lookup(gpa, key.TREE_INODES, &k_b)) orelse return error.TestRecordLost;
    defer gpa.free(seen_b);
    try testing.expectEqualStrings("written by B", seen_b);
    const seen_a = (try set.lookup(gpa, key.TREE_INODES, &k_a)).?;
    defer gpa.free(seen_a);
    try testing.expectEqualStrings("written by A", seen_a);
    // A writes the slot again (its first touch takes the adopted tree):
    // its record lands beside B's, never over them.
    const k_a2 = try commitRecord(be0, slot, "A again");
    try be0.checkpointNow();
    for ([_]struct { k: [key.INODE_KEY_LEN]u8, v: []const u8 }{ .{ .k = k_a, .v = "written by A" }, .{ .k = k_b, .v = "written by B" }, .{ .k = k_a2, .v = "A again" } }) |want| {
        const got = (try set.lookup(gpa, key.TREE_INODES, &want.k)) orelse return error.TestRecordLost;
        defer gpa.free(got);
        try testing.expectEqualStrings(want.v, got);
    }
}
