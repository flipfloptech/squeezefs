//! The Linux smoke stage of the token plane (charter §5.5; `docs/porting/
//! sym_tokens_intents_striping.md` §6 "Venue law"): ONE real file-backed
//! symmetric volume opened through the device path — its conveyor pass
//! (a fiber on the journal lane) armed with the A1½ hook of TWO holder
//! planes (writer A over the native slot, writer B over a guest slot,
//! both serving the volume's own trees as their record source) — and ONE
//! `-o ro` member-reader + token client reading both over the loopback
//! wire: first touches, cached reads, a commit's recall through the real
//! pass, fresh re-grants, and the S5 epoch poll of the fixed ledger on the
//! device (the predicted-slot read after a checkpoint). Linux only (the
//! backend is).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const linux = std.os.linux;

const backend = @import("../../backend.zig");
const exec = @import("../../../exec/exec.zig");
const align_probe = @import("../../../io/align_probe.zig");
const geometry = @import("../../geometry.zig");
const economy = @import("../../economy.zig");
const record = @import("../../record.zig");
const key = @import("../../key.zig");
const tree = @import("../../tree.zig");
const inos = @import("../../inos.zig");
const ledger = @import("../../ledger.zig");
const route_table = @import("../../routing/route_table.zig");
const conveyor = @import("../../conveyor.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const loopback = @import("../../../wire/loopback.zig");
const auth = @import("../../../wire/auth.zig");
const tokens = @import("../tokens.zig");
const table = tokens.table;
const cache = tokens.cache;
const divert = tokens.divert;
const ro = tokens.ro;

comptime {
    if (builtin.os.tag != .linux) @compileError("the token smoke stage is Linux only (the backend is); gate the import on builtin.os.tag");
}

const testing = std.testing;
const test_dir = "/build/tmp";
const volume_len: u64 = 64 << 20;
const set_identity: geometry.SetIdentity = .{ .set_uuid = "tok-smoke-set!!!".*, .hash_seed = 0x70C3_5EED_0000_0001 };

/// The backend's trees as a holder's `RecordSource`.
const BackendSource = struct {
    be: *backend.KvBackend,

    fn inode(ctx: *anyopaque, object: u64) table.SourceError!?record.InodeValue {
        const self: *BackendSource = @ptrCast(@alignCast(ctx));
        const bytes = (self.be.lookup(self.be.gpa, key.TREE_INODES, &key.inodeKey(object)) catch |e| return mapErr(e)) orelse return null;
        defer self.be.gpa.free(bytes);
        return record.InodeValue.decode(bytes) catch return error.Corrupt;
    }

    fn xattrs(ctx: *anyopaque, arena: Allocator, object: u64, out: *std.ArrayList(table.XattrRec)) table.SourceError!void {
        const self: *BackendSource = @ptrCast(@alignCast(ctx));
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.be.gpa, &kvs);
        const start = key.xattrKey(object, 0, 0);
        const end = key.xattrKey(object, key.HASH56_MAX, std.math.maxInt(u8));
        self.be.range(self.be.gpa, key.TREE_XATTRS, &start, &end, 4096, &kvs) catch |e| return mapErr(e);
        for (kvs.items) |kv| {
            const xv = record.XattrValue.decode(kv.value) catch return error.Corrupt;
            try out.append(arena, .{ .name = try arena.dupe(u8, xv.name), .value = try arena.dupe(u8, xv.value) });
        }
        std.mem.sort(table.XattrRec, out.items, {}, struct {
            fn lt(_: void, a: table.XattrRec, b: table.XattrRec) bool {
                return std.mem.lessThan(u8, a.name, b.name);
            }
        }.lt);
    }

    fn dentries(ctx: *anyopaque, arena: Allocator, object: u64, after: u64, max: usize, out: *std.ArrayList(table.DirRec)) table.SourceError!bool {
        const self: *BackendSource = @ptrCast(@alignCast(ctx));
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.be.gpa, &kvs);
        const start = key.dentryKey(object, 0, 0);
        const end = key.dentryKey(object, key.HASH54_MAX, std.math.maxInt(u8));
        self.be.range(self.be.gpa, key.TREE_DENTRIES, &start, &end, max + 1 + 4096, &kvs) catch |e| return mapErr(e);
        var n: usize = 0;
        for (kvs.items) |kv| {
            const dk = key.DentryKey.decode(kv.key) catch return error.Corrupt;
            const cookie = key.encodeReaddirCookie(dk.hash54(), dk.collSeq());
            if (cookie <= after) continue;
            if (n == max) return true;
            const dv = record.DentryValue.decode(kv.value) catch return error.Corrupt;
            try out.append(arena, .{ .cookie = cookie, .child_ino = dv.child_ino, .file_type = dv.file_type, .name = try arena.dupe(u8, dv.name) });
            n += 1;
        }
        return false;
    }

    fn mapErr(e: backend.Error) table.SourceError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    fn source(self: *BackendSource) table.RecordSource {
        return .{ .ctx = self, .inode = &inode, .xattrs = &xattrs, .dentries = &dentries };
    }
};

/// The two holder planes' A1½ as ONE conveyor hook: each plane recalls its
/// own grants (an object another plane holds is a no-op here) and both
/// settle.
const TwoPlaneHook = struct {
    a: tokens.ConveyorBinding,
    b: tokens.ConveyorBinding,

    fn hook(self: *TwoPlaneHook) conveyor.TokenHook {
        return .{ .ctx = self, .recall = &recall, .settle = &settle };
    }
    fn recall(ctx: *anyopaque, objects: []const u64) Allocator.Error!void {
        const self: *TwoPlaneHook = @ptrCast(@alignCast(ctx));
        const ha = self.a.hook();
        const hb = self.b.hook();
        try ha.recall(ha.ctx, objects);
        try hb.recall(hb.ctx, objects);
    }
    fn settle(ctx: *anyopaque, objects: []const u64) void {
        const self: *TwoPlaneHook = @ptrCast(@alignCast(ctx));
        const ha = self.a.hook();
        const hb = self.b.hook();
        ha.settle(ha.ctx, objects);
        hb.settle(hb.ctx, objects);
    }
};

const A_ID: u64 = 0xA;
const B_ID: u64 = 0xB;
const SLOT_B: u16 = 3;

/// The ledger on the device through the backend's O_DIRECT reader.
const DeviceLedger = struct {
    be: *backend.KvBackend,
    fn readSlot(ctx: *anyopaque, slot: u64, out: *[ro.SLOT_LEN]u8) ro.IoError!void {
        const self: *DeviceLedger = @ptrCast(@alignCast(ctx));
        const bytes = self.be.dev.readRange(self.be.gpa, ledger.slotOffset(self.be.dev.ledger_base, slot), ro.SLOT_LEN) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer self.be.gpa.free(bytes);
        @memcpy(out, bytes[0..ro.SLOT_LEN]);
    }
    fn readAll(ctx: *anyopaque, out: []u8) ro.IoError!void {
        const self: *DeviceLedger = @ptrCast(@alignCast(ctx));
        const bytes = self.be.dev.readRange(self.be.gpa, self.be.dev.ledger_base, ro.LEDGER_LEN) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer self.be.gpa.free(bytes);
        @memcpy(out[0..ro.LEDGER_LEN], bytes[0..ro.LEDGER_LEN]);
    }
    fn io(self: *DeviceLedger) ro.LedgerIo {
        return .{ .ctx = self, .readSlot = &readSlot, .readAll = &readAll };
    }
};

fn linkCall(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
    const c: *loopback.Conn = @ptrCast(@alignCast(ctx));
    return c.call(req);
}

/// The reader's resolver: slot 0 is A's, `SLOT_B` is B's.
const RoResolver = struct {
    to_a: *loopback.Conn,
    to_b: *loopback.Conn,
    fn homeOf(_: *anyopaque, slot: u16) divert.Home {
        return if (slot == 0) .{ .foreign = .{ .holder = 1, .identity = A_ID } } else if (slot == SLOT_B) .{ .foreign = .{ .holder = 2, .identity = B_ID } } else .local;
    }
    fn refresh(ctx: *anyopaque, slot: u16, _: u32) divert.Home {
        return homeOf(ctx, slot);
    }
    fn dial(ctx: *anyopaque, identity: u64, _: u32) ?cache.Link {
        const self: *RoResolver = @ptrCast(@alignCast(ctx));
        return .{ .ctx = if (identity == A_ID) self.to_a else self.to_b, .call = &linkCall };
    }
};

const Peer = struct {
    conn: *loopback.Conn,
    fn linkFor(ctx: *anyopaque, _: u64) ?cache.Link {
        const self: *Peer = @ptrCast(@alignCast(ctx));
        return .{ .ctx = self.conn, .call = &linkCall };
    }
};

fn truncateFile(path: []const u8, len: u64) !void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
}

fn stageFile(be: *backend.KvBackend, t: *backend.KvTx, slot: u16, name: []const u8, mtime: u64) !u64 {
    const ino = try be.allocateInoInSlot(slot);
    const parent_value: record.InodeValue = .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2 };
    const global = inos.makeGlobalIno(ino, slot, inos.ROUTING_WIDTH);
    _ = try economy.stageCreate(t, .{ .hash_seed = be.hashSeed(), .now = mtime, .parent = 1, .parent_value = parent_value, .name = name, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = route_table.effectiveIno(global), .global_ino = global });
    return route_table.effectiveIno(global);
}

test "SMOKE: one real volume, two holder planes on its conveyor's A1½, one -o ro reader over the wire: first touch, cached, a commit's recall through the real pass, fresh re-grants from both holders, the epoch poll's predicted slot on the device" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const sqz = try exec.SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
    var path_buf: [128]u8 = undefined;
    const path = try std.fmt.bufPrint(&path_buf, "{s}/sqz-tok-smoke-{d}.img", .{ test_dir, linux.getpid() });
    try truncateFile(path, volume_len);
    defer if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {};
    try backend.formatVolume(gpa, path, .{ .env = &env, .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "tok-smoke-uuid!!".*, .set = set_identity });
    const be = try backend.KvBackend.open(gpa, io, path, .{ .env = &env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
    defer be.close();

    // The writers' records: A's file in the native slot, B's in slot 3.
    var a_file: u64 = 0;
    var b_file: u64 = 0;
    {
        const t = try be.begin(@src());
        a_file = try stageFile(be, t, 0, "a-file", 100);
        try be.commit(t);
        const t2 = try be.begin(@src());
        b_file = try stageFile(be, t2, SLOT_B, "b-file", 100);
        try be.commit(t2);
    }

    // The planes: two holders over the volume's trees, one reader.
    var owner = ro.OwnerLeases.init(gpa);
    defer owner.deinit();
    const now = clock.monotonicNs();
    try owner.grant(7, now);
    var src: BackendSource = .{ .be = be };
    var holder_a = try table.Holder.init(gpa, io, A_ID, owner.oracle(), 1024, 16);
    defer holder_a.deinit();
    var holder_b = try table.Holder.init(gpa, io, B_ID, owner.oracle(), 1024, 16);
    defer holder_b.deinit();
    var hub = loopback.LoopbackHub.init(gpa, io);
    defer hub.deinit();
    const secret: auth.Secret = @splat(0x33);
    var svcs: [3]service.Service = @splat(.init);
    const ResA = struct {
        fn homeOf(_: *anyopaque, slot: u16) divert.Home {
            return if (slot == 0) .local else .{ .foreign = .{ .holder = 2, .identity = B_ID } };
        }
        fn refresh(_: *anyopaque, _: u16, _: u32) divert.Home {
            return .local;
        }
        fn dial(_: *anyopaque, _: u64, _: u32) ?cache.Link {
            return null;
        }
    };
    const ResB = struct {
        fn homeOf(_: *anyopaque, slot: u16) divert.Home {
            return if (slot == SLOT_B) .local else .{ .foreign = .{ .holder = 1, .identity = A_ID } };
        }
    };
    var marker: u8 = 0;
    var svc_a: divert.HolderService = .{ .io = io, .holder = &holder_a, .source = src.source(), .resolver = .{ .ctx = &marker, .homeOf = &ResA.homeOf, .refresh = &ResA.refresh, .dial = &ResA.dial }, .custody = null };
    var svc_b: divert.HolderService = .{ .io = io, .holder = &holder_b, .source = src.source(), .resolver = .{ .ctx = &marker, .homeOf = &ResB.homeOf, .refresh = &ResA.refresh, .dial = &ResA.dial }, .custody = null };
    svc_a.register(&svcs[0]);
    svc_b.register(&svcs[1]);
    const ident = struct {
        fn of(n: u64) verbs.WireIdentity {
            return .{ .node_token = 0x7000 + n, .writer_id = n, .mount_slot = @intCast(n), .reserved = 0 };
        }
    };
    _ = try hub.addDaemon(&svcs[0], secret, ident.of(1));
    _ = try hub.addDaemon(&svcs[1], secret, ident.of(2));
    _ = try hub.addDaemon(&svcs[2], secret, ident.of(3));
    var r_to_a = try hub.connect(2, 0);
    defer r_to_a.deinit();
    var r_to_b = try hub.connect(2, 1);
    defer r_to_b.deinit();
    var a_to_r = try hub.connect(0, 2);
    defer a_to_r.deinit();
    var b_to_r = try hub.connect(1, 2);
    defer b_to_r.deinit();
    var peer_a: Peer = .{ .conn = &a_to_r };
    var peer_b: Peer = .{ .conn = &b_to_r };
    var push_a: divert.LinkPusher = .{ .volume = 0, .identity = A_ID, .peers = .{ .ctx = &peer_a, .linkFor = &Peer.linkFor } };
    var push_b: divert.LinkPusher = .{ .volume = 0, .identity = B_ID, .peers = .{ .ctx = &peer_b, .linkFor = &Peer.linkFor } };
    var two: TwoPlaneHook = .{ .a = .{ .io = io, .holder = &holder_a, .pusher = push_a.pusher() }, .b = .{ .io = io, .holder = &holder_b, .pusher = push_b.pusher() } };
    const hook = two.hook();
    be.cv.armTokens(&hook);
    defer be.cv.disarmTokens();

    // The -o ro reader: a member (its lease), the TTL law, a divert over
    // both holders.
    var opts: @import("../../../fuse/mount_options.zig").MountOptions = .{ .read_only = true };
    const ttls = try ro.tokenReaderTtls(&opts);
    try testing.expectEqual(@as(u64, 0), ttls.attr_ns);
    try testing.expectEqual(ro.ReaderArm.tokens, ro.armFor(true));
    var my_lease: ro.MemberLease = .{};
    my_lease.renew(now);
    var res: RoResolver = .{ .to_a = &r_to_a, .to_b = &r_to_b };
    var reader = divert.Divert.init(gpa, io, .{ .ctx = &res, .homeOf = &RoResolver.homeOf, .refresh = &RoResolver.refresh, .dial = &RoResolver.dial }, 7, 0, cache.RecallSink.none, my_lease.selfLease(), cache.recordsBudgetBytes(0));
    defer reader.deinit();
    var ro_svc: divert.ReaderService = .{ .io = io, .divert = &reader };
    ro_svc.register(&svcs[2]);

    // First touches of both writers' files: one grant each; then cached.
    const grants0 = cache.stats.dlm_token_grants.load();
    var s = (try reader.serve(io, a_file, .{})).?;
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    s = (try reader.serve(io, b_file, .{})).?;
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    try testing.expectEqual(grants0 + 2, cache.stats.dlm_token_grants.load());
    const hits0 = cache.stats.dlm_token_hits.load();
    s = (try reader.serve(io, a_file, .{})).?;
    s.release(io);
    try testing.expectEqual(hits0 + 1, cache.stats.dlm_token_hits.load());
    try testing.expect(holder_a.holdsGrant(io, a_file, 7));
    try testing.expect(holder_b.holdsGrant(io, b_file, 7));

    // A's setattr through the REAL conveyor: the pass fiber recalls at A1½
    // before ring admission; the reader acks inline; the entry lands.
    const recalls0 = table.stats.dlm_token_recalls.load();
    {
        const t = try be.begin(@src());
        const v: record.InodeValue = .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .mtime = 200, .ctime = 200 };
        try t.stagePut(key.TREE_INODES, &key.inodeKey(a_file), &v.encode());
        try be.commit(t);
    }
    try testing.expectEqual(recalls0 + 1, table.stats.dlm_token_recalls.load());
    try testing.expect(!holder_a.holdsGrant(io, a_file, 7));
    try testing.expect(holder_b.holdsGrant(io, b_file, 7));
    try testing.expect(be.cv.stats.pass_token_recall.snapshot().count >= 1);
    s = (try reader.serve(io, a_file, .{})).?;
    try testing.expectEqual(@as(u64, 200), s.entry.?.attrs.mtime);
    s.release(io);
    // B's file through the same conveyor: B's plane recalls, A's is a no-op.
    {
        const t = try be.begin(@src());
        const v: record.InodeValue = .{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .mtime = 300, .ctime = 300 };
        try t.stagePut(key.TREE_INODES, &key.inodeKey(b_file), &v.encode());
        try be.commit(t);
    }
    try testing.expectEqual(recalls0 + 2, table.stats.dlm_token_recalls.load());
    s = (try reader.serve(io, b_file, .{})).?;
    try testing.expectEqual(@as(u64, 300), s.entry.?.attrs.mtime);
    s.release(io);
    try testing.expectEqual(@as(usize, 0), holder_a.gate.inflightCount(io) + holder_b.gate.inflightCount(io));

    // The control plane: the reader's poll of the fixed ledger on the
    // device — a checkpoint lands in the predicted slot.
    var dl: DeviceLedger = .{ .be = be };
    var poll = ro.EpochPoll.init(gpa, dl.io(), be.mounted.seq);
    // Adopt whatever the cadence landed so far, then the explicit cycle
    // sits in the predicted slot.
    var first = try poll.poll();
    if (first.newest) |*n| n.deinit(gpa);
    const adopted = poll.adopted_seq;
    try be.checkpointNow();
    var o = try poll.poll();
    try testing.expect(o.predicted_hit);
    try testing.expect(!o.full_read);
    try testing.expect(o.newest != null);
    try testing.expect(o.newest.?.seq > adopted);
    o.newest.?.deinit(gpa);
    // A quiescent reader that saw this advance would fail-stop ESTALE.
    var q = ro.QuiescentReader.init(adopted);
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    q.onPoll(.ledger_advance, path);
    try @import("../../../core/log.zig").testing.expectAllFired();
    try testing.expectError(error.ReaderFailStopped, q.admit(false));
    try q.admit(true);
}
