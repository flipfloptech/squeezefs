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

// ---------------------------------------------------------------------------
// THE CUSTODY OF A MOVED TREE'S IMAGES (charter §5.5 "Extent grants": the
// grant is a POOL, retired images recycle; 0003 §AD). Found by the shared
// hot directory row: B took `hot`'s slot by handover, its SMOs retired the
// images A had written the tree with, and B's grant mirror recycled them
// into B's pool — extents B's record never held. B's next claim of one was
// an `alloc` its recovery refused (`meta_kv_replay_extent_violations`); a
// surplus withdrawal of one returned an extent the record did not name
// (`extent_grant_closure_violations`, and the extent lost to every pool).
// ---------------------------------------------------------------------------

const cr = @import("control_records.zig");
const grant_record = @import("grant_record.zig");
const tree = @import("../tree.zig");
const smo = @import("../smo.zig");
const alloc_ext = @import("../alloc_ext.zig");

/// The manager's record of `appender_id`'s grant as runs (empty: none).
fn grantRecordOf(gpa: std.mem.Allocator, be: *KvBackend, appender_id: u32) !std.ArrayList(cr.GrantRun) {
    var runs: std.ArrayList(cr.GrantRun) = .empty;
    errdefer runs.deinit(gpa);
    const r = cr.extentGrantFamily(appender_id);
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    try be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs);
    var fam = try grant_record.Family.load(gpa, appender_id, kvs.items);
    defer fam.deinit();
    try runs.appendSlice(gpa, fam.record);
    return runs;
}

fn runsContain(runs: []const cr.GrantRun, e: u64) bool {
    for (runs) |r| if (r.contains(e)) return true;
    return false;
}

/// `n` records of `slot` committed by its lessee `be`, 64 to an entry.
fn fillSlot(be: *KvBackend, slot: u16, n: usize, value: []const u8) !void {
    var done: usize = 0;
    while (done < n) {
        const t = try be.begin(@src());
        var i: usize = 0;
        while (i < 64 and done < n) : ({
            i += 1;
            done += 1;
        }) {
            const local = try be.allocateInoInSlot(slot);
            try t.stagePut(key.TREE_INODES, &key.inodeKey(key.guestLocalIno(slot, local)), value);
        }
        try be.commit(t);
    }
}

test "linux: THE CUSTODY OF A MOVED TREE'S IMAGES (charter §5.5 'Extent grants', 0003 §AD; found by the shared hot directory row — a joiner's recovery refused an `alloc` outside its grant, a joiner's surplus return named an extent its grant did not hold): A grows a tree in a slot it leases; B takes the slot and its grant record gains every image of the tree in the grant's entry; B compacts one of A's leaves and its pool recycles the retired image — every extent B's pool can hand out (a claim, a surplus withdrawal) is one B's record grants; B leaves and its record names no image of the tree it released" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "image-custody", 1);
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
    // A's tree of the slot: leaves under an interior root (1,500 records
    // of 200 B over 64 KiB nodes), every image claimed from A's heap.
    try fillSlot(be0, slot, 1500, &@as([200]u8, @splat('a')));
    try be0.checkpointNow();
    var images_a: std.ArrayList(u64) = .empty;
    defer images_a.deinit(gpa);
    try be0.collectTreeExtents(be0.slotTree(slot).?, &images_a);
    try testing.expect(images_a.items.len >= 3);
    var me: u32 = undefined;
    {
        const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-image-custody-b", .endpoint = plane.advertised } });
        defer b.close();
        const bj = b.volumes[0];
        me = bj.joined.?.appenderId();
        try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
        const sink = bj.slots.carriageSink();
        sink.offered(sink.ctx, slot, lt.MANAGER);
        try testing.expect(bj.slots.table.view(slot).leasedBy(me));
        // The grant's entry moved the tree's images into B's record.
        {
            var rec = try grantRecordOf(gpa, be0, me);
            defer rec.deinit(gpa);
            for (images_a.items) |e| try testing.expect(runsContain(rec.items, e));
        }
        // B compacts one of A's leaves: A's image retires through B's SMO
        // context, the fresh image comes from B's pool.
        const t = bj.slotTree(slot).?;
        var tails: std.ArrayList(cr.TailEntry) = .empty;
        defer tails.deinit(gpa);
        try bj.collectLeafTails(t, &tails);
        const leaf = tails.items[0].leaf_addr;
        const retired = bj.cache.addrExtent(leaf);
        {
            bj.smo_lock.lockUncancelable(bj.io);
            defer bj.smo_lock.unlock(bj.io);
            const n = try t.getNode(leaf);
            defer n.release();
            var out: smo.MaintenanceOutcome = .{};
            try testing.expect(try smo.compactForDefrag(t, &bj.smo_ctx, n, &out));
        }
        // B's checkpoints cover the free: the retired image leaves the
        // coverage clock into B's pool.
        var cycles: u32 = 0;
        while (bj.alloc.pendingCount() != 0 and cycles < 4) : (cycles += 1) try bj.checkpointNow();
        try testing.expectEqual(@as(u64, 0), bj.alloc.pendingCount());
        try testing.expect(!bj.alloc.isAllocated(retired));
        // What recovery's screen and the closure judge: every extent B's
        // pool can hand out is one B's record grants.
        var rec = try grantRecordOf(gpa, be0, me);
        defer rec.deinit(gpa);
        var e: u64 = 0;
        while (e < bj.alloc.totalExtents()) : (e += 1) {
            if (bj.alloc.isAllocated(e)) continue;
            if (!runsContain(rec.items, e)) {
                std.debug.print("\nB's pool holds extent {d} (the retired image of A's leaf is {d}) — B's record does not grant it\n", .{ e, retired });
                return error.TestUnexpectedResult;
            }
        }
    }
    // B left (every slot released through its transfer): the tree is the
    // manager's again, and B's record names none of its images.
    var images_now: std.ArrayList(u64) = .empty;
    defer images_now.deinit(gpa);
    try be0.collectTreeExtents(be0.slotTree(slot).?, &images_now);
    var rec = try grantRecordOf(gpa, be0, me);
    defer rec.deinit(gpa);
    for (images_now.items) |e| try testing.expect(!runsContain(rec.items, e));
}

/// Counts the C6 bitmap pages that land (`backend.test_seams`).
const PageCount = struct {
    n: std.atomic.Value(u32) = .init(0),

    fn hook(self: *PageCount) @import("../node_cache.zig").Hook {
        return .{ .ctx = self, .call = call };
    }

    fn call(ctx: *anyopaque) void {
        const self: *PageCount = @ptrCast(@alignCast(ctx));
        _ = self.n.fetchAdd(1, .acq_rel);
    }
};

test "linux: THE MANAGER'S KILL WITH A LIVE LESSEE (charter §5.5 'Extent grants', 0003 §U.1): A mints a slot tree (its root one image claimed in A's RAM), B takes the slot — the handover's cycle lands A's claim on A's bitmap BEFORE the grant's entry moves the image into B's record — and compacts the root (the image retires into B's pool); A is killed: the reopened manager holds every extent B's record names allocated (its claims never hand out B's pooled image), and its mount's reachability walk never visits B's tree (a live lessee's projection: its interior may name extents the lessee recycled)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "custody-cut", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var cut_buf: [192]u8 = undefined;
    const cut = try std.fmt.bufPrintSentinel(&cut_buf, "{s}.cut", .{s.paths[0]}, 0);
    defer _ = std.os.linux.unlink(cut.ptr);
    var pages: PageCount = .{};
    defer backend.test_seams = .{};
    var moved: u64 = undefined;
    var me: u32 = undefined;
    var slot_of_b: u16 = undefined;
    {
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
        slot_of_b = slot;
        const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-custody-cut-b", .endpoint = plane.advertised } });
        defer b.close();
        const bj = b.volumes[0];
        me = bj.joined.?.appenderId();
        try be0.checkpointNow();
        backend.test_seams.after_bitmap_page = pages.hook();
        try fillSlot(be0, slot, 1, &@as([200]u8, @splat('a')));
        var images_a: std.ArrayList(u64) = .empty;
        defer images_a.deinit(gpa);
        try be0.collectTreeExtents(be0.slotTree(slot).?, &images_a);
        try testing.expectEqual(@as(usize, 1), images_a.items.len);
        moved = images_a.items[0];
        try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
        const sink = bj.slots.carriageSink();
        sink.offered(sink.ctx, slot, lt.MANAGER);
        try testing.expect(bj.slots.table.view(slot).leasedBy(me));
        {
            var rec = try grantRecordOf(gpa, be0, me);
            defer rec.deinit(gpa);
            try testing.expect(runsContain(rec.items, moved));
        }
        // The handover's cycle (`cycleOnce`) landed A's claim first.
        try testing.expect(pages.n.load(.acquire) >= 1);
        // B compacts the root: A's image retires through B's SMO context and
        // B's checkpoints cover it into B's pool.
        const t = bj.slotTree(slot).?;
        {
            bj.smo_lock.lockUncancelable(bj.io);
            defer bj.smo_lock.unlock(bj.io);
            const n = try t.getNode(t.rootPtr().addr);
            defer n.release();
            var out: smo.MaintenanceOutcome = .{};
            try testing.expect(try smo.compactForDefrag(t, &bj.smo_ctx, n, &out));
        }
        var cycles: u32 = 0;
        while (bj.alloc.pendingCount() != 0 and cycles < 4) : (cycles += 1) try bj.checkpointNow();
        try testing.expect(!bj.alloc.isAllocated(moved));
        // The kill of A: its device as it stands.
        try backend.snapshotImage(s.paths[0], cut);
    }
    const copy = try KvBackend.open(gpa, s.io(), cut, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
    defer copy.close();
    var rec = try grantRecordOf(gpa, copy, me);
    defer rec.deinit(gpa);
    try testing.expect(runsContain(rec.items, moved));
    var free_named: usize = 0;
    for (rec.items) |r| {
        var e = r.start;
        while (e < r.start + r.len) : (e += 1) free_named += @intFromBool(!copy.alloc.isAllocated(e));
    }
    try testing.expectEqual(@as(usize, 0), free_named);
    // The walk's extents: every tree but B's projection.
    var maintained: u64 = 0;
    var extents: std.ArrayList(u64) = .empty;
    defer extents.deinit(gpa);
    for (copy.trees.load()) |tr| {
        if (tr == copy.slotTree(slot_of_b).?) continue;
        extents.clearRetainingCapacity();
        try copy.collectTreeExtents(tr, &extents);
        maintained += extents.items.len;
    }
    const walked = alloc_ext.meta_kv_mount_reach_walk_extents.load();
    std.debug.print("\nmanager kill with a live lessee: B's pooled image {d} allocated {}; the mount walked {d} extent(s), the maintained trees hold {d}\n", .{ moved, copy.alloc.isAllocated(moved), walked, maintained });
    try testing.expectEqual(maintained, walked);
}

// ---------------------------------------------------------------------------
// THE MOVE MATRIX (0003 §X.1, §X.2): the filesystem over both members — A's
// and B's planes bound the way `mount` binds them, B's behind the product's
// move gate — and a directory of A's moving to B by a holder-decided
// handover held at one step while B runs one op of each kind into it.
// Every op answers what ONE successful call answers, and both members read
// the same tree afterwards.
// ---------------------------------------------------------------------------

const kv_fs = @import("../../fuse/handler/kv_fs.zig");
const filesystem = @import("../../fuse/handler/filesystem.zig");
const op_trace = @import("../../fuse/handler/op_trace.zig");
const move_gate = @import("../../fuse/handler/move_gate.zig");
const moves = @import("slots/moves.zig");
const door = @import("slots/door.zig");
const inos = @import("../inos.zig");
const route_table = @import("../routing/route_table.zig");
const clock = @import("../../core/clock.zig");

const SetFs = kv_fs.KvFs(MetaSet, kv_fs.NoData);

/// One member's filesystem with its cross-owner plane and wire plane bound
/// in `mount`'s order.
const Member = struct {
    nodata: kv_fs.NoData = .{},
    fs: SetFs,
    bound: *xv.set_bind.Bound,
    plane: *wire_plane.WirePlane,

    fn arm(self: *Member, gpa: std.mem.Allocator, io: std.Io, set: *MetaSet, env: *std.process.Environ.Map) !void {
        self.nodata = .{};
        self.fs = try SetFs.mount(gpa, io, set, &self.nodata, .{ .dlm_stripes = 64 });
        errdefer self.fs.unmount();
        self.bound = try xv.set_bind.Bound.create(gpa, io, set, &self.fs.locks, env);
        errdefer self.bound.destroy(gpa);
        _ = try self.fs.bindXv(&self.bound.plane);
        self.plane = (try wire_plane.WirePlane.start(gpa, io, set, env, 4, 1 << 30, &self.bound.plane, false)).?;
        self.fs.bindDivert(&self.plane.divert);
        self.bound.bindDialer(self.plane.peers.xvDialer());
    }

    fn disarm(self: *Member, gpa: std.mem.Allocator) void {
        self.plane.stop();
        self.bound.destroy(gpa);
        self.fs.unmount();
    }

    fn face(self: *Member) filesystem.Filesystem {
        return filesystem.Filesystem.of(SetFs, &self.fs);
    }
};

/// The handover step B's ops meet.
const Step = enum {
    /// A's offer pushed, the reservation's stand raised for B; A still holds.
    offered,
    /// A's transfer parked past step 0's drain, the door's gate and the
    /// page `Releasing` (`park_after_page`): A's door closed on the slot.
    drained,
    /// A released and granted; B's install parked between its tree and its
    /// lease word (`park_mid_install`).
    granted,
    /// The move done: B's door open on the slot.
    reopened,
};

const Kind = enum { create, chmod, unlink, rename, setattr };

/// The `setattr` op's times (whole seconds: the kernel's attr words).
const TIMES_S: u64 = 1_234_567;

const Op = struct {
    kind: Kind,
    fs: filesystem.Filesystem,
    io: std.Io,
    dir: u64,
    target: u64 = 0,
    got: u64 = 0,
    err: ?anyerror = null,
    done: *std.atomic.Value(u32),

    fn run(op: *Op) void {
        // Paired with the release-hold loop's acquire load.
        defer _ = op.done.fetchAdd(1, .release);
        var tr = op_trace.OpTrace.begin(0, .other);
        defer tr.finish();
        var window: [4096]u8 = undefined;
        var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = op.io, .trace = &tr, .wd_slot = null, .window = &window };
        op.got = op.call(&c) catch |e| {
            op.err = e;
            return;
        };
    }

    fn call(op: *Op, c: *filesystem.Ctx) !u64 {
        const v = op.fs.vtable;
        const impl = op.fs.impl;
        switch (op.kind) {
            .create => return (try v.create(impl, c, .{ .parent = op.dir, .name = "made", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid,
            .chmod => _ = try v.setattr(impl, c, .{ .ino = op.target, .attr = .{ .mode = 0o600 } }),
            .unlink => try v.unlink(impl, c, .{ .parent = op.dir, .name = "f.unlink" }),
            .rename => try v.rename(impl, c, .{ .parent = op.dir, .name = "f.rename", .newparent = op.dir, .newname = "f.renamed", .flags = 0 }),
            .setattr => _ = try v.setattr(impl, c, .{ .ino = op.target, .attr = .{ .atime = TIMES_S * std.time.ns_per_s, .mtime = TIMES_S * std.time.ns_per_s } }),
        }
        return op.target;
    }
};

fn testCtx(io: std.Io, tr: *op_trace.OpTrace, window: []u8) filesystem.Ctx {
    return .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = tr, .wd_slot = null, .window = window };
}

fn lookupIno(f: filesystem.Filesystem, c: *filesystem.Ctx, parent: u64, name: []const u8) !u64 {
    return (try f.vtable.lookup(f.impl, c, .{ .parent = parent, .name = name })).nodeid;
}

/// The ops' "in the window" verdict: every op finished, parked at A's door
/// (a served commit meeting A's own release), parked on B's move epoch, or
/// waiting behind B's held install (its resolver's replayed first touch of
/// the same grant). Bounded: past the bound the step answers the miss (a
/// pin whose window was never met proves nothing).
const REACH_BOUND_NS: u64 = 30 * std.time.ns_per_s;

const Marks = struct {
    door_parks: u64,
    gate_parks: u64,
    install_waits: u64,

    fn now() Marks {
        return .{ .door_parks = door.stats.slot_door_parks.load(), .gate_parks = move_gate.stats.slot_move_parks.load(), .install_waits = acquire.stats.slot_install_waits.load() };
    }

    fn since(m: Marks) u64 {
        const n = now();
        return (n.door_parks - m.door_parks) + (n.gate_parks - m.gate_parks) + (n.install_waits - m.install_waits);
    }
};

fn awaitInWindow(n: u32, done: *std.atomic.Value(u32), marks: Marks) bool {
    const until = clock.monotonicNs() + REACH_BOUND_NS;
    while (clock.monotonicNs() < until) {
        if (done.load(.acquire) + marks.since() >= n) return true;
        std.Thread.yield() catch {};
    }
    return false;
}

fn moveOne(comptime step: Step, comptime kind: Kind) !void {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "move-matrix-" ++ @tagName(step) ++ "-" ++ @tagName(kind), 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    const io = s.io();
    const set = try s.open();
    defer set.close();
    var a: Member = undefined;
    try a.arm(gpa, io, set, &s.env);
    defer a.disarm(gpa);
    const be0 = set.volumes[0];
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c = testCtx(io, &tr, &window);
    // A's directory in one of A's rotor slots, its files beside it.
    var nb: [16]u8 = undefined;
    var dir: u64 = 0;
    var dir_idx: usize = 0;
    var slot: u16 = 0;
    for (0..64) |i| {
        const e = try a.fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = try std.fmt.bufPrint(&nb, "d{d}", .{i}), .mode = 0o755, .umask = 0 });
        const sl = route_table.slotOf(e.nodeid);
        if (sl != be0.stamp.native_slot and be0.slots.table.view(sl).leasedBy(lt.MANAGER)) {
            dir = e.nodeid;
            dir_idx = i;
            slot = sl;
            break;
        }
    } else return error.NoRotorDirectory;
    var files: [4]u64 = undefined;
    for (&files, [_][]const u8{ "f.chmod", "f.unlink", "f.rename", "f.times" }) |*f, name| {
        f.* = (try a.fs.create(&c, .{ .parent = dir, .name = name, .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
    }
    try be0.checkpointNow();
    // B joins with its filesystem behind the product's move gate.
    const b = try MetaSet.open(gpa, io, s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-move-matrix-b", .endpoint = a.plane.advertised } });
    defer b.close();
    var mv = moves.Moves.init(io);
    b.bindMoves(&mv);
    defer mv.close();
    var bm: Member = undefined;
    try bm.arm(gpa, io, b, &s.env);
    defer bm.disarm(gpa);
    var gate: move_gate.MoveGate = .{ .inner = bm.face(), .moves = &mv, .deadline_ns = moves.deadlineNs(bm.plane.moveDeadlineMs()) };
    const bfs = gate.filesystem();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();
    try testing.expectEqual(dir, try lookupIno(bfs, &c, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "d{d}", .{dir_idx})));
    var done = std.atomic.Value(u32).init(0);
    var op: Op = .{ .kind = kind, .fs = bfs, .io = io, .dir = dir, .done = &done, .target = switch (kind) {
        .create => 0,
        .chmod => files[0],
        .unlink => files[1],
        .rename => files[2],
        .setattr => files[3],
    } };
    const Accept = struct {
        fn run(p: anytype, sl: u16) void {
            const sink = p.carriageSink();
            sink.offered(sink.ctx, sl, lt.MANAGER);
        }
    };
    // The step's window, the op issued inside it.
    var reached = true;
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
    switch (step) {
        .offered => {
            op.run();
            Accept.run(bj.slots, slot);
        },
        .drained, .granted => {
            const seam = if (step == .drained) &be0.slots.seam else &bj.slots.seam;
            seam.hold = if (step == .drained) .park_after_page else .park_mid_install;
            const accept = try std.Thread.spawn(.{}, Accept.run, .{ bj.slots, slot });
            seam.reached.waitUncancelable(io);
            const marks = Marks.now();
            const t = try std.Thread.spawn(.{}, Op.run, .{&op});
            reached = awaitInWindow(1, &done, marks);
            seam.hold = .off;
            seam.release.set(io);
            accept.join();
            t.join();
        },
        .reopened => {
            Accept.run(bj.slots, slot);
            op.run();
        },
    }
    try testing.expect(reached);
    if (op.err) |e| {
        std.debug.print("move matrix {t}: {t} answered {t}\n", .{ step, kind, e });
        return e;
    }
    // The move landed: the slot is B's on both tables.
    try testing.expect(bj.slots.table.view(slot).leasedBy(me));
    try testing.expect(be0.slots.table.view(slot).leasedBy(me));
    // Both members read what one successful call leaves.
    for ([_]filesystem.Filesystem{ a.face(), bfs }) |f| switch (kind) {
        .create => {
            try testing.expect(op.got != 0);
            try testing.expectEqual(op.got, try lookupIno(f, &c, dir, "made"));
        },
        .chmod => try testing.expectEqual(@as(u32, 0o600), (try f.vtable.getattr(f.impl, &c, .{ .ino = files[0], .fh = null })).attr.mode & 0o7777),
        .unlink => {
            try testing.expectEqual(@as(u64, 0), try lookupIno(f, &c, dir, "f.unlink"));
            try testing.expectEqual(@as(u32, 0), (try f.vtable.getattr(f.impl, &c, .{ .ino = files[1], .fh = null })).attr.nlink);
        },
        .rename => {
            try testing.expectEqual(@as(u64, 0), try lookupIno(f, &c, dir, "f.rename"));
            try testing.expectEqual(files[2], try lookupIno(f, &c, dir, "f.renamed"));
        },
        .setattr => {
            const tm = try f.vtable.getattr(f.impl, &c, .{ .ino = files[3], .fh = null });
            try testing.expectEqual(TIMES_S, tm.attr.mtime);
            try testing.expectEqual(TIMES_S, tm.attr.atime);
        },
    };
}

fn moveMatrix(comptime step: Step) !void {
    inline for (comptime std.enums.values(Kind)) |kind| try moveOne(step, kind);
}

test "linux: THE MOVE MATRIX — offer pushed, stand raised (0003 §X.1): B's create / chmod / unlink / rename / setattr into A's directory while A's offer of its slot to B stands each answer one successful call's result; the accept then moves the slot and both members read the same tree" {
    try moveMatrix(.offered);
}

test "linux: THE MOVE MATRIX — step 0 drained, A's door closed (0003 §X.1, §X.2): B's five ops issued while A's transfer of the slot to B is parked past its drain, its gate and its page each answer one successful call's result — the system parks and re-issues every refusal the move answers, never EAGAIN, a create never EEXIST against its own first attempt" {
    try moveMatrix(.drained);
}

test "linux: THE MOVE MATRIX — grant landed, B's install mid-way (0003 §X.1, §X.4): B's five ops issued while B's install of the grant is parked between its tree and its lease word each answer one successful call's result" {
    try moveMatrix(.granted);
}

test "linux: THE MOVE MATRIX — door re-opened (0003 §X.1): B's five ops into the directory it now holds land locally, each one successful call's result, and A reads them through B" {
    try moveMatrix(.reopened);
}
