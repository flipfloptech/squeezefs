//! The FUSE handler layer (`docs/porting/fuse_handlers_lanes.md`; charter
//! §2 lock order level (1), §4, §6.2, §9, §10): the `Filesystem` interface,
//! the request lifecycle on the handler lanes, the per-inode op guard with
//! shared-mode admission, the open/lookup accounting, the deadline watchdog,
//! the per-op trace, and the first real filesystem — the POSIX set over the
//! KV metadata backend with inline data.
//!
//! Portable: every module here compiles on every host; the ONE Linux-only
//! binding is `BackendFs` (the KV backend owns device I/O), gated below.
//!
//! The transport binding: `lifecycle.Dispatcher(T)` takes the transport as
//! a comptime parameter checked by `transport_seam.assertTransport`. The
//! ring transport (`src/fuse/transport/*`) addresses a reply by its
//! `filesystem_vtable.Handle` — the slot by value plus the session's `Tx`
//! face — so `ring.Transport` IS that handle's methods and `ring.Binding`
//! is the `filesystem_vtable.Filesystem` the session calls. The in-memory
//! `transport_seam.Loopback` is the venue every lifecycle test runs on.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const codec = @import("codec.zig");
const fsv = @import("transport/filesystem_vtable.zig");
const slot = @import("transport/slot.zig");

pub const filesystem = @import("handler/filesystem.zig");
pub const lifecycle = @import("handler/lifecycle.zig");
pub const op_guard = @import("handler/op_guard.zig");
pub const op_trace = @import("handler/op_trace.zig");
pub const open_table = @import("handler/open_table.zig");
pub const watchdog = @import("handler/watchdog.zig");
pub const transport_seam = @import("handler/transport_seam.zig");
pub const kv_fs = @import("handler/kv_fs.zig");

pub const Filesystem = filesystem.Filesystem;
pub const Ctx = filesystem.Ctx;
pub const Dispatcher = lifecycle.Dispatcher;
pub const LoopbackDispatcher = lifecycle.Dispatcher(transport_seam.Loopback);

/// The first mount's filesystem over the Linux KV backend.
pub const BackendFs = if (builtin.os.tag == .linux) kv_fs.KvFs(@import("../meta/backend.zig").KvBackend) else struct {};
/// The device-path end-to-end suite (Linux).
pub const backend_e2e = if (builtin.os.tag == .linux) @import("handler/backend_e2e.zig") else struct {};

/// The ring transport binding.
pub const ring = struct {
    /// The transport as the dispatcher sees it: the reply address IS the
    /// session's `Handle` (slot by value + the `Tx` face), so the type is
    /// stateless — one instance serves every session.
    pub const Transport = struct {
        pub const SlotRef = fsv.Handle;
        pub const PayloadLease = slot.PayloadLease;

        pub fn replyWindow(_: *Transport, h: SlotRef) []u8 {
            return h.window;
        }

        pub fn submitReply(_: *Transport, h: SlotRef, r: codec.Reply) void {
            h.reply(r);
        }

        pub fn failEnt(_: *Transport, h: SlotRef, _: u64, errno: linux.E) void {
            h.fail(errno);
        }

        pub fn releaseLease(_: *Transport, l: PayloadLease) void {
            l.release();
        }
    };

    pub const RingDispatcher = lifecycle.Dispatcher(Transport);

    /// The `filesystem_vtable.Filesystem` the session calls, over a
    /// dispatcher. Pinned in place after `bind` (the dispatcher points at
    /// the embedded transport).
    pub const Binding = struct {
        transport: Transport = .{},
        d: RingDispatcher = undefined,

        pub fn bind(self: *Binding, opts: lifecycle.Options) void {
            self.d = RingDispatcher.init(&self.transport, opts);
        }

        pub fn filesystem(self: *Binding) fsv.Filesystem {
            return .{ .ctx = self, .vtable = &vtable };
        }

        const vtable: fsv.Filesystem.VTable = .{
            .init = fsInit,
            .destroy = fsDestroy,
            .dispatch = fsDispatch,
            .probe_read = fsProbeRead,
            .forget = fsForget,
        };

        fn of(ctx: *anyopaque) *Binding {
            return @ptrCast(@alignCast(ctx));
        }

        fn fsInit(ctx: *anyopaque, caps: fsv.InitCaps) void {
            // A filesystem that refuses INIT refuses the mount: the session
            // fails loud before it is ready (charter §1.8).
            of(ctx).d.initFilesystem(caps.negotiated, caps.minor) catch |e| {
                std.debug.panic("filesystem init refused after INIT negotiated: {t}", .{e});
            };
        }

        fn fsDestroy(ctx: *anyopaque) void {
            of(ctx).d.destroyFilesystem();
        }

        fn fsDispatch(ctx: *anyopaque, req: fsv.Request, h: fsv.Handle, lease: ?slot.PayloadLease) void {
            of(ctx).d.dispatch(req.op, .{ .unique = req.header.unique, .nodeid = req.header.nodeid, .uid = req.header.uid, .gid = req.header.gid, .pid = req.header.pid }, h, lease);
        }

        /// The READ dispatch law's arm (a): rung 4's try-only warm ladder.
        /// Rung 3 has no resident tier to serve from, so every READ takes
        /// arm (c) — the lane homed on the queue's CPU.
        fn fsProbeRead(_: *anyopaque, _: fsv.Request, _: []u8) ?codec.Reply {
            return null;
        }

        fn fsForget(ctx: *anyopaque, ino: u64, nlookup: u64) void {
            of(ctx).d.forget(ino, nlookup);
        }
    };
};

comptime {
    transport_seam.assertTransport(ring.Transport);
}

test {
    _ = filesystem;
    _ = lifecycle;
    _ = op_guard;
    _ = op_trace;
    _ = open_table;
    _ = watchdog;
    _ = transport_seam;
    _ = kv_fs;
    if (builtin.os.tag == .linux) _ = backend_e2e;
}

const testing = std.testing;

/// A recording `Tx` face: what the session would commit.
const RecordingTx = struct {
    replies: u32 = 0,
    fails: u32 = 0,
    last_errno: linux.E = .SUCCESS,
    last_len: u32 = 0,
    last_out: codec.OutBuf = undefined,

    fn submit(ctx: *anyopaque, _: fsv.Handle, r: codec.Reply) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
        self.last_len = r.header.len;
        @memcpy(self.last_out[0..r.out.len], r.out);
    }
    fn submitFdBody(ctx: *anyopaque, _: fsv.Handle, _: codec.Reply, _: linux.fd_t, _: u64) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
    }
    fn submitPrefilled(ctx: *anyopaque, _: fsv.Handle, _: u32) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
    }
    fn fail(ctx: *anyopaque, _: fsv.Handle, errno: linux.E) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.fails += 1;
        self.last_errno = errno;
    }
    fn tx(self: *RecordingTx) fsv.Tx {
        return .{ .ctx = self, .submit = submit, .submit_fd_body = submitFdBody, .submit_prefilled = submitPrefilled, .fail = fail };
    }
};

test "ring binding: the session's vtable reaches the lifecycle — init negotiates, a dispatch answers through the handle exactly once, the probe declines, forget hands off, destroy sweeps" {
    var store = try kv_fs.MemStore.init(testing.allocator);
    defer store.deinit();
    var fs = try kv_fs.MemFs.mount(testing.allocator, testing.io, &store, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var reg = try watchdog.Registry.init(testing.allocator, 1);
    defer reg.deinit();
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var opens = try open_table.OpenTable.init(testing.allocator, 64);
    defer opens.deinit();
    var binding: ring.Binding = .{};
    binding.bind(.{ .fs = Filesystem.of(kv_fs.MemFs, &fs), .io = testing.io, .registry = &reg, .guards = &guards, .opens = &opens });
    const f = binding.filesystem();
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var window: [4096]u8 = undefined;
    f.init(.{ .negotiated = @import("abi.zig").FUSE_HANDLE_KILLPRIV_V2, .minor = 45, .plan = undefined, .io = testing.io });
    try testing.expect(fs.killpriv_v2);
    try testing.expectEqual(@as(u32, 45), fs.minor);
    const header: @import("abi.zig").fuse_in_header = .{ .len = 40, .opcode = 35, .unique = 2, .nodeid = 1, .uid = 5, .gid = 6, .pid = 7, .total_extlen = 0, .padding = 0 };
    const h: fsv.Handle = .{ .tx = &t, .slot = .{ .ring = .{ .qid = 0, .ent_idx = 0, .commit_id = 2 } }, .unique = 2, .window = &window };
    f.dispatch(.{ .header = header, .op = .{ .create = .{ .name = "f", .flags = 0, .mode = 0o644, .umask = 0, .open_flags = 0 } } }, h, null);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 16 + 144), rec.last_len);
    try testing.expectEqual(@as(u64, 1), opens.openCount());
    const ino = (@import("abi.zig").decodePrefix(@import("abi.zig").fuse_entry_out, &rec.last_out) orelse return error.TestBadReply).nodeid;
    try testing.expectEqual(@as(u64, 1), opens.get(ino).?.lookups);
    // The probe declines at rung 3; a READ takes the lane.
    const rd: @import("abi.zig").fuse_read_in = .{ .fh = 1, .offset = 0, .size = 16, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    try testing.expect(f.probeRead(.{ .header = header, .op = .{ .read = rd } }, &window) == null);
    // A miss through the handle: one fail, the table's errno.
    var h2 = h;
    h2.unique = 4;
    f.dispatch(.{ .header = .{ .len = 40, .opcode = 3, .unique = 4, .nodeid = ino, .uid = 0, .gid = 0, .pid = 0, .total_extlen = 0, .padding = 0 }, .op = .{ .getattr = .{ .flags = 0, .fh = null } } }, h2, null);
    f.dispatch(.{ .header = .{ .len = 40, .opcode = 3, .unique = 6, .nodeid = 777, .uid = 0, .gid = 0, .pid = 0, .total_extlen = 0, .padding = 0 }, .op = .{ .getattr = .{ .flags = 0, .fh = null } } }, h2, null);
    try testing.expectEqual(@as(u32, 2), rec.replies);
    try testing.expectEqual(@as(u32, 1), rec.fails);
    try testing.expectEqual(linux.E.NOENT, rec.last_errno);
    // RELEASE through the handle, then the worker-side forget hand-off:
    // the open table closes the ino with nothing stranded.
    const stranded0 = @import("../core/counters.zig").tripwireCount("open_count_stranded");
    f.dispatch(.{ .header = .{ .len = 40, .opcode = 18, .unique = 8, .nodeid = ino, .uid = 0, .gid = 0, .pid = 0, .total_extlen = 0, .padding = 0 }, .op = .{ .release = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } } }, h2, null);
    try testing.expectEqual(@as(u32, 3), rec.replies);
    f.forget(ino, 1);
    try testing.expectEqual(@as(?open_table.Entry, null), opens.get(ino));
    try testing.expectEqual(stranded0, @import("../core/counters.zig").tripwireCount("open_count_stranded"));
    try testing.expectEqual(@as(u64, 0), opens.openCount());
    f.destroy();
}
