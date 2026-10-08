//! The per-volume membership stamp at RUNTIME (`docs/porting/
//! dynamic_routing.md` §1.6): the stored membership (set, epoch, position,
//! runs, native slot) plus the LIVE guest-cursor cells, folded into the
//! stamp image every checkpoint's ledger record carries, and mutated by the
//! flip's stamp writes (claim + cursor in; release + cursor out) and the
//! offline verbs' re-stamps. PORTABLE.
//!
//! A guest slot's cursor is an `inos.InoWatermark` cell (the same
//! publication edge as the native watermark: a mint whose record reached
//! the tree before the checkpoint's flush synchronizes-with the snapshot
//! load, so a ledger record never under-declares a cursor it covers). A
//! virgin hosted slot mints at 2 — its keyspace is empty by construction
//! (only travelling cursors and replay maxima make records, and both
//! install floors). Cells are read latch-free (one atomic pointer load per
//! mint) and created under a lock on the first touch of a slot.

const std = @import("std");
const Allocator = std.mem.Allocator;
const key = @import("../key.zig");
const inos = @import("../inos.zig");
const ledger = @import("../ledger.zig");
const jc = @import("../journal_core.zig");
const route_table = @import("route_table.zig");
const rstats = @import("stats.zig");

const assert = std.debug.assert;
const W = route_table.W;

pub const StampCell = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    set_uuid: [16]u8,
    set_epoch: u64,
    member_position: u16,
    member_count: u16,
    native_slot: ?u16,
    /// The hosted set as validated runs (owned).
    runs: []ledger.SlotRun,
    /// One cell pointer per routing slot (512 KiB): null until the slot's
    /// first mint or an installed floor.
    cells: []std.atomic.Value(?*inos.InoWatermark),
    /// The slots with a cell, in creation order (the fold's iteration).
    created: std.ArrayList(u16) = .empty,
    /// Cells of released slots: retired, freed with the volume (a mint
    /// racing the release may still hold the pointer).
    retired: std.ArrayList(*inos.InoWatermark) = .empty,
    /// `forLedger`'s output storage (valid until the next call).
    scratch: std.ArrayList(ledger.SlotCursor) = .empty,

    /// From a decoded stamp: runs copied, every stamped cursor a cell.
    pub fn init(gpa: Allocator, stamp: ledger.MembershipStamp) Allocator.Error!*StampCell {
        const self = try gpa.create(StampCell);
        errdefer gpa.destroy(self);
        const runs = try gpa.dupe(ledger.SlotRun, stamp.slots_hosted.runs);
        errdefer gpa.free(runs);
        const cells = try gpa.alloc(std.atomic.Value(?*inos.InoWatermark), W);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .init(null);
        self.* = .{
            .gpa = gpa,
            .set_uuid = stamp.set_uuid,
            .set_epoch = stamp.set_epoch,
            .member_position = stamp.member_position,
            .member_count = stamp.member_count,
            .native_slot = stamp.native_slot,
            .runs = runs,
            .cells = cells,
        };
        errdefer self.deinitCells();
        for (stamp.slot_cursors) |c| try self.installFloor(c.slot, c.next_local_ino);
        return self;
    }

    fn deinitCells(self: *StampCell) void {
        for (self.created.items) |s| {
            if (self.cells[s].load(.monotonic)) |cell| self.gpa.destroy(cell);
        }
        self.created.deinit(self.gpa);
        for (self.retired.items) |cell| self.gpa.destroy(cell);
        self.retired.deinit(self.gpa);
    }

    pub fn deinit(self: *StampCell) void {
        self.deinitCells();
        self.scratch.deinit(self.gpa);
        self.gpa.free(self.runs);
        self.gpa.free(self.cells);
        self.gpa.destroy(self);
    }

    /// The cell of guest slot `slot` (created at 2 on first touch).
    pub fn cursorFor(self: *StampCell, slot: u16) Allocator.Error!*inos.InoWatermark {
        // acquire pairs with the creating store's release.
        if (self.cells[slot].load(.acquire)) |c| return c;
        self.lock.lock();
        defer self.lock.unlock();
        if (self.cells[slot].load(.acquire)) |c| return c;
        const cell = try self.gpa.create(inos.InoWatermark);
        errdefer self.gpa.destroy(cell);
        cell.* = inos.InoWatermark.init(inos.LOCAL_INO_BASE);
        try self.created.append(self.gpa, slot);
        self.cells[slot].store(cell, .release);
        return cell;
    }

    pub fn cursor(self: *const StampCell, slot: u16) ?*inos.InoWatermark {
        return self.cells[slot].load(.acquire);
    }

    /// Raise slot `slot`'s cursor to at least `floor` (a stamped cursor,
    /// a replay maximum, a travelled cursor): idempotent, never regresses.
    pub fn installFloor(self: *StampCell, slot: u16, floor: u64) Allocator.Error!void {
        const cell = try self.cursorFor(slot);
        cell.installFloor(floor);
    }

    /// The cursor's snapshot, null when the slot never minted here.
    pub fn cursorSnapshot(self: *const StampCell, slot: u16) ?u64 {
        const c = self.cursor(slot) orelse return null;
        return c.snapshot();
    }

    /// Drop slot `slot`'s cell (a flip's source after write 2): the cell is
    /// retired, never freed — a mint racing the flip may hold it; the
    /// retired cells go with the volume. A slot that returns later gets a
    /// fresh cell (its cursor travels back with the flip).
    pub fn removeCursor(self: *StampCell, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        for (self.created.items, 0..) |s, i| if (s == slot) {
            _ = self.created.swapRemove(i);
            if (self.cells[slot].swap(null, .acq_rel)) |cell| self.retired.append(self.gpa, cell) catch {
                // Unretirable only under OOM: the cell leaks with the
                // mount rather than dangling under a racing mint.
            };
            return;
        };
    }

    pub fn hosts(self: *StampCell, slot: u16) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return (ledger.SlotSet{ .runs = self.runs }).contains(slot);
    }

    pub fn hostedCount(self: *StampCell) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return (ledger.SlotSet{ .runs = self.runs }).len();
    }

    /// Replace the hosted set (the flip's `±slot`, the verbs' re-stamps):
    /// `slots_ascending` coalesced into runs.
    pub fn setHosted(self: *StampCell, slots_ascending: []const u16) Allocator.Error!void {
        const runs = try route_table.coalesceRuns(self.gpa, slots_ascending);
        self.lock.lock();
        defer self.lock.unlock();
        self.gpa.free(self.runs);
        self.runs = runs;
    }

    /// The hosted set as an ascending slot list (caller frees).
    pub fn hostedSlots(self: *StampCell, gpa: Allocator) Allocator.Error![]u16 {
        self.lock.lock();
        defer self.lock.unlock();
        const set = ledger.SlotSet{ .runs = self.runs };
        const out = try gpa.alloc(u16, set.len());
        var n: usize = 0;
        var s: u32 = 0;
        while (s < W) : (s += 1) {
            if (set.contains(@intCast(s))) {
                out[n] = @intCast(s);
                n += 1;
            }
        }
        assert(n == out.len);
        return out;
    }

    /// `hosted ∪ {slot}`.
    pub fn claimSlot(self: *StampCell, slot: u16) Allocator.Error!void {
        const cur = try self.hostedSlots(self.gpa);
        defer self.gpa.free(cur);
        var next: std.ArrayList(u16) = .empty;
        defer next.deinit(self.gpa);
        var inserted = false;
        for (cur) |s| {
            if (!inserted and s > slot) {
                try next.append(self.gpa, slot);
                inserted = true;
            }
            if (s == slot) inserted = true;
            try next.append(self.gpa, s);
        }
        if (!inserted) try next.append(self.gpa, slot);
        try self.setHosted(next.items);
    }

    /// `hosted ∖ {slot}`.
    pub fn releaseSlot(self: *StampCell, slot: u16) Allocator.Error!void {
        const cur = try self.hostedSlots(self.gpa);
        defer self.gpa.free(cur);
        var next: std.ArrayList(u16) = .empty;
        defer next.deinit(self.gpa);
        for (cur) |s| if (s != slot) try next.append(self.gpa, s);
        try self.setHosted(next.items);
    }

    pub fn setEpoch(self: *StampCell, epoch: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.set_epoch = epoch;
    }

    pub fn setMembership(self: *StampCell, position: u16, count: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.member_position = position;
        self.member_count = count;
    }

    /// The stamp image a ledger record carries: the stored membership with
    /// every live cell's snapshot folded in, sorted by slot. Borrows the
    /// cell's storage until the next call (the checkpointer's record sites
    /// run one at a time under the volume's SMO lock). The budget gauges
    /// move here: a stamp past either cap is the encoder's loud refusal.
    pub fn forLedger(self: *StampCell) ledger.MembershipStamp {
        self.lock.lock();
        defer self.lock.unlock();
        self.scratch.clearRetainingCapacity();
        for (self.created.items) |s| {
            const cell = self.cells[s].load(.acquire) orelse continue;
            // The scratch list can only fail to grow past the cursor cap;
            // the encoder then refuses the record loud, so appending
            // without the slot is never silent.
            self.scratch.append(self.gpa, .{ .slot = s, .next_local_ino = cell.snapshot() }) catch break;
        }
        std.mem.sort(ledger.SlotCursor, self.scratch.items, {}, struct {
            fn lt(_: void, a: ledger.SlotCursor, b: ledger.SlotCursor) bool {
                return a.slot < b.slot;
            }
        }.lt);
        if (self.runs.len > rstats.stats.routing_stamp_runs_max.load()) rstats.stats.routing_stamp_runs_max.set(self.runs.len);
        if (self.scratch.items.len > rstats.stats.routing_stamp_cursors_max.load()) rstats.stats.routing_stamp_cursors_max.set(self.scratch.items.len);
        return .{
            .set_uuid = self.set_uuid,
            .set_epoch = self.set_epoch,
            .member_position = self.member_position,
            .member_count = self.member_count,
            .slots_hosted = .{ .runs = self.runs },
            .native_slot = self.native_slot,
            .slot_cursors = self.scratch.items,
        };
    }

    /// `inos.liveInodes`' guest input: every live cell's snapshot.
    pub fn cursorSnapshots(self: *StampCell, gpa: Allocator) Allocator.Error![]u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const out = try gpa.alloc(u64, self.created.items.len);
        for (self.created.items, out) |s, *o| o.* = if (self.cells[s].load(.acquire)) |c| c.snapshot() else inos.LOCAL_INO_BASE;
        return out;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "a cell per first-touched guest slot at 2; stamped cursors install as floors; the ledger fold lists live cells sorted by slot and never under-declares a mint; claim/release rewrite the runs; remove drops a cursor from the fold" {
    const gpa = testing.allocator;
    var run: [1]ledger.SlotRun = undefined;
    var stamp = ledger.MembershipStamp.member("set-uuid-fixture".*, 1, 2, &run);
    const stamped = [_]ledger.SlotCursor{ .{ .slot = 9, .next_local_ino = 500 }, .{ .slot = 3, .next_local_ino = 2 } };
    stamp.slot_cursors = &stamped;
    const c = try StampCell.init(gpa, stamp);
    defer c.deinit();
    try testing.expectEqual(@as(?u64, 500), c.cursorSnapshot(9));
    try testing.expectEqual(@as(?u64, 2), c.cursorSnapshot(3));
    try testing.expectEqual(@as(?u64, null), c.cursorSnapshot(5));
    const cell5 = try c.cursorFor(5);
    try testing.expectEqual(@as(u64, 2), cell5.allocate());
    try testing.expectEqual(@as(u64, 3), cell5.allocate());
    try testing.expectEqual(cell5, try c.cursorFor(5));
    try c.installFloor(5, 3);
    try testing.expectEqual(@as(?u64, 4), c.cursorSnapshot(5));
    const img = c.forLedger();
    try testing.expectEqualSlices(ledger.SlotCursor, &.{ .{ .slot = 3, .next_local_ino = 2 }, .{ .slot = 5, .next_local_ino = 4 }, .{ .slot = 9, .next_local_ino = 500 } }, img.slot_cursors);
    try testing.expectEqual(@as(?u16, null), img.native_slot);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ledger.memberRun(1, 2)}, img.slots_hosted.runs);
    try testing.expect(rstats.stats.routing_stamp_cursors_max.load() >= 3);
    // The fold round-trips through the ledger codec.
    const rec: ledger.LedgerRecord = .{ .seq = 7, .tree_roots = &.{}, .journal_tail_seq = 0, .next_ino = 2, .alloc_bitmap_generation = 1, .node_seq_watermark = 1, .membership_stamp = img };
    const slot_img = try rec.encodeSlot();
    var got = try ledger.LedgerRecord.decodeSlot(gpa, &slot_img);
    defer got.deinit(gpa);
    try testing.expectEqualSlices(ledger.SlotCursor, img.slot_cursors, got.membership_stamp.slot_cursors);
    // Claim slot 4 (a flip in): the runs become {1 stride 2 …} + singleton 4
    // coalesced; release it again → one run.
    try testing.expect(!c.hosts(4));
    try c.claimSlot(4);
    try testing.expect(c.hosts(4));
    try testing.expectEqual(@as(usize, 32769), c.hostedCount());
    try testing.expect(c.forLedger().slots_hosted.runs.len >= 2);
    try c.releaseSlot(4);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ledger.memberRun(1, 2)}, c.forLedger().slots_hosted.runs);
    try c.releaseSlot(1);
    try testing.expect(!c.hosts(1));
    try testing.expectEqual(@as(u16, 3), c.forLedger().slots_hosted.runs[0].start);
    c.removeCursor(9);
    try testing.expectEqual(@as(usize, 2), c.forLedger().slot_cursors.len);
    c.setEpoch(5);
    c.setMembership(1, 3);
    const f = c.forLedger();
    try testing.expectEqual(@as(u64, 5), f.set_epoch);
    try testing.expectEqual(@as(u16, 3), f.member_count);
    const snaps = try c.cursorSnapshots(gpa);
    defer gpa.free(snaps);
    try testing.expectEqual(@as(usize, 2), snaps.len);
}
