//! The per-worker commit message path (charter §4 "Reply submission";
//! `docs/porting/fuse_uring_transport.md` §7; decision 0002 ledger Q5): a
//! reply or a synthesized failure travels from the handler's thread to the
//! drain-group worker as ONE preallocated cell — never a heap allocation on
//! an unbounded channel.
//!
//! The bound is structural: a reply exists only for a delivered slot and a
//! slot owes at most one reply between two commits, so there is exactly ONE
//! cell per group ent (`gent = member_slot × depth + ent_idx`). A submitter
//! claims the slot's cell (`free → filling`), fills it, publishes it
//! (`published`, release) and pushes the index onto the bounded MPMC the
//! worker pops; the worker frees the cell after it applied or refused the
//! message. A second message for a slot whose cell is not free is refused
//! HERE (`busy` → `transport_replies_refused_stale`): the slot table would
//! refuse it anyway, and a double reply can never consume another slot's
//! cell. The MPMC's power-of-two rounding is the only slack.
//!
//! Publication order is the coalescer's producer law: the push is the
//! publication, then `arm`, then the eventfd write iff arm returned true —
//! N publications between two worker passes cost one write
//! (`transport_wake_{writes,elided}`).
//!
//! Lifetime of a reply BODY: the out-struct is copied into the cell (144 B
//! at most); the body is a VIEW the worker reads when it commits. It must
//! be either a slice of the slot's payload window (the serve-into-payload
//! path: the handler wrote the bytes where the kernel reads them and the
//! copy is elided) or memory that outlives the session. A handler that owns
//! its bytes on a stack serves them into the window.

const std = @import("std");
const linux = std.os.linux;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const slot = @import("slot.zig");
const mpmc = @import("../../sync/mpmc.zig");
const counters = @import("../../core/counters.zig");

/// `transport_commit_cells_busy`: a message refused because the slot's
/// cell was already claimed (a double or late reply) — the early face of
/// `transport_replies_refused_stale`.
pub var cells_busy: counters.ShardedCounter = .{};
/// `transport_commit_queue_overflow` (must-stay-0): the index ring refused
/// a push — impossible by construction (one index per cell).
pub var queue_overflow: counters.Tripwire = .init("transport_commit_queue_overflow");
/// `transport_fd_body_fallbacks`: `submitReplyFdBody` on a queue without a
/// zero-copy slot table — the body view was applied, the fd ignored.
pub var fd_body_fallbacks: counters.ShardedCounter = .{};

pub const Kind = enum(u8) {
    /// `header ‖ out ‖ body` applied into the ent.
    reply,
    /// Like `reply`, with an `(fd, off)` source a zc worker bridges through
    /// `READ_FIXED(fd → slot)`; on a UserEnts queue the body view stands in.
    fd_body,
    /// Header only, `payload_sz = n`: the bytes already landed in the slot
    /// (the direct device leg, or a handler that served into the window).
    prefilled,
    /// `fail_ent`: a header-only `-errno`.
    fail,
    /// Like `reply`, committed with `FUSE_URING_COMMIT_RETAIN`: the request
    /// ends (write(2) returns) while the slot's pages stay registered until
    /// a `release` — the ACK-early accelerator. Refused at the worker (and
    /// committed plain) unless the slot holds a `.page_cache` WRITE on a
    /// retention-armed queue.
    reply_retain,
    /// `RELEASE_PAYLOAD` for a retained slot; not a reply.
    release,
};

pub const Msg = struct {
    kind: Kind,
    slot: slot.SlotRef,
    header: abi.fuse_out_header,
    out: [codec.MAX_OUT_LEN]u8 = undefined,
    out_len: u8 = 0,
    body: []const u8 = &.{},
    fd: linux.fd_t = -1,
    fd_off: u64 = 0,
    prefilled: u32 = 0,
    /// `codec.Reply.zero_pad`: the bounce bridge's length for a short paged READ.
    zero_pad: u32 = 0,
    errno: linux.E = .SUCCESS,

    pub fn reply(s: slot.SlotRef, r: codec.Reply) Msg {
        std.debug.assert(r.out.len <= codec.MAX_OUT_LEN);
        var m: Msg = .{ .kind = .reply, .slot = s, .header = r.header, .out_len = @intCast(r.out.len), .body = r.body, .zero_pad = r.zero_pad };
        @memcpy(m.out[0..r.out.len], r.out);
        return m;
    }

    pub fn fdBody(s: slot.SlotRef, r: codec.Reply, fd: linux.fd_t, off: u64) Msg {
        var m = reply(s, r);
        m.kind = .fd_body;
        m.fd = fd;
        m.fd_off = off;
        return m;
    }

    pub fn prefilledMsg(s: slot.SlotRef, unique: u64, n: u32) Msg {
        return .{ .kind = .prefilled, .slot = s, .header = codec.okHeader(unique, n), .prefilled = n };
    }

    pub fn fail(s: slot.SlotRef, unique: u64, errno: linux.E) Msg {
        return .{ .kind = .fail, .slot = s, .header = codec.errorHeader(unique, errno), .errno = errno };
    }

    pub fn replyRetain(s: slot.SlotRef, r: codec.Reply) Msg {
        var m = reply(s, r);
        m.kind = .reply_retain;
        return m;
    }

    pub fn releaseMsg(s: slot.SlotRef) Msg {
        return .{ .kind = .release, .slot = s, .header = codec.okHeader(0, 0) };
    }

    /// The reply the worker applies (the out view points into the cell).
    pub fn asReply(self: *const Msg) codec.Reply {
        return .{ .header = self.header, .out = self.out[0..self.out_len], .body = self.body, .zero_pad = self.zero_pad };
    }
};

pub const Submit = enum { published, busy };

const CellState = enum(u8) { free, filling, published };

pub const Cell = struct {
    state: std.atomic.Value(u8) = .init(@backingInt(CellState.free)),
    msg: Msg = undefined,
};

pub const Queue = struct {
    cells: []Cell,
    ready: mpmc.BoundedMpmc(u32),
    waker: slot.Waker,

    pub fn init(gpa: std.mem.Allocator, slots: usize, waker: slot.Waker) std.mem.Allocator.Error!Queue {
        const cells = try gpa.alloc(Cell, slots);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .{};
        return .{ .cells = cells, .ready = try mpmc.BoundedMpmc(u32).init(gpa, slots), .waker = waker };
    }

    pub fn deinit(self: *Queue, gpa: std.mem.Allocator) void {
        self.ready.deinit(gpa);
        gpa.free(self.cells);
        self.* = undefined;
    }

    /// Any thread: claim the slot's cell, fill, publish, wake. `busy` when
    /// the cell is already claimed (counted; the caller drops the message —
    /// the slot table owes nothing to a second reply).
    pub fn submit(self: *Queue, gent: u32, m: Msg) Submit {
        const cell = &self.cells[gent];
        // acquire pairs with the worker's release of `free`: the previous
        // occupant's bytes are dead before this fill.
        if (cell.state.cmpxchgStrong(@backingInt(CellState.free), @backingInt(CellState.filling), .acquire, .monotonic) != null) {
            cells_busy.inc();
            slot.replies_refused_stale.inc();
            return .busy;
        }
        cell.msg = m;
        // release pairs with the worker's acquire load after `tryPop`.
        cell.state.store(@backingInt(CellState.published), .release);
        if (!self.ready.tryPush(gent)) {
            queue_overflow.fire("commit index ring refused a push");
            cell.state.store(@backingInt(CellState.free), .release);
            return .busy;
        }
        // THE wake: publish → arm → write iff arm returned true.
        if (self.waker.call()) slot.wake_writes.inc() else slot.wakes_elided.inc();
        return .published;
    }

    /// Worker: the next published cell's index, or null.
    pub fn pop(self: *Queue) ?u32 {
        const gent = self.ready.tryPop() orelse return null;
        // acquire pairs with the submitter's release publish.
        std.debug.assert(self.cells[gent].state.load(.acquire) == @backingInt(CellState.published));
        return gent;
    }

    pub fn msg(self: *Queue, gent: u32) *const Msg {
        return &self.cells[gent].msg;
    }

    /// Worker: the message was applied or refused; the cell may be claimed
    /// again (release pairs with the next submitter's acquire CAS).
    pub fn recycle(self: *Queue, gent: u32) void {
        self.cells[gent].state.store(@backingInt(CellState.free), .release);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const FakeWaker = struct {
    armed: bool = false,
    writes: u32 = 0,
    fn wake(ctx: ?*anyopaque) bool {
        const self: *FakeWaker = @ptrCast(@alignCast(ctx.?));
        if (self.armed) return false;
        self.armed = true;
        self.writes += 1;
        return true;
    }
    fn waker(self: *FakeWaker) slot.Waker {
        return .{ .ctx = self, .wake = FakeWaker.wake };
    }
    fn disarm(self: *FakeWaker) void {
        self.armed = false;
    }
};

test "cells are preallocated and the path allocates nothing after init (a failing allocator past the two init allocations); messages round-trip FIFO with the out-struct copied and the body a view" {
    var failing = testing.FailingAllocator.init(testing.allocator, .{ .fail_index = 2 });
    var fw: FakeWaker = .{};
    var q = try Queue.init(failing.allocator(), 4, fw.waker());
    defer q.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 2), failing.allocations);
    var out: codec.OutBuf = undefined;
    const body = "payload-view";
    const r = codec.replyWrite(&out, 10, 7);
    var m = Msg.reply(.{ .qid = 1, .ent_idx = 2, .commit_id = 10 }, r);
    m.body = body;
    try testing.expectEqual(Submit.published, q.submit(2, m));
    // The out bytes live in the cell: scribbling the caller's buffer changes nothing.
    @memset(&out, 0xFF);
    try testing.expectEqual(Submit.published, q.submit(0, Msg.fail(.{ .qid = 1, .ent_idx = 0, .commit_id = 12 }, 12, .IO)));
    try testing.expectEqual(@as(?u32, 2), q.pop());
    const got = q.msg(2);
    try testing.expectEqual(Kind.reply, got.kind);
    try testing.expectEqual(@as(u64, 10), got.header.unique);
    try testing.expectEqual(@as(u8, 8), got.out_len);
    try testing.expectEqual(@as(u32, 7), std.mem.readInt(u32, got.out[0..4], .little));
    try testing.expectEqual(body.ptr, got.body.ptr);
    const as_reply = got.asReply();
    try testing.expectEqual(@as(usize, 8 + body.len), as_reply.extraLen());
    q.recycle(2);
    try testing.expectEqual(@as(?u32, 0), q.pop());
    try testing.expectEqual(Kind.fail, q.msg(0).kind);
    try testing.expectEqual(-@as(i32, @intCast(@backingInt(linux.E.IO))), q.msg(0).header.@"error");
    q.recycle(0);
    try testing.expectEqual(@as(?u32, null), q.pop());
    try testing.expectEqual(@as(usize, 2), failing.allocations);
}

test "the bound is per slot: a second message for a claimed cell is busy (counted stale) and never consumes another slot's cell; the cell is claimable again after recycle" {
    var fw: FakeWaker = .{};
    var q = try Queue.init(testing.allocator, 2, fw.waker());
    defer q.deinit(testing.allocator);
    const s: slot.SlotRef = .{ .qid = 0, .ent_idx = 1, .commit_id = 4 };
    const busy_before = cells_busy.load();
    const stale_before = slot.replies_refused_stale.load();
    try testing.expectEqual(Submit.published, q.submit(1, Msg.fail(s, 4, .IO)));
    try testing.expectEqual(Submit.busy, q.submit(1, Msg.fail(s, 4, .NOTCONN)));
    try testing.expectEqual(busy_before + 1, cells_busy.load());
    try testing.expectEqual(stale_before + 1, slot.replies_refused_stale.load());
    // The other slot is untouched.
    try testing.expectEqual(Submit.published, q.submit(0, Msg.prefilledMsg(.{ .qid = 0, .ent_idx = 0, .commit_id = 6 }, 6, 4096)));
    try testing.expectEqual(@as(?u32, 1), q.pop());
    try testing.expectEqual(linux.E.IO, q.msg(1).errno);
    q.recycle(1);
    try testing.expectEqual(Submit.published, q.submit(1, Msg.fail(s, 4, .NOTCONN)));
    try testing.expectEqual(@as(?u32, 0), q.pop());
    try testing.expectEqual(@as(u32, 4096), q.msg(0).prefilled);
    try testing.expectEqual(@as(?u32, 1), q.pop());
    try testing.expectEqual(@as(?u32, null), q.pop());
}

test "the wake coalesces: 8 publications between two worker passes pay ONE eventfd write (7 elided); after the worker disarms the next publication pays again" {
    var fw: FakeWaker = .{};
    var q = try Queue.init(testing.allocator, 8, fw.waker());
    defer q.deinit(testing.allocator);
    const writes = slot.wake_writes.load();
    const elided = slot.wakes_elided.load();
    var i: u32 = 0;
    while (i < 8) : (i += 1) {
        try testing.expectEqual(Submit.published, q.submit(i, Msg.fail(.{ .qid = 0, .ent_idx = @intCast(i), .commit_id = i + 2 }, i + 2, .IO)));
    }
    try testing.expectEqual(@as(u32, 1), fw.writes);
    try testing.expectEqual(writes + 1, slot.wake_writes.load());
    try testing.expectEqual(elided + 7, slot.wakes_elided.load());
    // The worker's pass: drain → disarm → scan.
    fw.disarm();
    i = 0;
    while (q.pop()) |g| : (i += 1) q.recycle(g);
    try testing.expectEqual(@as(u32, 8), i);
    try testing.expectEqual(Submit.published, q.submit(3, Msg.fail(.{ .qid = 0, .ent_idx = 3, .commit_id = 40 }, 40, .IO)));
    try testing.expectEqual(@as(u32, 2), fw.writes);
}

test "fd-body and prefilled messages carry their legs; a fail message carries its errno and a 16-byte header" {
    const s: slot.SlotRef = .{ .qid = 5, .ent_idx = 6, .commit_id = 44 };
    const m = Msg.fdBody(s, codec.replyData(44, "x"), 9, 4096);
    try testing.expectEqual(Kind.fd_body, m.kind);
    try testing.expectEqual(@as(linux.fd_t, 9), m.fd);
    try testing.expectEqual(@as(u64, 4096), m.fd_off);
    try testing.expectEqual(@as(u8, 0), m.out_len);
    const p = Msg.prefilledMsg(s, 44, 123);
    try testing.expectEqual(@as(u32, 16 + 123), p.header.len);
    const f = Msg.fail(s, 44, .NOTCONN);
    try testing.expectEqual(@as(u32, 16), f.header.len);
    try testing.expectEqual(linux.E.NOTCONN, f.errno);
    var out: codec.OutBuf = undefined;
    const rt = Msg.replyRetain(s, codec.replyWrite(&out, 44, 4096));
    try testing.expectEqual(Kind.reply_retain, rt.kind);
    try testing.expectEqual(@as(u8, 8), rt.out_len);
    try testing.expectEqual(@as(u32, 4096), std.mem.readInt(u32, rt.out[0..4], .little));
    const rel = Msg.releaseMsg(s);
    try testing.expectEqual(Kind.release, rel.kind);
    try testing.expectEqual(@as(u64, 44), rel.slot.commit_id);
}
