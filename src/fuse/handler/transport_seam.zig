//! The handler layer's view of the transport (`docs/porting/
//! fuse_handlers_lanes.md` §1.7, §1.11; charter §4 "Per-entry slot state
//! machine"): ONE reply path — a handler commits through the transport by
//! SLOT REFERENCE from whatever thread it runs on; no reply channel, no
//! reply task (decision 0002 ledger: the reference's two reply arms).
//!
//! The transport is a COMPTIME parameter of the dispatcher (`lifecycle.
//! Dispatcher(Transport)`): duck-typed, checked by `assertTransport`. The
//! ring transport (`src/fuse/transport/*`, its own module) binds by
//! satisfying this shape; until it lands the `Loopback` below is the one
//! transport the layer compiles and tests against. The final binding is the
//! import swap in `handler.zig` (`pub const RingTransport = …`).
//!
//! The contract a transport provides:
//!
//!   const SlotRef      — the reply address carried IN the request (a
//!                        value: `(qid, ent_idx, commit_id)` on the ring,
//!                        the classical marker on the sideband); never a
//!                        `unique → slot` map (fuse_handlers_lanes.md §1.3).
//!   const PayloadLease — a WRITE's zero-copy payload lease the dispatcher
//!                        releases once the handler returned (the commit
//!                        gate parks a reply while a lease lives).
//!   fn replyWindow(*T, SlotRef) []u8
//!                      — the ent's payload region: where READ data,
//!                        dirents, link targets and xattr bytes are written
//!                        in place; its length is the oversize bound.
//!   fn submitReply(*T, SlotRef, codec.Reply) void
//!                      — THE commit: `header ‖ out ‖ body`.
//!   fn failEnt(*T, SlotRef, unique: u64, errno: linux.E) void
//!                      — the one non-reply exit: header-only `-errno`.
//!   fn releaseLease(*T, PayloadLease) void

const std = @import("std");
const linux = std.os.linux;
const assert = std.debug.assert;
const codec = @import("../codec.zig");
const abi = @import("../abi.zig");

/// Compile-time duck typing of a transport: a missing or mis-shaped member
/// is a compile error naming it.
pub fn assertTransport(comptime T: type) void {
    comptime {
        if (!@hasDecl(T, "SlotRef")) @compileError(@typeName(T) ++ " lacks `SlotRef`");
        if (!@hasDecl(T, "PayloadLease")) @compileError(@typeName(T) ++ " lacks `PayloadLease`");
        for ([_][]const u8{ "replyWindow", "submitReply", "failEnt", "releaseLease" }) |name| {
            if (!@hasDecl(T, name)) @compileError(@typeName(T) ++ " lacks `" ++ name ++ "`");
        }
        const Window = @TypeOf(@as(*T, undefined).replyWindow(@as(T.SlotRef, undefined)));
        if (Window != []u8) @compileError(@typeName(T) ++ ".replyWindow must return []u8");
    }
}

/// The in-memory transport: every slot captures its LAST reply and counts
/// how many it received, so the exactly-one law is an assertable number
/// (`replyCount(slot) == 1`) and a reply's bytes are inspectable without a
/// kernel. Slots are independent: concurrent dispatches on distinct slots
/// never share state beyond the lease counter.
pub const Loopback = struct {
    pub const SlotRef = struct { idx: u32 };
    pub const PayloadLease = struct { idx: u32 };

    /// One captured reply: the header, the out-struct bytes and an owned
    /// copy of the body view (the handler's window is reused by the next
    /// op on the slot).
    pub const Captured = struct {
        header: abi.fuse_out_header,
        out: codec.OutBuf,
        out_len: usize,
        body: []u8,

        pub fn errno(self: Captured) ?linux.E {
            if (self.header.@"error" == 0) return null;
            return @fromBackingInt(@intCast(@as(u16, @intCast(-self.header.@"error"))));
        }

        pub fn outBytes(self: *const Captured) []const u8 {
            return self.out[0..self.out_len];
        }
    };

    const Slot = struct {
        window: []u8,
        replies: std.atomic.Value(u32) = .init(0),
        last: ?Captured = null,
    };

    gpa: std.mem.Allocator,
    slots: []Slot,
    leases_released: std.atomic.Value(u64) = .init(0),

    /// `window_bytes` plays the ent payload size (the oversize bound).
    pub fn init(gpa: std.mem.Allocator, nslots: u32, window_bytes: usize) std.mem.Allocator.Error!Loopback {
        const slots = try gpa.alloc(Slot, nslots);
        errdefer gpa.free(slots);
        var built: usize = 0;
        errdefer for (slots[0..built]) |s| gpa.free(s.window);
        for (slots) |*s| {
            s.* = .{ .window = try gpa.alloc(u8, window_bytes) };
            built += 1;
        }
        return .{ .gpa = gpa, .slots = slots };
    }

    pub fn deinit(self: *Loopback) void {
        for (self.slots) |*s| {
            if (s.last) |c| self.gpa.free(c.body);
            self.gpa.free(s.window);
        }
        self.gpa.free(self.slots);
        self.* = undefined;
    }

    pub fn slot(idx: u32) SlotRef {
        return .{ .idx = idx };
    }

    pub fn replyWindow(self: *Loopback, s: SlotRef) []u8 {
        return self.slots[s.idx].window;
    }

    pub fn submitReply(self: *Loopback, s: SlotRef, reply: codec.Reply) void {
        self.capture(s, reply);
    }

    pub fn failEnt(self: *Loopback, s: SlotRef, unique: u64, errno: linux.E) void {
        self.capture(s, codec.replyError(unique, errno));
    }

    pub fn releaseLease(self: *Loopback, _: PayloadLease) void {
        _ = self.leases_released.fetchAdd(1, .monotonic);
    }

    fn capture(self: *Loopback, s: SlotRef, reply: codec.Reply) void {
        const sl = &self.slots[s.idx];
        _ = sl.replies.fetchAdd(1, .monotonic);
        if (sl.last) |c| self.gpa.free(c.body);
        var out: codec.OutBuf = undefined;
        @memcpy(out[0..reply.out.len], reply.out);
        const body = self.gpa.dupe(u8, reply.body) catch &.{};
        sl.last = .{ .header = reply.header, .out = out, .out_len = reply.out.len, .body = @constCast(body) };
    }

    /// Replies this slot received (the exactly-one law reads 1).
    pub fn replyCount(self: *const Loopback, s: SlotRef) u32 {
        return self.slots[s.idx].replies.load(.monotonic);
    }

    pub fn captured(self: *const Loopback, s: SlotRef) ?Captured {
        return self.slots[s.idx].last;
    }

    /// The captured out-struct decoded as `T` (a test's reply inspection).
    pub fn outAs(self: *const Loopback, s: SlotRef, comptime T: type) ?T {
        const c = self.captured(s) orelse return null;
        return abi.decodeExact(T, c.outBytes());
    }

    pub fn leasesReleased(self: *const Loopback) u64 {
        return self.leases_released.load(.monotonic);
    }
};

comptime {
    assertTransport(Loopback);
}

const testing = std.testing;

test "loopback: a slot captures its last reply with the header, out-struct and an owned body copy, and counts every reply" {
    var lb = try Loopback.init(testing.allocator, 2, 64);
    defer lb.deinit();
    const s = Loopback.slot(1);
    try testing.expectEqual(@as(u32, 0), lb.replyCount(s));
    var out: codec.OutBuf = undefined;
    lb.submitReply(s, codec.replyWrite(&out, 77, 4096));
    try testing.expectEqual(@as(u32, 1), lb.replyCount(s));
    const w = lb.outAs(s, abi.fuse_write_out).?;
    try testing.expectEqual(@as(u32, 4096), w.size);
    try testing.expectEqual(@as(u64, 77), lb.captured(s).?.header.unique);
    try testing.expectEqual(@as(?linux.E, null), lb.captured(s).?.errno());
    // A body view into the window survives the window's reuse.
    const win = lb.replyWindow(s);
    @memcpy(win[0..3], "abc");
    lb.submitReply(s, codec.replyData(78, win[0..3]));
    @memset(win[0..3], 0);
    try testing.expectEqualStrings("abc", lb.captured(s).?.body);
    try testing.expectEqual(@as(u32, 2), lb.replyCount(s));
    lb.failEnt(s, 79, .IO);
    try testing.expectEqual(linux.E.IO, lb.captured(s).?.errno().?);
    try testing.expectEqual(@as(u32, 0), lb.replyCount(Loopback.slot(0)));
    lb.releaseLease(.{ .idx = 1 });
    try testing.expectEqual(@as(u64, 1), lb.leasesReleased());
}
