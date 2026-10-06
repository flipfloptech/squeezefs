//! The `Filesystem` face the transport calls and the reply handle a
//! handler answers through (`docs/porting/fuse_handlers_lanes.md` §1, §5;
//! charter §4 "READ dispatch law", §9). Portable; the lifecycle agent's
//! real filesystem is built against this SAME shape.
//!
//! The exactly-one-reply obligation is a VALUE: `Handle{slot, unique}` (a
//! ring slot by `(qid, ent_idx, commit_id)` or the classical venue by
//! `unique`) plus the transport's `Tx` face. A handler calls exactly one of
//! `reply`, `replyFdBody`, `replyPrefilled` or `fail`; a second call is
//! refused by the slot table (never applied), a missing call is caught by
//! the teardown drain's `fail_ent`. No drop guards, no unwinding (decision
//! 0002 rows 9 and Q3): a handler that cannot answer says so with `fail`.
//!
//! Request views: `Request.op`'s slices (names, xattr values, the FORGET
//! array, WRITE data) point into the ent's header/payload regions, which
//! the kernel does not touch between delivery and the COMMIT this handler
//! itself triggers — they are valid until the reply is submitted. WRITE
//! data additionally rides a `PayloadLease` the handler releases exactly
//! once (the commit gate parks a reply submitted while it is live).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const slot = @import("slot.zig");
const geometry = @import("../geometry.zig");
const errors = @import("../../core/errors.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");

/// Where a reply goes: a ring slot by value, or the classical sideband
/// (the request arrived through `/dev/fuse` and is answered by a device write).
pub const ReplySlot = union(enum) {
    ring: slot.SlotRef,
    classical,
};

/// A decoded request with its identity.
pub const Request = struct {
    header: abi.fuse_in_header,
    op: codec.Request,

    pub fn unique(self: Request) u64 {
        return self.header.unique;
    }
    pub fn nodeid(self: Request) u64 {
        return self.header.nodeid;
    }
};

/// The transport face (type-erased so this module stays portable): the
/// session fills it; a handle carries a pointer to it.
pub const Tx = struct {
    ctx: *anyopaque,
    submit: *const fn (ctx: *anyopaque, h: Handle, r: codec.Reply) void,
    submit_fd_body: *const fn (ctx: *anyopaque, h: Handle, r: codec.Reply, fd: linux.fd_t, off: u64) void,
    submit_prefilled: *const fn (ctx: *anyopaque, h: Handle, n: u32) void,
    fail: *const fn (ctx: *anyopaque, h: Handle, errno: linux.E) void,
};

/// The reply obligation by value.
pub const Handle = struct {
    tx: *const Tx,
    slot: ReplySlot,
    unique: u64,
    /// The slot's payload window on the ring (empty on the classical
    /// venue): a handler may serve a body INTO it so the commit's copy is
    /// elided; the window is writable only while no lease is live over it.
    window: []u8,

    pub fn reply(self: Handle, r: codec.Reply) void {
        self.tx.submit(self.tx.ctx, self, r);
    }

    /// The zc fd→slot bridge (`READ_FIXED(fd @ off → slot)`); the body is
    /// the keepalive and the fallback on a UserEnts queue.
    pub fn replyFdBody(self: Handle, r: codec.Reply, fd: linux.fd_t, off: u64) void {
        self.tx.submit_fd_body(self.tx.ctx, self, r, fd, off);
    }

    /// Header only: `n` bytes already sit in the slot's payload.
    pub fn replyPrefilled(self: Handle, n: u32) void {
        self.tx.submit_prefilled(self.tx.ctx, self, n);
    }

    /// Header-only `-errno`.
    pub fn fail(self: Handle, errno: linux.E) void {
        self.tx.fail(self.tx.ctx, self, errno);
    }

    /// A typed error through the ONE errno table (`core/errors.zig`).
    pub fn failWith(self: Handle, err: errors.Error) void {
        self.fail(errors.toErrno(err));
    }
};

/// What `init` tells the filesystem after INIT negotiated.
pub const InitCaps = struct {
    /// The folded capability word the kernel will apply.
    negotiated: u64,
    minor: u32,
    plan: geometry.Plan,
    /// The executor handle handlers may park on (`std.Io` futex ops park
    /// the fiber, never the lane thread).
    io: std.Io,
};

pub const Filesystem = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// After INIT negotiated, before the session is ready.
        init: *const fn (ctx: *anyopaque, caps: InitCaps) void,
        /// DESTROY arrived, or the session is torn down (once).
        destroy: *const fn (ctx: *anyopaque) void,
        /// On a handler lane homed on the request's queue CPU (or the
        /// sideband's lane for a classical straggler): answer `handle`
        /// exactly once; release `lease` exactly once when present.
        dispatch: *const fn (ctx: *anyopaque, req: Request, handle: Handle, lease: ?slot.PayloadLease) void,
        /// The READ dispatch law's arm (a), ON THE WORKER THREAD at the
        /// delivery CQE: SYNC, try-only, non-blocking (a contended lock is
        /// a decline, never a spin; never a device touch; never an
        /// allocation). Serve into `window` and return the reply, or null to
        /// decline (the request then takes arm (c)).
        probe_read: *const fn (ctx: *anyopaque, req: Request, window: []u8) ?codec.Reply,
        /// FORGET / BATCH_FORGET nlookup hand-off, on the worker or sideband
        /// thread before the auto-commit: cheap and non-blocking. The
        /// lifecycle layer owns the open-count law (`open_count_stranded`:
        /// a final FORGET with open handles = a lost RELEASE) behind it.
        forget: *const fn (ctx: *anyopaque, ino: u64, nlookup: u64) void,
    };

    pub fn init(self: Filesystem, caps: InitCaps) void {
        self.vtable.init(self.ctx, caps);
    }
    pub fn destroy(self: Filesystem) void {
        self.vtable.destroy(self.ctx);
    }
    pub fn dispatch(self: Filesystem, req: Request, handle: Handle, lease: ?slot.PayloadLease) void {
        self.vtable.dispatch(self.ctx, req, handle, lease);
    }
    pub fn probeRead(self: Filesystem, req: Request, window: []u8) ?codec.Reply {
        return self.vtable.probe_read(self.ctx, req, window);
    }
    pub fn forget(self: Filesystem, ino: u64, nlookup: u64) void {
        self.vtable.forget(self.ctx, ino, nlookup);
    }
};

// ---------------------------------------------------------------------------
// EchoFs: the test filesystem — a fixed tree served from static bytes.
// Test binaries only (referencing it from product code fails to compile).
// ---------------------------------------------------------------------------

pub const EchoFs = if (builtin.is_test) EchoFsImpl else @compileError("EchoFs is a test filesystem");

const EchoFsImpl = struct {
    const Self = @This();

    /// The tree: `/hello` (static text), `/big` (a 1 MiB pattern, several
    /// READs per file), `/park` (reads park until `release_parked`), `/scratch`
    /// (accepts WRITEs; the handler may hold the lease across its reply).
    pub const ino_root: u64 = abi.FUSE_ROOT_ID;
    pub const ino_hello: u64 = 2;
    pub const ino_big: u64 = 3;
    pub const ino_park: u64 = 4;
    pub const ino_scratch: u64 = 5;
    pub const hello_text = "hello world\n";
    pub const big_len: usize = 1 << 20;
    pub const scratch_cap: usize = 1 << 16;
    pub const names = [_][]const u8{ "hello", "big", "park", "scratch" };
    pub const inos = [_]u64{ ino_hello, ino_big, ino_park, ino_scratch };

    /// Every READ declines the probe (arm (a)) and takes the lane (arm (c)).
    probe_declines: bool = false,
    /// A WRITE handler replies BEFORE releasing its lease and waits until
    /// the worker parked the commit (the gate's Parked arm, observable).
    hold_lease_across_reply: bool = false,
    io: ?std.Io = null,
    caps: ?InitCaps = null,
    inited: counters.Gauge = .{},
    destroyed: counters.Gauge = .{},
    /// `/park` readers wait on this (set from the test); handlers park as
    /// fibers through the `Io` futex, never the lane thread.
    park_gate: std.Io.Event = .unset,
    parked_readers: std.atomic.Value(u32) = .init(0),
    /// nlookup ledger: entry replies +1, FORGET −n.
    lookups: [8]std.atomic.Value(u64) = @splat(.init(0)),
    forgets: std.atomic.Value(u64) = .init(0),
    forget_nlookup: std.atomic.Value(u64) = .init(0),
    dispatches: std.atomic.Value(u64) = .init(0),
    probes_served: std.atomic.Value(u64) = .init(0),
    writes_seen: std.atomic.Value(u64) = .init(0),
    write_bytes: std.atomic.Value(u64) = .init(0),
    lease_waits_parked: std.atomic.Value(u64) = .init(0),
    scratch: [scratch_cap]u8 = @splat(0),
    scratch_len: std.atomic.Value(u64) = .init(0),

    pub const vtable: Filesystem.VTable = .{
        .init = fsInit,
        .destroy = fsDestroy,
        .dispatch = fsDispatch,
        .probe_read = fsProbeRead,
        .forget = fsForget,
    };

    pub fn filesystem(self: *Self) Filesystem {
        return .{ .ctx = self, .vtable = &vtable };
    }

    fn of(ctx: *anyopaque) *Self {
        return @ptrCast(@alignCast(ctx));
    }

    fn fsInit(ctx: *anyopaque, caps: InitCaps) void {
        const self = of(ctx);
        self.caps = caps;
        self.io = caps.io;
        self.inited.add(1);
    }

    fn fsDestroy(ctx: *anyopaque) void {
        of(ctx).destroyed.add(1);
    }

    fn fsForget(ctx: *anyopaque, ino: u64, nlookup: u64) void {
        const self = of(ctx);
        _ = self.forgets.fetchAdd(1, .monotonic);
        _ = self.forget_nlookup.fetchAdd(nlookup, .monotonic);
        if (ino < self.lookups.len) {
            const cur = self.lookups[@intCast(ino)].load(.monotonic);
            self.lookups[@intCast(ino)].store(cur -| @max(nlookup, 1), .monotonic);
        }
    }

    /// The pattern byte of `/big` at `off`.
    pub fn bigByte(off: usize) u8 {
        return @truncate((off * 7 + (off >> 12)) & 0xFF);
    }

    fn fileLen(self: *const Self, ino: u64) ?u64 {
        return switch (ino) {
            ino_hello => hello_text.len,
            ino_big => big_len,
            ino_park => hello_text.len,
            ino_scratch => self.scratch_len.load(.acquire),
            else => null,
        };
    }

    fn attrOf(self: *const Self, ino: u64) ?abi.fuse_attr {
        const is_dir = ino == ino_root;
        const len = if (is_dir) 0 else (self.fileLen(ino) orelse return null);
        const mode: u32 = if (is_dir) 0o40755 else if (ino == ino_scratch) 0o100644 else 0o100444;
        return .{
            .ino = ino,
            .size = len,
            .blocks = (len + 511) / 512,
            .atime = 1_700_000_000,
            .mtime = 1_700_000_000,
            .ctime = 1_700_000_000,
            .atimensec = 0,
            .mtimensec = 0,
            .ctimensec = 0,
            .mode = mode,
            .nlink = if (is_dir) 2 else 1,
            .uid = 0,
            .gid = 0,
            .rdev = 0,
            .blksize = 4096,
            .flags = 0,
        };
    }

    fn lookupName(name: []const u8) ?u64 {
        for (names, inos) |n, i| if (std.mem.eql(u8, n, name)) return i;
        return null;
    }

    fn entry(self: *Self, ino: u64) ?codec.EntryReply {
        const attr = self.attrOf(ino) orelse return null;
        if (ino < self.lookups.len and ino != ino_root) _ = self.lookups[@intCast(ino)].fetchAdd(1, .monotonic);
        return .{ .nodeid = ino, .entry_ttl_ns = std.time.ns_per_s, .attr_ttl_ns = std.time.ns_per_s, .attr = attr };
    }

    /// The bytes of `ino` at `[off, off + size)` served into `dst`.
    fn readInto(self: *const Self, ino: u64, off: u64, size: u32, dst: []u8) ?usize {
        const len = self.fileLen(ino) orelse return null;
        if (off >= len) return 0;
        const n: usize = @intCast(@min(@as(u64, size), @min(len - off, dst.len)));
        switch (ino) {
            ino_hello, ino_park => @memcpy(dst[0..n], hello_text[@intCast(off)..][0..n]),
            ino_big => for (dst[0..n], 0..) |*b, i| {
                b.* = bigByte(@intCast(off + i));
            },
            ino_scratch => @memcpy(dst[0..n], self.scratch[@intCast(off)..][0..n]),
            else => return null,
        }
        return n;
    }

    fn fsProbeRead(ctx: *anyopaque, req: Request, window: []u8) ?codec.Reply {
        const self = of(ctx);
        if (self.probe_declines) return null;
        const ino = req.nodeid();
        // `/park` always declines: its handler parks on the lane.
        if (ino == ino_park) return null;
        const in = req.op.read;
        const n = self.readInto(ino, in.offset, in.size, window) orelse return null;
        _ = self.probes_served.fetchAdd(1, .monotonic);
        return codec.replyData(req.unique(), window[0..n]);
    }

    fn fsDispatch(ctx: *anyopaque, req: Request, h: Handle, lease: ?slot.PayloadLease) void {
        const self = of(ctx);
        _ = self.dispatches.fetchAdd(1, .monotonic);
        var out: codec.OutBuf = undefined;
        const u = req.unique();
        const ino = req.nodeid();
        switch (req.op) {
            .lookup => |l| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                const child = lookupName(l.name) orelse return h.reply(codec.replyEntry(&out, u, codec.EntryReply.negative(std.time.ns_per_s)));
                h.reply(codec.replyEntry(&out, u, self.entry(child).?));
            },
            .getattr => {
                const attr = self.attrOf(ino) orelse return h.fail(.NOENT);
                h.reply(codec.replyAttr(&out, u, .{ .attr_ttl_ns = std.time.ns_per_s, .attr = attr }));
            },
            .open => {
                if (self.attrOf(ino) == null) return h.fail(.NOENT);
                // DIRECT_IO: every read and write reaches the daemon.
                h.reply(codec.replyOpen(&out, u, ino, abi.FOPEN_DIRECT_IO));
            },
            .opendir => h.reply(codec.replyOpen(&out, u, ino, 0)),
            .release, .releasedir, .flush, .fsync, .fsyncdir => h.reply(codec.replyOk(u)),
            .read => |in| {
                if (ino == ino_park) {
                    _ = self.parked_readers.fetchAdd(1, .monotonic);
                    self.park_gate.wait(self.io.?) catch {};
                    _ = self.parked_readers.fetchSub(1, .monotonic);
                }
                // Serve into the window when there is one (the ring);
                // static bytes on the classical venue.
                if (h.window.len > 0) {
                    const n = self.readInto(ino, in.offset, in.size, h.window) orelse return h.fail(.NOENT);
                    return h.reply(codec.replyData(u, h.window[0..n]));
                }
                if (ino == ino_hello or ino == ino_park) {
                    return h.reply(codec.replyRead(u, if (in.offset >= hello_text.len) "" else hello_text[@intCast(in.offset)..], in.size));
                }
                h.fail(.IO);
            },
            .write => |w| {
                _ = self.writes_seen.fetchAdd(1, .monotonic);
                const l = lease orelse return h.fail(.INVAL);
                defer l.release();
                if (ino != ino_scratch) return h.fail(.ROFS);
                const end = w.in.offset + w.in.size;
                if (end > scratch_cap) return h.fail(.FBIG);
                @memcpy(self.scratch[@intCast(w.in.offset)..][0..w.in.size], w.data);
                _ = self.scratch_len.fetchMax(end, .acq_rel);
                _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
                h.reply(codec.replyWrite(&out, u, w.in.size));
                if (self.hold_lease_across_reply) {
                    // Hold the lease until the worker's gate parked the
                    // commit (a bounded poll, never a sleep), so the
                    // Parked → Replied arm is exercised deterministically.
                    const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
                    while (!l.state.isParked() and clock.monotonicNs() < deadline) std.atomic.spinLoopHint();
                    if (l.state.isParked()) _ = self.lease_waits_parked.fetchAdd(1, .monotonic);
                }
            },
            .readdir => |in| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                var scratch: [4096]u8 = undefined;
                const buf: []u8 = if (h.window.len > 0) h.window else &scratch;
                var w = codec.DirentWriter.init(buf, in.size);
                var cookie: u64 = in.offset;
                while (cookie < names.len) : (cookie += 1) {
                    w.add(inos[@intCast(cookie)], cookie + 1, 0o100444, names[@intCast(cookie)]) catch break;
                }
                // A classical reply is written before `reply` returns, so
                // the stack scratch is a legal body there.
                h.reply(codec.replyData(u, w.bytes()));
            },
            .readdirplus => |in| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                var scratch: [4096]u8 = undefined;
                const buf: []u8 = if (h.window.len > 0) h.window else &scratch;
                var w = codec.DirentPlusWriter.init(buf, in.size);
                var cookie: u64 = in.offset;
                while (cookie < names.len) : (cookie += 1) {
                    const e = self.entry(inos[@intCast(cookie)]).?;
                    w.add(e, cookie + 1, names[@intCast(cookie)]) catch {
                        // Not emitted: hand the lookup reference back.
                        fsForget(ctx, e.nodeid, 1);
                        break;
                    };
                }
                h.reply(codec.replyData(u, w.bytes()));
            },
            .statfs => h.reply(codec.replyStatfs(&out, u, .{ .blocks = 1024, .bfree = 512, .bavail = 512, .files = 8, .ffree = 2, .bsize = 4096, .namelen = 255, .frsize = 4096 })),
            .access => h.reply(codec.replyOk(u)),
            .setattr => {
                const attr = self.attrOf(ino) orelse return h.fail(.NOENT);
                h.reply(codec.replyAttr(&out, u, .{ .attr_ttl_ns = std.time.ns_per_s, .attr = attr }));
            },
            .forget, .batch_forget, .interrupt, .notify_reply => {},
            .unsupported => h.reply(codec.replyEnosys(u)),
            else => h.reply(codec.replyEnosys(u)),
        }
    }

    /// Release every parked `/park` reader.
    pub fn releaseParked(self: *Self) void {
        self.park_gate.set(self.io.?);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const RecordingTx = struct {
    replies: u32 = 0,
    fails: u32 = 0,
    prefilled: u32 = 0,
    fd_bodies: u32 = 0,
    last_errno: linux.E = .SUCCESS,
    last_len: u32 = 0,
    last_body: []const u8 = &.{},

    fn submit(ctx: *anyopaque, _: Handle, r: codec.Reply) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
        self.last_len = r.header.len;
        self.last_body = r.body;
    }
    fn submitFdBody(ctx: *anyopaque, _: Handle, _: codec.Reply, _: linux.fd_t, _: u64) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.fd_bodies += 1;
    }
    fn submitPrefilled(ctx: *anyopaque, _: Handle, n: u32) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.prefilled += 1;
        self.last_len = n;
    }
    fn fail(ctx: *anyopaque, _: Handle, errno: linux.E) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.fails += 1;
        self.last_errno = errno;
    }
    fn tx(self: *RecordingTx) Tx {
        return .{ .ctx = self, .submit = submit, .submit_fd_body = submitFdBody, .submit_prefilled = submitPrefilled, .fail = fail };
    }
};

fn requestOf(op: codec.Request, unique: u64, nodeid: u64) Request {
    return .{ .header = .{ .len = 40, .opcode = 0, .unique = unique, .nodeid = nodeid, .uid = 0, .gid = 0, .pid = 1, .total_extlen = 0, .padding = 0 }, .op = op };
}

test "the handle routes each reply kind through the Tx face; failWith maps a typed error through the ONE table (EAGAIN for backpressure, EIO for a dead ring)" {
    var rec: RecordingTx = .{};
    const t = rec.tx();
    const h: Handle = .{ .tx = &t, .slot = .classical, .unique = 8, .window = &.{} };
    h.reply(codec.replyOk(8));
    h.replyPrefilled(77);
    h.replyFdBody(codec.replyData(8, "x"), 3, 0);
    h.fail(.NOENT);
    h.failWith(error.QueueFull);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 1), rec.prefilled);
    try testing.expectEqual(@as(u32, 1), rec.fd_bodies);
    try testing.expectEqual(@as(u32, 2), rec.fails);
    try testing.expectEqual(linux.E.AGAIN, rec.last_errno);
    h.failWith(error.RingFailed);
    try testing.expectEqual(linux.E.IO, rec.last_errno);
    h.failWith(error.FileNotFound);
    try testing.expectEqual(linux.E.NOENT, rec.last_errno);
    try testing.expectEqual(@as(u32, 4), rec.fails);
}

test "EchoFs: lookup/getattr/read on the fixed tree answer exactly once each; a miss is a negative entry; the probe serves hello and big into the window and declines park; forget returns lookups" {
    var fs: EchoFs = .{};
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var window: [4096]u8 = undefined;
    const f = fs.filesystem();
    f.dispatch(requestOf(.{ .lookup = .{ .name = "hello" } }, 2, EchoFs.ino_root), .{ .tx = &t, .slot = .classical, .unique = 2, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 16 + 128), rec.last_len);
    try testing.expectEqual(@as(u64, 1), fs.lookups[EchoFs.ino_hello].load(.monotonic));
    f.dispatch(requestOf(.{ .lookup = .{ .name = "nope" } }, 4, EchoFs.ino_root), .{ .tx = &t, .slot = .classical, .unique = 4, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 2), rec.replies);
    f.dispatch(requestOf(.{ .getattr = .{ .flags = 0, .fh = null } }, 6, EchoFs.ino_big), .{ .tx = &t, .slot = .classical, .unique = 6, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 3), rec.replies);
    try testing.expectEqual(@as(u32, 16 + 104), rec.last_len);
    f.dispatch(requestOf(.{ .getattr = .{ .flags = 0, .fh = null } }, 8, 99), .{ .tx = &t, .slot = .classical, .unique = 8, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 1), rec.fails);
    try testing.expectEqual(linux.E.NOENT, rec.last_errno);
    // READ through the lane arm into a window.
    const rd: abi.fuse_read_in = .{ .fh = 2, .offset = 6, .size = 100, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    f.dispatch(requestOf(.{ .read = rd }, 10, EchoFs.ino_hello), .{ .tx = &t, .slot = .{ .ring = .{ .qid = 0, .ent_idx = 0, .commit_id = 10 } }, .unique = 10, .window = &window }, null);
    try testing.expectEqualSlices(u8, "world\n", rec.last_body);
    try testing.expectEqual(@as([*]const u8, &window), rec.last_body.ptr);
    // The probe (arm a).
    const served = f.probeRead(requestOf(.{ .read = .{ .fh = 3, .offset = 4096, .size = 16, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 } }, 12, EchoFs.ino_big), &window).?;
    try testing.expectEqual(@as(usize, 16), served.body.len);
    try testing.expectEqual(EchoFs.bigByte(4096), served.body[0]);
    try testing.expectEqual(EchoFs.bigByte(4111), served.body[15]);
    try testing.expect(f.probeRead(requestOf(.{ .read = rd }, 14, EchoFs.ino_park), &window) == null);
    fs.probe_declines = true;
    try testing.expect(f.probeRead(requestOf(.{ .read = rd }, 16, EchoFs.ino_hello), &window) == null);
    // FORGET returns the reference.
    f.forget(EchoFs.ino_hello, 1);
    try testing.expectEqual(@as(u64, 0), fs.lookups[EchoFs.ino_hello].load(.monotonic));
    try testing.expectEqual(@as(u64, 1), fs.forgets.load(.monotonic));
}

test "EchoFs: a WRITE to scratch consumes the lease exactly once and lands the bytes; a WRITE elsewhere is EROFS and still releases" {
    var fs: EchoFs = .{};
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var state: slot.LeaseState = .{};
    const NoWake = struct {
        fn wake(_: ?*anyopaque) bool {
            return false;
        }
    };
    const waker: slot.Waker = .{ .ctx = null, .wake = NoWake.wake };
    const data = "scratch bytes";
    const win: abi.fuse_write_in = .{ .fh = 5, .offset = 3, .size = data.len, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    const lease = slot.PayloadLease.acquire(&state, data, waker);
    fs.filesystem().dispatch(requestOf(.{ .write = .{ .in = win, .data = data } }, 20, EchoFs.ino_scratch), .{ .tx = &t, .slot = .classical, .unique = 20, .window = &.{} }, lease);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expect(!state.leased());
    try testing.expectEqualSlices(u8, data, fs.scratch[3..][0..data.len]);
    try testing.expectEqual(@as(u64, 3 + data.len), fs.scratch_len.load(.acquire));
    const lease2 = slot.PayloadLease.acquire(&state, data, waker);
    fs.filesystem().dispatch(requestOf(.{ .write = .{ .in = win, .data = data } }, 22, EchoFs.ino_hello), .{ .tx = &t, .slot = .classical, .unique = 22, .window = &.{} }, lease2);
    try testing.expectEqual(linux.E.ROFS, rec.last_errno);
    try testing.expect(!state.leased());
}
