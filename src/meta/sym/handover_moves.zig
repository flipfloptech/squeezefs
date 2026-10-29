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
const slots_mod = @import("slots.zig");
const slots_plane = @import("slots/plane.zig");
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

/// The leases a plane's own regions hold, off its table.
fn ownLeases(p: *slots_plane.Plane) u64 {
    var held: u64 = 0;
    for (p.own_ids[0..p.own_n.load(.acquire)]) |*o| held += p.table.heldCount(o.load(.acquire));
    return held;
}

/// The process-wide `slot_leases_held` against what the live planes'
/// own regions hold: the gauge's move since `g0` IS their sum.
fn expectLeaseGauge(g0: u64, planes: []const *slots_plane.Plane) !void {
    var held: u64 = 0;
    for (planes) |p| held += ownLeases(p);
    const now = slots_mod.stats.slot_leases_held.load();
    try testing.expect(now >= g0);
    try testing.expectEqual(held, now - g0);
}

test "linux: THE LEASE GAUGE IS ONE EXACT SUM (rung-10-gate-blockers-2 §7 — `slot_leases_held` read 18,446,744,073,709,551,452, −164 wrapped, on A after the rejoin row: the arm added the table's whole count, a wire grant added nothing at the manager, every release subtracted one, a joiner's close subtracted the foreign views it had loaded): after A's arm, B's join, B's accept of A's slot, B's release of every lease, B's close and B's re-join under the same mountpoint, the gauge moved by exactly the leases the live planes' own regions hold, never below where it started" {
    const gpa = testing.allocator;
    const g0 = slots_mod.stats.slot_leases_held.load();
    {
        const s = try meta_set.TestSet.create(gpa, "lease-gauge", 1);
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
        try expectLeaseGauge(g0, &.{be0.slots});
        const slot = try managerRotorSlot(gpa, be0);
        const join: meta_set.OpenOptions = .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-lease-gauge-b", .endpoint = plane.advertised } };
        {
            const b = try MetaSet.open(gpa, s.io(), s.pathList(), join);
            defer b.close();
            const bj = b.volumes[0];
            const me = bj.joined.?.appenderId();
            try expectLeaseGauge(g0, &.{ be0.slots, bj.slots });
            // A's slot moves to B: A's share falls by one, B's rises by one.
            try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
            const sink = bj.slots.carriageSink();
            sink.offered(sink.ctx, slot, lt.MANAGER);
            try testing.expect(be0.slots.table.view(slot).leasedBy(me));
            try expectLeaseGauge(g0, &.{ be0.slots, bj.slots });
            // B returns every lease it holds: A's share is unchanged by a
            // wire lessee's release.
            bj.slots.releaseAllForLeave(me);
            try testing.expectEqual(@as(u64, 0), ownLeases(bj.slots));
            try expectLeaseGauge(g0, &.{ be0.slots, bj.slots });
        }
        // B's close takes out its own share only (its table's views of A's
        // leases were never in it).
        try expectLeaseGauge(g0, &.{be0.slots});
        {
            // The re-join re-adopts under the same mountpoint's id.
            const b = try MetaSet.open(gpa, s.io(), s.pathList(), join);
            defer b.close();
            try expectLeaseGauge(g0, &.{ be0.slots, b.volumes[0].slots });
        }
        try expectLeaseGauge(g0, &.{be0.slots});
    }
    try testing.expectEqual(g0, slots_mod.stats.slot_leases_held.load());
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
    // The walk's extents: every tree the reopened manager holds — none of
    // B's (decision 0003 §CF.1).
    try testing.expect(copy.slotTree(slot_of_b) == null);
    var maintained: u64 = 0;
    var extents: std.ArrayList(u64) = .empty;
    defer extents.deinit(gpa);
    for (copy.trees.load()) |tr| {
        extents.clearRetainingCapacity();
        try copy.collectTreeExtents(tr, &extents);
        maintained += extents.items.len;
    }
    const walked = alloc_ext.meta_kv_mount_reach_walk_extents.load();
    std.debug.print("\nmanager kill with a live lessee: B's pooled image {d} allocated {}; the mount walked {d} extent(s), the maintained trees hold {d}\n", .{ moved, copy.alloc.isAllocated(moved), walked, maintained });
    try testing.expectEqual(maintained, walked);
}

/// B compacts the root of its tree of `slot` (the root's image retires
/// through B's SMO context), cycles once — serialized behind B's own
/// cadence on the SMO lock, so the page naming the moved root is on the
/// device when this returns — and on until the free is covered into B's
/// pool.
fn compactRootAndCover(bj: *KvBackend, slot: u16) !void {
    const t = bj.slotTree(slot).?;
    {
        bj.smo_lock.lockUncancelable(bj.io);
        defer bj.smo_lock.unlock(bj.io);
        const n = try t.getNode(t.rootPtr().addr);
        defer n.release();
        var out: smo.MaintenanceOutcome = .{};
        try testing.expect(try smo.compactForDefrag(t, &bj.smo_ctx, n, &out));
    }
    try bj.checkpointNow();
    var cycles: u32 = 0;
    while (bj.alloc.pendingCount() != 0 and cycles < 4) : (cycles += 1) try bj.checkpointNow();
    try testing.expectEqual(@as(u64, 0), bj.alloc.pendingCount());
}

/// The root tree 0's `slot_state` record of `slot` names on `be`.
fn tree0Root(gpa: std.mem.Allocator, be: *KvBackend, slot: u16) !tree.RootPtr {
    const v = (try be.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(slot).slice())) orelse return error.TestSlotUnrecorded;
    defer gpa.free(v);
    const st = try cr.SlotStateWire.decode(v);
    return .{ .addr = st.root.addr, .seq = st.root.seq };
}

/// The `slot_state` record of `slot` as a READER's projection of tree 0
/// holds it (the checkpointed leaf image, no ring).
fn projectedSlotState(gpa: std.mem.Allocator, reader: *MetaSet, slot: u16) !cr.SlotState {
    const v = (try reader.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(slot).slice())) orelse return error.TestSlotUnrecorded;
    defer gpa.free(v);
    return try cr.SlotStateWire.decode(v);
}

test "linux: A LESSEE'S ROOT HAS TWO HOMES AND A READER FOLLOWS THE LIVE ONE (charter §5.5, spec §1.2 'Two durable homes at every instant'; found by the online fsck row — `fsck`'s storage-trust open of the two-daemon set refused 'the projection's roots stayed stale through 64 re-points'): A mints a slot tree; B takes it by handover and compacts the root — tree 0 keeps the grant-time root (whether or not A's checkpoint has landed the grant yet), B's page names the live one; a read-only open of the set opens B's slot at the root B's PAGE names; after B's pool recycles the grant-time image under a fresh node the open still succeeds (no record of A's ever renames a home B left, so no re-point could have waited it out)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "reader-lessee-home", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    // No cycle by age during the row: tree 0's device image moves only
    // with the handover's own cycles and the test's `checkpointNow`.
    try s.env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "60000");
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
    try fillSlot(be0, slot, 1, &@as([200]u8, @splat('a')));
    try be0.checkpointNow();
    const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-reader-lessee-home-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
    const sink = bj.slots.carriageSink();
    sink.offered(sink.ctx, slot, lt.MANAGER);
    try testing.expect(bj.slots.table.view(slot).leasedBy(me));
    // B's first cycle: its page names the slot (page-homed from here on —
    // the overflow law's tree-0 publication runs only for a slot the page
    // does not name). Tree 0's words are the grant-time root, the root
    // B's tree stands at before its first SMO.
    try bj.checkpointNow();
    const grant_time = try tree0Root(gpa, be0, slot);
    try testing.expectEqual(grant_time, bj.slotTree(slot).?.rootPtr());
    const g_b = bj.slots.table.view(slot).g;
    // B's root moves: the grant-time image retires into B's pool (its
    // bytes intact), B's cycle refreshes its page with the live root;
    // tree 0 keeps the grant-time words.
    try compactRootAndCover(bj, slot);
    const live1 = bj.slotTree(slot).?.rootPtr();
    try testing.expect(live1.addr != grant_time.addr);
    try testing.expect(!bj.alloc.isAllocated(bj.cache.addrExtent(grant_time.addr)));
    try testing.expectEqual(grant_time, try tree0Root(gpa, be0, slot));
    const ro: meta_set.OpenOptions = .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .read_only = true };
    // 0. Before A's next checkpoint: whether the reader's tree 0 already
    //    carries the grant (the handover's trailing cycle landed it) or
    //    still predates it (the entry sits in A's ring: B's page Live at a
    //    generation above the projection's is the slot's only durable home
    //    — the lag the `projected` fold adopts through), the root the
    //    reader opens is the one B's page names.
    {
        const reader = try MetaSet.open(gpa, s.io(), s.pathList(), ro);
        defer reader.close();
        const seen = try projectedSlotState(gpa, reader, slot);
        try testing.expect(seen.g <= g_b);
        const named = reader.volumes[0].guest_published.get(slot) orelse return error.TestSlotNotProjected;
        std.debug.print("\nreader's root of B's slot {d} with tree 0 at g {d} (B's lease g {d}): 0x{x} seq {d}; B's live root 0x{x} seq {d}; the grant-time root 0x{x} seq {d}\n", .{ slot, seen.g, g_b, named.addr, named.seq, live1.addr, live1.seq, grant_time.addr, grant_time.seq });
        try testing.expectEqual(live1, named);
    }
    // 1. A's checkpoint has landed the grant: tree 0 names B at the
    //    grant-time words, B's page the live root — the reader opens B's.
    try be0.checkpointNow();
    {
        const reader = try MetaSet.open(gpa, s.io(), s.pathList(), ro);
        defer reader.close();
        const seen = try projectedSlotState(gpa, reader, slot);
        try testing.expect(seen.variant == .leased and seen.appender_id == me and seen.g == g_b);
        try testing.expectEqual(grant_time, tree.RootPtr{ .addr = seen.root.addr, .seq = seen.root.seq });
        try testing.expectEqual(live1, reader.volumes[0].guest_published.get(slot) orelse return error.TestSlotNotProjected);
    }
    // 2. The grant-time image recycled: B's next root claims the lowest
    //    free extent of its pool — the image A's root lived in — so tree
    //    0's word now names a node under another seq.
    var rounds: u32 = 0;
    while (!bj.alloc.isAllocated(bj.cache.addrExtent(grant_time.addr)) and rounds < 8) : (rounds += 1) try compactRootAndCover(bj, slot);
    try testing.expect(bj.alloc.isAllocated(bj.cache.addrExtent(grant_time.addr)));
    const live2 = bj.slotTree(slot).?.rootPtr();
    try testing.expect(live2.addr != grant_time.addr or live2.seq != grant_time.seq);
    {
        const reader = MetaSet.open(gpa, s.io(), s.pathList(), ro) catch |e| {
            std.debug.print("\nread-only open refused {s} after B recycled the grant-time image ({d} compaction round(s)); B's live root 0x{x} seq {d}\n", .{ @errorName(e), rounds, live2.addr, live2.seq });
            return e;
        };
        defer reader.close();
        try testing.expectEqual(live2, reader.volumes[0].guest_published.get(slot) orelse return error.TestSlotNotProjected);
    }
    // 3. The race inside ONE open: the reader has read B's page (naming
    //    live2) and, before it opens the roots, B moves its root again and
    //    its pool recycles live2's image under the new root. The lessee is
    //    live and moving: its slot is left un-projected (a reader reads it
    //    under a token from B), the open succeeds, nothing re-points.
    const MoveOn = struct {
        bj: *KvBackend,
        slot: u16,
        fired: u32 = 0,
        fn call(ctx: *anyopaque) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.fired += 1;
            compactRootAndCover(self.bj, self.slot) catch return;
            compactRootAndCover(self.bj, self.slot) catch return;
        }
    };
    var move_on: MoveOn = .{ .bj = bj, .slot = slot };
    backend.test_seams.reader_after_pages = .{ .ctx = &move_on, .call = &MoveOn.call };
    defer backend.test_seams = .{};
    const skipped0 = backend.stats.reader_foreign_roots_skipped.load();
    {
        const reader = try MetaSet.open(gpa, s.io(), s.pathList(), ro);
        defer reader.close();
        backend.test_seams = .{};
        const live3 = bj.slotTree(slot).?.rootPtr();
        std.debug.print("B moved on inside the reader's open: the page named 0x{x} seq {d}, B's root is 0x{x} seq {d}, that image allocated {}; hook fired {d}, foreign roots skipped +{d}\n", .{ live2.addr, live2.seq, live3.addr, live3.seq, bj.alloc.isAllocated(bj.cache.addrExtent(live2.addr)), move_on.fired, backend.stats.reader_foreign_roots_skipped.load() - skipped0 });
        try testing.expectEqual(@as(u32, 1), move_on.fired);
        try testing.expect(live3.addr == live2.addr and live3.seq != live2.seq);
        try testing.expectEqual(skipped0 + 1, backend.stats.reader_foreign_roots_skipped.load());
        try testing.expect(reader.volumes[0].slotTree(slot) == null);
        try testing.expect(reader.volumes[0].guest_published.get(slot) == null);
    }
    // 4. The same race on a slot the MANAGER holds: its page names the
    //    root, and A moves on inside the reader's open, recycling the
    //    image the page named. A's page is refreshed by the cycle that
    //    covers the free, so the slot's homes re-read name the live root:
    //    the reader retries that one slot and opens it there.
    var a_slot: u16 = 0;
    {
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        try be0.slots.table.heldBy(lt.MANAGER, &held);
        for (held.items) |s2| if (be0.stamp.native_slot != s2 and s2 != slot) {
            a_slot = s2;
            break;
        };
    }
    try testing.expect(a_slot != 0);
    try fillSlot(be0, a_slot, 1, &@as([200]u8, @splat('m')));
    try be0.checkpointNow();
    const a_root = be0.slotTree(a_slot).?.rootPtr();
    var a_moves: MoveOn = .{ .bj = be0, .slot = a_slot };
    backend.test_seams.reader_after_pages = .{ .ctx = &a_moves, .call = &MoveOn.call };
    {
        const reader = try MetaSet.open(gpa, s.io(), s.pathList(), ro);
        defer reader.close();
        backend.test_seams = .{};
        const a_live = be0.slotTree(a_slot).?.rootPtr();
        std.debug.print("A moved on inside the reader's open: the page named 0x{x} seq {d}, A's root is 0x{x} seq {d}; hook fired {d}\n", .{ a_root.addr, a_root.seq, a_live.addr, a_live.seq, a_moves.fired });
        try testing.expectEqual(@as(u32, 1), a_moves.fired);
        try testing.expect(a_live.seq != a_root.seq);
        try testing.expectEqual(a_live, reader.volumes[0].guest_published.get(a_slot) orelse return error.TestSlotNotProjected);
        try testing.expectEqual(skipped0 + 1, backend.stats.reader_foreign_roots_skipped.load());
    }
}

/// The block references of `vol_tag` the census of `be` reads (every slot
/// tree it holds, `rangeRefs`).
fn refsOf(gpa: std.mem.Allocator, be: *KvBackend, vol_tag: u64) !usize {
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    const vr = key.volumeRange(vol_tag);
    try be.range(gpa, key.TREE_BLOCK_REFS, vr.lo.slice(), vr.hi.slice(), std.math.maxInt(usize), &kvs);
    return kvs.items.len;
}

test "linux: THE MANAGER'S REMOUNT BESIDE A LIVE LESSEE HOLDS NONE OF ITS TREES (decision 0003 §CF.1; the writer's half of the reader's two-homes fix): A mints a slot tree holding a block reference and hands the slot to B — the live manager forgets the tree at the grant, its census holds none of B's references; B compacts the root, then deletes the reference; A's remount beside the live B holds the same (no tree of the slot, the reference B deleted counted nowhere — never the grant-time word's stale tree); B compacts on until its pool recycles the grant-time image and the remount still opens (it refused Corrupt: 'slot N's tree root … does not verify')" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "writer-defers-lessee", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    // No cycle by age: the devices move only with the test's cycles.
    try s.env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "60000");
    var cut_buf: [192]u8 = undefined;
    const cut = try std.fmt.bufPrintSentinel(&cut_buf, "{s}.cut", .{s.paths[0]}, 0);
    defer _ = std.os.linux.unlink(cut.ptr);
    const ref_vol: u64 = 0x5EED;
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
    // A's tree of the slot: an inode and a block reference it owns.
    try fillSlot(be0, slot, 1, &@as([200]u8, @splat('a')));
    const ref_key = key.blockRefKey(ref_vol, 42, key.guestLocalIno(slot, try be0.allocateInoInSlot(slot)), 0);
    {
        const t = try be0.begin(@src());
        try t.stagePut(key.TREE_BLOCK_REFS, &ref_key, "r");
        try be0.commit(t);
    }
    try be0.checkpointNow();
    try testing.expectEqual(@as(usize, 1), try refsOf(gpa, be0, ref_vol));
    const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-writer-defers-lessee-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
    const sink = bj.slots.carriageSink();
    sink.offered(sink.ctx, slot, lt.MANAGER);
    try testing.expect(bj.slots.table.view(slot).leasedBy(me));
    try bj.checkpointNow();
    const grant_time = try tree0Root(gpa, be0, slot);
    // The live manager's posture: the tree is the lessee's from the grant
    // on — none here, none of its references in this census.
    try testing.expect(be0.slotTree(slot) == null);
    try testing.expectEqual(@as(usize, 0), try refsOf(gpa, be0, ref_vol));
    try testing.expectEqual(@as(usize, 1), try refsOf(gpa, bj, ref_vol));
    // B moves the root (the grant-time image retires into B's pool, its
    // bytes intact), then deletes the reference in the moved tree.
    try compactRootAndCover(bj, slot);
    {
        const t = try bj.begin(@src());
        try t.stageDelete(key.TREE_BLOCK_REFS, &ref_key);
        try bj.commit(t);
    }
    try bj.checkpointNow();
    try testing.expectEqual(@as(usize, 0), try refsOf(gpa, bj, ref_vol));
    try testing.expectEqual(grant_time, try tree0Root(gpa, be0, slot));
    // 1. A's remount (its device cut beside the live B): what the live
    //    manager holds — no tree of B's slot, B's deleted reference
    //    nowhere in its census.
    try backend.snapshotImage(s.paths[0], cut);
    const deferred0 = backend.stats.writer_foreign_trees_deferred.load();
    {
        const copy = try KvBackend.open(gpa, s.io(), cut, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
        defer copy.close();
        try testing.expectEqual(deferred0 + 1, backend.stats.writer_foreign_trees_deferred.load());
        const held = if (copy.slotTree(slot)) |t| t.rootPtr() else null;
        const counted = try refsOf(gpa, copy, ref_vol);
        std.debug.print("\nthe remount beside the live lessee: slot {d}'s tree here {?}, the grant-time root 0x{x} seq {d}; B's root 0x{x} seq {d}; references counted {d} (B holds 0, the live manager 0)\n", .{ slot, held, grant_time.addr, grant_time.seq, bj.slotTree(slot).?.rootPtr().addr, bj.slotTree(slot).?.rootPtr().seq, counted });
        try testing.expect(held == null);
        try testing.expectEqual(@as(usize, 0), counted);
    }
    // 2. B's pool recycles the grant-time image: tree 0's word now names
    //    a node under another seq; the remount still opens.
    var rounds: u32 = 0;
    while (!bj.alloc.isAllocated(bj.cache.addrExtent(grant_time.addr)) and rounds < 8) : (rounds += 1) try compactRootAndCover(bj, slot);
    try testing.expect(bj.alloc.isAllocated(bj.cache.addrExtent(grant_time.addr)));
    try backend.snapshotImage(s.paths[0], cut);
    {
        const copy = KvBackend.open(gpa, s.io(), cut, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 }) catch |e| {
            std.debug.print("the remount refused {s} after B recycled the grant-time image ({d} compaction round(s))\n", .{ @errorName(e), rounds });
            return e;
        };
        defer copy.close();
        try testing.expectEqual(deferred0 + 2, backend.stats.writer_foreign_trees_deferred.load());
        try testing.expect(copy.slotTree(slot) == null);
    }
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

// ---------------------------------------------------------------------------
// THE JOINER'S INTENT HOME (found by generic/732 under the symmetric
// default, `rung-10-adjudication-audit.md` §2.4 / F8): a rename through
// the joined mount whose EVERY object the manager holds has a foreign step
// 0, so its intent is written ALONE into "the mount's own rotor slot" —
// and the joiner's resolver named slot 0 (the manager's native slot) as
// that home. The joiner's own door refused the commit (another appender
// leases slot 0: `SlotBusy` → `SlotMoved`), nothing moved, and the move
// gate re-issued the same refusal until the fail-stop deadline (90 s, EIO).
// No handover is involved: the law is that a cross-owner op resolves
// within its round trips, never the deadline.
// ---------------------------------------------------------------------------

/// The bound on a cross-owner rename with nothing moving: generous against
/// a loaded venue, far below the gate's deadline the defect answered at.
const CROSS_OWNER_RENAME_BOUND_NS: u64 = 2 * std.time.ns_per_s;

fn timedRename(f: filesystem.Filesystem, c: *filesystem.Ctx, parent: u64, name: []const u8, newparent: u64, newname: []const u8) !void {
    const t0 = clock.monotonicNs();
    try f.vtable.rename(f.impl, c, .{ .parent = parent, .name = name, .newparent = newparent, .newname = newname, .flags = 0 });
    const took = clock.monotonicNs() - t0;
    if (took > CROSS_OWNER_RENAME_BOUND_NS) {
        std.debug.print("cross-owner rename {s} → {s} took {d} ms (bound {d} ms)\n", .{ name, newname, took / std.time.ns_per_ms, CROSS_OWNER_RENAME_BOUND_NS / std.time.ns_per_ms });
        return error.RenameTooSlow;
    }
}

test "linux: A JOINER'S RENAME WHOSE EVERY OBJECT IS THE MANAGER'S resolves within its round trips (F8, generic/732 under the symmetric default): B moves A's file between A's directories and back, A moves B's file between B's directories and back, and each moves the OTHER's file between the other's directories — every rename ≤ 2 s and never the move gate's deadline, the intent's home a slot the initiator LEASES (slot 0 is the manager's), minted ≡ retired on both, both members read the same tree" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "joiner-rename-home", 1);
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
    const afs = a.face();
    // A's directories and A's file: generic/732's `mkdir A B; touch A/f`.
    const da = (try a.fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "A", .mode = 0o755, .umask = 0 })).nodeid;
    const db = (try a.fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "B", .mode = 0o755, .umask = 0 })).nodeid;
    const f = (try a.fs.create(&c, .{ .parent = da, .name = "f", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
    try timedRename(afs, &c, da, "f", db, "f");
    try be0.checkpointNow();
    // B joins behind the product's move gate at the product's deadline.
    const b = try MetaSet.open(gpa, io, s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-joiner-rename-home-b", .endpoint = a.plane.advertised } });
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
    // The joiner's intent home is a slot IT leases (its rotor), never the
    // manager's native slot 0; its roll-forward homes are its own too.
    {
        const own = bm.bound.plane.resolver.ownHome(bm.bound.plane.resolver.ctx) orelse return error.NoOwnHome;
        try testing.expect(own != 0);
        try testing.expect(bj.slots.table.view(own).leasedBy(me) and bj.slots.table.view(own).rotor);
        var buf: [512]u16 = undefined;
        for (bm.bound.plane.resolver.localSlots(bm.bound.plane.resolver.ctx, &buf)) |sl| try testing.expect(bj.slots.table.view(sl).leasedBy(me));
        try testing.expectEqual(@as(?u16, 0), a.bound.plane.resolver.ownHome(a.bound.plane.resolver.ctx));
    }
    // Process-wide counters: judged as deltas (another shard-mate's test
    // leaves intents open on purpose).
    const unresolved0 = move_gate.stats.slot_move_unresolved.load();
    const minted0 = xv.intent.stats.xv_cross_owner_intents_minted.load();
    const retired0 = xv.intent.stats.xv_cross_owner_intents_retired.load();
    const open0 = xv.intent.stats.xv_cross_owner_intents_open.load();
    // generic/732's shape: `cat B/f` then `mv B/f A/` through the joiner,
    // then back through the manager, then once more through the joiner.
    try testing.expectEqual(f, try lookupIno(bfs, &c, db, "f"));
    try timedRename(bfs, &c, db, "f", da, "f");
    try testing.expectEqual(f, try lookupIno(afs, &c, da, "f"));
    try timedRename(afs, &c, da, "f", db, "f");
    try timedRename(bfs, &c, db, "f", da, "f");
    // The mirror: B's directories and B's file; the manager moves it
    // (every object the joiner's), then the joiner moves it back.
    const dc = (try bfs.vtable.mkdir(bfs.impl, &c, .{ .parent = inos.ROOT_INO, .name = "C", .mode = 0o755, .umask = 0 })).nodeid;
    const dd = (try bfs.vtable.mkdir(bfs.impl, &c, .{ .parent = inos.ROOT_INO, .name = "D", .mode = 0o755, .umask = 0 })).nodeid;
    const g = (try bfs.vtable.create(bfs.impl, &c, .{ .parent = dc, .name = "g", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
    try testing.expect(bj.slots.table.view(route_table.slotOf(dc)).leasedBy(me));
    try testing.expect(bj.slots.table.view(route_table.slotOf(g)).leasedBy(me));
    try timedRename(afs, &c, dc, "g", dd, "g");
    try timedRename(bfs, &c, dd, "g", dc, "g");
    // The other member's file between the other member's directories:
    // B's file into A's directories through B, A's file into B's
    // directories through A, and each back.
    const h = (try bfs.vtable.create(bfs.impl, &c, .{ .parent = da, .name = "h", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
    try testing.expect(bj.slots.table.view(route_table.slotOf(h)).leasedBy(me));
    try timedRename(bfs, &c, da, "h", db, "h");
    try timedRename(afs, &c, db, "h", da, "h");
    const i = (try a.fs.create(&c, .{ .parent = dc, .name = "i", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
    try timedRename(afs, &c, dc, "i", dd, "i");
    try timedRename(bfs, &c, dd, "i", dc, "i");
    // Never the deadline; every intent of this test's retired, none open.
    try testing.expectEqual(unresolved0, move_gate.stats.slot_move_unresolved.load());
    const minted = xv.intent.stats.xv_cross_owner_intents_minted.load() - minted0;
    try testing.expect(minted >= 8);
    try testing.expectEqual(minted, xv.intent.stats.xv_cross_owner_intents_retired.load() - retired0);
    try testing.expectEqual(open0, xv.intent.stats.xv_cross_owner_intents_open.load());
    // Both members read the same tree: f in A, g in C, h in A, i in C.
    for ([_]filesystem.Filesystem{ afs, bfs }) |fs_| {
        try testing.expectEqual(f, try lookupIno(fs_, &c, da, "f"));
        try testing.expectEqual(@as(u64, 0), try lookupIno(fs_, &c, db, "f"));
        try testing.expectEqual(g, try lookupIno(fs_, &c, dc, "g"));
        try testing.expectEqual(@as(u64, 0), try lookupIno(fs_, &c, dd, "g"));
        try testing.expectEqual(h, try lookupIno(fs_, &c, da, "h"));
        try testing.expectEqual(i, try lookupIno(fs_, &c, dc, "i"));
        for ([_]u64{ f, g, h, i }) |ino| try testing.expectEqual(@as(u32, 1), (try fs_.vtable.getattr(fs_.impl, &c, .{ .ino = ino, .fh = null })).attr.nlink);
    }
}

// ---------------------------------------------------------------------------
// THE MINT AGAINST A HANDOVER OF ITS SLOT (decision 0003 §BV). A mint's slot
// decision and its cursor `fetch_add` are two steps; a handover of the slot
// that lands between them carries the cursor it read to the receiver, and
// the minter's `fetch_add` then hands out the receiver's first ino.
// ---------------------------------------------------------------------------

test "linux: A MINT RACING THE HANDOVER OF ITS SLOT keeps no ino the receiver mints (decision 0003 §BV; `mint_window_model.MintRecheck`): A's create decides its rotor slot and parks before the cursor's fetch_add; A hands that slot to B, which mints its first ino there off the cursor A carried; A's create resumes — the ino it answers is never B's (red: the same global ino, minted twice)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "mint-race", 1);
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
    const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-mint-race-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();
    // A's create under the root: the census picks a rotor slot, the seam
    // parks it before the fetch_add.
    be0.slots.mint_seam.armed.store(true, .release);
    const Minter = struct {
        fn run(st: *MetaSet, out: *(meta_set.Error!u64)) void {
            out.* = st.allocateIno(st.effectiveIno(inos.ROOT_INO), false, false, 0, .file);
        }
    };
    var minted: meta_set.Error!u64 = error.Io;
    const th = try std.Thread.spawn(.{}, Minter.run, .{ set, &minted });
    var joined_minter = false;
    defer if (!joined_minter) {
        be0.slots.mint_seam.release.set(s.io());
        th.join();
    };
    be0.slots.mint_seam.reached.waitUncancelable(s.io());
    const slot: u16 = @intCast(be0.slots.mint_seam.slot.load(.acquire));
    try testing.expect(be0.slots.table.view(slot).leasedBy(lt.MANAGER));
    // The slot's tree on the device, then the handover to B: B's cursor is
    // floored at the one A carried.
    _ = try commitRecord(be0, slot, "written by A");
    try be0.checkpointNow();
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, slot, me, std.math.maxInt(u64)));
    const sink = bj.slots.carriageSink();
    sink.offered(sink.ctx, slot, lt.MANAGER);
    try testing.expect(bj.slots.table.view(slot).leasedBy(me));
    const b_global = inos.makeGlobalIno(try bj.allocateInoInSlot(slot), slot, inos.ROUTING_WIDTH);
    // A's create resumes: its ino is burned (the slot stands at another
    // generation, B's) and the mint redone in a slot A leases.
    const burns0 = acquire.stats.mint_burns.load();
    be0.slots.mint_seam.release.set(s.io());
    th.join();
    joined_minter = true;
    const a_global = set.globalIno(try minted);
    std.debug.print("mint race: A's create answered ino {d} (slot {d}); B minted ino {d} in the slot A handed over ({d})\n", .{ a_global, route_table.slotOf(a_global), b_global, slot });
    try testing.expect(a_global != b_global);
    // What A answers is a slot A still leases.
    try testing.expect(be0.slots.table.view(route_table.slotOf(a_global)).leasedBy(lt.MANAGER));
    try testing.expect(acquire.stats.mint_burns.load() >= burns0 + 1);
}

test "linux: CONCURRENT MINTERS NEVER MINT ONE INO TWICE, across window refills and a handover of a windowed slot (decision 0003 §BV): eight threads mint through A's set, each past two window refills; mid-run, every thread paused inside its window, A hands the tree one window claims to B, which mints there; the threads resume — every ino A's threads and B minted is distinct, the windows stood on distinct trees, and the robbed window re-picked a tree A leases" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "mint-windows", 1);
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
    const b = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-mint-windows-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    const me = bj.joined.?.appenderId();

    const threads = 8;
    const per_phase: usize = be0.slots.window_mints + be0.slots.window_mints / 2;
    const Shared = struct {
        set: *MetaSet,
        per_phase: usize,
        arrived: std.atomic.Value(u32) = .init(0),
        go: std.atomic.Value(bool) = .init(false),
        failed: std.atomic.Value(bool) = .init(false),
    };
    const Minter = struct {
        sh: *Shared,
        out: std.ArrayList(u64) = .empty,

        fn mintPhase(self: *@This()) void {
            for (0..self.sh.per_phase) |_| {
                const eff = self.sh.set.allocateIno(self.sh.set.effectiveIno(inos.ROOT_INO), false, false, 0, .file) catch {
                    self.sh.failed.store(true, .release);
                    return;
                };
                self.out.append(testing.allocator, self.sh.set.globalIno(eff)) catch {
                    self.sh.failed.store(true, .release);
                    return;
                };
            }
        }

        fn run(self: *@This()) void {
            self.mintPhase();
            _ = self.sh.arrived.fetchAdd(1, .acq_rel);
            while (!self.sh.go.load(.acquire)) std.Thread.yield() catch {};
            self.mintPhase();
        }
    };
    var sh: Shared = .{ .set = set, .per_phase = per_phase };
    var minters: [threads]Minter = undefined;
    for (&minters) |*m| m.* = .{ .sh = &sh };
    defer for (&minters) |*m| m.out.deinit(gpa);
    const grants0 = slots_mod.mint_window.stats.mint_window_grants.load();
    const wmints0 = slots_mod.mint_window.stats.mint_window_mints.load();
    var ths: [threads]std.Thread = undefined;
    var spawned: usize = 0;
    defer {
        sh.go.store(true, .release);
        for (ths[0..spawned]) |t| t.join();
    }
    for (&ths, &minters) |*t, *m| {
        t.* = try std.Thread.spawn(.{}, Minter.run, .{m});
        spawned += 1;
    }
    while (sh.arrived.load(.acquire) < threads) std.Thread.yield() catch {};
    try testing.expect(!sh.failed.load(.acquire));
    // Every thread paused inside a window: the live claims name distinct
    // trees, one per windowed thread.
    const now_ms = clock.monotonicNs() / std.time.ns_per_ms;
    var claimed: std.ArrayList(u16) = .empty;
    defer claimed.deinit(gpa);
    var held: std.ArrayList(u16) = .empty;
    defer held.deinit(gpa);
    try be0.slots.table.heldBy(lt.MANAGER, &held);
    for (held.items) |sl| if (be0.slots.claims.live(sl, now_ms)) try claimed.append(gpa, sl);
    try testing.expect(claimed.items.len >= 2);
    try testing.expect(claimed.items.len <= threads);
    // The handover of one windowed tree to B; B mints there off the
    // cursor A carried.
    const robbed = claimed.items[0];
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try be0.slots.verbs.offerSlot(lt.MANAGER, robbed, me, std.math.maxInt(u64)));
    const sink = bj.slots.carriageSink();
    sink.offered(sink.ctx, robbed, lt.MANAGER);
    try testing.expect(bj.slots.table.view(robbed).leasedBy(me));
    var b_inos: [64]u64 = undefined;
    for (&b_inos) |*g| g.* = inos.makeGlobalIno(try bj.allocateInoInSlot(robbed), robbed, inos.ROUTING_WIDTH);
    sh.go.store(true, .release);
    for (ths[0..spawned]) |t| t.join();
    spawned = 0;
    try testing.expect(!sh.failed.load(.acquire));
    // Every ino distinct; A's second phase never in the robbed tree.
    var all: std.ArrayList(u64) = .empty;
    defer all.deinit(gpa);
    try all.appendSlice(gpa, &b_inos);
    for (&minters) |*m| {
        try testing.expectEqual(2 * per_phase, m.out.items.len);
        try all.appendSlice(gpa, m.out.items);
        for (m.out.items[per_phase..]) |g| try testing.expect(route_table.slotOf(g) != robbed);
    }
    std.mem.sort(u64, all.items, {}, std.sort.asc(u64));
    for (all.items[1..], 1..) |g, i| if (g == all.items[i - 1]) {
        std.debug.print("ino {d} (slot {d}) minted twice\n", .{ g, route_table.slotOf(g) });
        return error.TestUnexpectedResult;
    };
    // The windows served the run: each thread's grants, the mints beside.
    try testing.expect(slots_mod.mint_window.stats.mint_window_grants.load() >= grants0 + 2 * threads);
    try testing.expect(slots_mod.mint_window.stats.mint_window_mints.load() >= wmints0 + threads * per_phase);
    std.debug.print("mint windows: {d} threads × {d} mints, {d} live claims at the pause, slot {d} handed to B; grants +{d}, window mints +{d}\n", .{ threads, 2 * per_phase, claimed.items.len, robbed, slots_mod.mint_window.stats.mint_window_grants.load() - grants0, slots_mod.mint_window.stats.mint_window_mints.load() - wmints0 });
}
