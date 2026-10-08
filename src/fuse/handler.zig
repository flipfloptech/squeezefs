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
pub const virtual_inodes = @import("handler/virtual_inodes.zig");

pub const Filesystem = filesystem.Filesystem;
pub const Ctx = filesystem.Ctx;
pub const Dispatcher = lifecycle.Dispatcher;
pub const LoopbackDispatcher = lifecycle.Dispatcher(transport_seam.Loopback);

/// The product filesystem: the Linux KV backend + the data plane.
pub const BackendFs = if (builtin.os.tag == .linux) kv_fs.KvFs(@import("../meta/meta_set.zig").MetaSet, @import("../data/plane.zig").DataPlane) else struct {};
/// The data plane's queue-thread arms (the probe, the direct leg, the W1
/// hold gate), bound into `ring.Binding` by the mount; null = rung 3's
/// declines (the loopback tests, a plane-less binding).
pub const DataArms = if (builtin.os.tag == .linux) struct {
    plane: *@import("../data/plane.zig").DataPlane,
} else struct {};
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
        arms: ?DataArms = null,

        pub fn bind(self: *Binding, opts: lifecycle.Options) void {
            self.d = RingDispatcher.init(&self.transport, opts);
        }

        /// The data plane's arms (after `bind`, before the session arms).
        pub fn bindData(self: *Binding, arms: DataArms) void {
            self.arms = arms;
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
            .write_hold_eligible = fsWriteHoldEligible,
            .direct_leg = fsDirectLeg,
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

        /// The kernel nodeid of a request → the KEY ino the plane's tables
        /// (`noteShape`, the layout and inode records, the write side) are
        /// keyed by: the same routing law `KvFs.localOf` applies on the
        /// lanes (charter §5.4), applied here on the queue thread so a
        /// nodeid never reaches the plane. A nodeid and a different file's
        /// key ino CAN coincide (global 3 = slot 1's first mint; key 3 =
        /// slot 0's second), so an untranslated probe serves the wrong
        /// file's bytes. Null for a nodeid outside the mint's space (the
        /// virtual control inodes, nodeid 0): the arm declines and the lane
        /// answers it.
        fn keyIno(nodeid: u64) ?u64 {
            const route_table = @import("../meta/routing/route_table.zig");
            if (!route_table.isRoutable(nodeid)) return null;
            return route_table.effectiveIno(nodeid);
        }

        /// The READ dispatch law's arm (a): the data plane's try-only warm
        /// ladder (per-inode try-lock → the write side → hot tier → hold →
        /// NVMe read cache), on the queue thread; without a plane every
        /// READ takes arm (c) — the lane homed on the queue's CPU.
        fn fsProbeRead(ctx: *anyopaque, req: fsv.Request, window: []u8) ?fsv.ProbeReply {
            if (builtin.os.tag != .linux) return null;
            const arms = of(ctx).arms orelse return null;
            return arms.plane.ladder.probeRead(keyIno(req.nodeid()) orelse return null, req, window);
        }

        fn fsForget(ctx: *anyopaque, ino: u64, nlookup: u64) void {
            of(ctx).d.forget(ino, nlookup);
        }

        /// The W1 hold gate: the plane's pure predicate over the hint and
        /// its published per-inode shape (`patch_w1`'s request-shape half
        /// plus striped ∧ passthrough ∧ non-extending); no plane ⇒ nothing
        /// is held and every zc payload is extracted at delivery.
        fn fsWriteHoldEligible(ctx: *anyopaque, hint: fsv.HoldHint) bool {
            if (builtin.os.tag != .linux) return false;
            const arms = of(ctx).arms orelse return false;
            var routed = hint;
            routed.ino = keyIno(hint.ino) orelse return false;
            return arms.plane.writeHoldEligible(routed);
        }

        /// The direct device leg with its post-DMA recheck hook.
        fn fsDirectLeg(ctx: *anyopaque, req: fsv.Request) ?fsv.DirectLeg {
            if (builtin.os.tag != .linux) return null;
            const arms = of(ctx).arms orelse return null;
            return arms.plane.ladder.directLegFor(keyIno(req.nodeid()) orelse return null, req);
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
    _ = virtual_inodes;
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
    fn submitRetain(ctx: *anyopaque, _: fsv.Handle, _: codec.Reply) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
    }
    fn releaseRetained(_: *anyopaque, _: fsv.Handle) void {}
    fn tx(self: *RecordingTx) fsv.Tx {
        return .{ .ctx = self, .submit = submit, .submit_fd_body = submitFdBody, .submit_prefilled = submitPrefilled, .fail = fail, .submit_retain = submitRetain, .release_retained = releaseRetained };
    }
};

test "ring binding: the queue-thread arms route the kernel nodeid with the lanes' law — the probe/hold/leg ino IS `KvFs.localOf`; the smoke's collision pair (nodeid 3 = slot 1's first mint, key 3 = slot 0's second) resolves to two different files" {
    const nodeid_slot1_first: u64 = 3;
    const nodeid_slot0_second: u64 = 65_538;
    try testing.expectEqual(try kv_fs.MemFs.localOf(nodeid_slot1_first), ring.Binding.keyIno(nodeid_slot1_first).?);
    try testing.expectEqual(try kv_fs.MemFs.localOf(nodeid_slot0_second), ring.Binding.keyIno(nodeid_slot0_second).?);
    // Untranslated, nodeid 3 names the key of slot 0's second file.
    try testing.expectEqual(@as(u64, 3), ring.Binding.keyIno(nodeid_slot0_second).?);
    try testing.expect(ring.Binding.keyIno(nodeid_slot1_first).? != nodeid_slot1_first);
    try testing.expectEqual(@as(u64, 1), ring.Binding.keyIno(1).?);
    // Outside the mint's space — a virtual control ino (`bench` reads
    // `.stats` under the probe), nodeid 0 — the arm declines and the lane
    // answers ENOENT; neither reaches the law.
    const virtual = virtual_inodes.GEN_INO_FIRST;
    try testing.expect(virtual_inodes.isVirtualIno(virtual));
    try testing.expectEqual(@as(?u64, null), ring.Binding.keyIno(virtual));
    try testing.expectEqual(@as(?u64, null), ring.Binding.keyIno(0));
    try testing.expectError(error.FileNotFound, kv_fs.MemFs.localOf(virtual));
}

test "ring binding: the session's vtable reaches the lifecycle — init negotiates, a dispatch answers through the handle exactly once, the probe declines, forget hands off, destroy sweeps" {
    var store = try kv_fs.MemStore.init(testing.allocator);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(testing.allocator, testing.io, &store, &nodata, .{ .dlm_stripes = 64 });
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
