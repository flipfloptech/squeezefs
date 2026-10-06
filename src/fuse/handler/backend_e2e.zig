//! The first mount end to end (Linux): `KvFs(KvBackend)` behind the
//! lifecycle over a formatted file volume on the venue's ext4 — the same
//! POSIX shapes the in-RAM suite pins, through the device path: every
//! mutation is a conveyor commit, every read a node-cache snapshot, and
//! the records survive a close → reopen.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const backend = @import("../../meta/backend.zig");
const exec = @import("../../exec/exec.zig");
const geometry = @import("../../meta/geometry.zig");
const align_probe = @import("../../io/align_probe.zig");
const counters = @import("../../core/counters.zig");
const filesystem = @import("filesystem.zig");
const lifecycle = @import("lifecycle.zig");
const kv_fs = @import("kv_fs.zig");
const seam = @import("transport_seam.zig");
const op_guard = @import("op_guard.zig");
const watchdog = @import("watchdog.zig");
const open_table = @import("open_table.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("backend_e2e is Linux only (the KV backend owns device I/O); gate the import on builtin.os.tag");
}

const testing = std.testing;
const BackendFs = kv_fs.KvFs(backend.KvBackend);
const Loopback = seam.Loopback;
const LoopDispatcher = lifecycle.Dispatcher(Loopback);

const test_dir = "/build/tmp";
const volume_len: u64 = 64 << 20;
const set_identity: geometry.SetIdentity = .{ .set_uuid = "kv-fs-e2e-set!!!".*, .hash_seed = 0x0DD5_EED5_F00D_CAFE };

fn truncateFile(path: []const u8, len: u64) !void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
}

fn unlinkPath(path: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {}
}

/// One mounted first-mount filesystem behind a dispatcher. The backend's
/// `io` must be `SqzIo`'s (its conveyor pass is a lane fiber), so the
/// mount owns a test executor instance.
const Mount = struct {
    sqz: *exec.SqzIo,
    be: *backend.KvBackend,
    fs: BackendFs,
    lb: Loopback,
    reg: watchdog.Registry,
    guards: op_guard.Table,
    opens: open_table.OpenTable,
    d: LoopDispatcher,
    unique: u64 = 2,

    const slot = Loopback.slot(0);

    fn open(self: *Mount, gpa: std.mem.Allocator, env: *const std.process.Environ.Map, path: []const u8) !void {
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        const io = self.sqz.io();
        self.be = try backend.KvBackend.open(gpa, io, path, .{ .env = env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
        errdefer self.be.close();
        self.fs = try BackendFs.mount(gpa, io, self.be, .{ .dlm_stripes = 64 });
        errdefer self.fs.unmount();
        self.lb = try Loopback.init(gpa, 1, 8192);
        errdefer self.lb.deinit();
        self.reg = try watchdog.Registry.init(gpa, 1);
        errdefer self.reg.deinit();
        self.guards = try op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES);
        errdefer self.guards.deinit();
        self.opens = try open_table.OpenTable.init(gpa, 64);
        self.unique = 2;
        self.d = LoopDispatcher.init(&self.lb, .{ .fs = filesystem.Filesystem.of(BackendFs, &self.fs), .io = io, .registry = &self.reg, .guards = &self.guards, .opens = &self.opens });
    }

    fn close(self: *Mount) void {
        self.opens.deinit();
        self.guards.deinit();
        self.reg.deinit();
        self.lb.deinit();
        self.fs.unmount();
        self.be.close();
        self.sqz.deinit();
    }

    fn call(self: *Mount, req: codec.Request, nodeid: u64) Loopback.Captured {
        const u = self.unique;
        self.unique += abi.FUSE_REQ_ID_STEP;
        const before = self.lb.replyCount(slot);
        self.d.dispatch(req, .{ .unique = u, .nodeid = nodeid, .uid = 0, .gid = 0, .pid = 1 }, slot, null);
        std.debug.assert(self.lb.replyCount(slot) == before + 1);
        return self.lb.captured(slot).?;
    }

    fn entry(c: Loopback.Captured) !abi.fuse_entry_out {
        if (c.errno()) |e| {
            std.debug.print("expected an entry, got errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        return abi.decodePrefix(abi.fuse_entry_out, c.outBytes()) orelse error.TestBadReply;
    }

    fn attr(c: Loopback.Captured) !abi.fuse_attr {
        if (c.errno()) |e| {
            std.debug.print("expected attrs, got errno {t}\n", .{e});
            return error.TestUnexpectedErrno;
        }
        return (abi.decodeExact(abi.fuse_attr_out, c.outBytes()) orelse return error.TestBadReply).attr;
    }
};

test "first mount e2e: format → mount → create/write/read/mkdir/readdir/rename/unlink/xattr/fsync through the lifecycle → close → reopen: the namespace and the bytes persist; nothing stranded" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
    var path_buf: [128]u8 = undefined;
    const path = try std.fmt.bufPrint(&path_buf, "{s}/sqz-kvfs-e2e-{d}.img", .{ test_dir, linux.getpid() });
    try truncateFile(path, volume_len);
    defer unlinkPath(path);
    try backend.formatVolume(gpa, path, .{ .env = &env, .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "kv-fs-e2e-uuid!!".*, .set = set_identity });

    const stranded0 = counters.tripwireCount("open_count_stranded");
    var f_ino: u64 = 0;
    var d_ino: u64 = 0;
    {
        var m: Mount = undefined;
        try m.open(gpa, &env, path);
        defer m.close();
        const root = try Mount.attr(m.call(.{ .getattr = .{ .flags = 0, .fh = null } }, 1));
        try testing.expectEqual(@as(u32, 2), root.nlink);
        try testing.expect(root.mode & 0o170000 == 0o040000);
        // CREATE + WRITE + READ.
        const cr = m.call(.{ .create = .{ .name = "hello.txt", .flags = 0, .mode = 0o644, .umask = 0, .open_flags = 0 } }, 1);
        f_ino = (try Mount.entry(cr)).nodeid;
        const win: abi.fuse_write_in = .{ .fh = 1, .offset = 0, .size = 13, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        const w = m.call(.{ .write = .{ .in = win, .data = "hello, world!" } }, f_ino);
        try testing.expectEqual(@as(u32, 13), (abi.decodeExact(abi.fuse_write_out, w.outBytes()) orelse return error.TestBadReply).size);
        const rin: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        try testing.expectEqualStrings("hello, world!", m.call(.{ .read = rin }, f_ino).body);
        try testing.expectEqual(@as(u64, 13), (try Mount.attr(m.call(.{ .getattr = .{ .flags = 0, .fh = null } }, f_ino))).size);
        _ = m.call(.{ .release = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, f_ino);
        // MKDIR, a rename into it, READDIR of the directory.
        d_ino = (try Mount.entry(m.call(.{ .mkdir = .{ .name = "dir", .mode = 0o755, .umask = 0 } }, 1))).nodeid;
        try testing.expectEqual(@as(?linux.E, null), m.call(.{ .rename = .{ .newdir = d_ino, .flags = 0, .name = "hello.txt", .newname = "moved.txt" } }, 1).errno());
        try testing.expectEqual(@as(u64, 0), (try Mount.entry(m.call(.{ .lookup = .{ .name = "hello.txt" } }, 1))).nodeid);
        try testing.expectEqual(f_ino, (try Mount.entry(m.call(.{ .lookup = .{ .name = "moved.txt" } }, d_ino))).nodeid);
        const rd = m.call(.{ .readdir = rin }, d_ino);
        try testing.expect(std.mem.indexOf(u8, rd.body, "moved.txt") != null);
        // xattr + fsync.
        try testing.expectEqual(@as(?linux.E, null), m.call(.{ .setxattr = .{ .name = "user.tag", .value = "v1", .flags = 0 } }, f_ino).errno());
        try testing.expectEqualStrings("v1", m.call(.{ .getxattr = .{ .name = "user.tag", .size = 64 } }, f_ino).body);
        try testing.expectEqual(@as(?linux.E, null), m.call(.{ .fsync = .{ .fh = 1, .datasync = false } }, f_ino).errno());
        // A second file, unlinked while open, forgotten: the corpse is gathered.
        const g = (try Mount.entry(m.call(.{ .create = .{ .name = "gone", .flags = 0, .mode = 0o600, .umask = 0, .open_flags = 0 } }, 1))).nodeid;
        try testing.expectEqual(@as(?linux.E, null), m.call(.{ .unlink = .{ .name = "gone" } }, 1).errno());
        _ = m.call(.{ .release = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, g);
        m.d.dispatch(.{ .forget = .{ .nlookup = 1 } }, .{ .unique = m.unique, .nodeid = g, .uid = 0, .gid = 0, .pid = 1 }, Mount.slot, null);
        m.unique += 2;
        try testing.expectEqual(@as(usize, 1), m.fs.reclaim.items.len);
        // DESTROY drains the gather (the dismount sweep).
        try testing.expectEqual(@as(?linux.E, null), m.call(.destroy, 1).errno());
        try testing.expectEqual(@as(usize, 0), m.fs.reclaim.items.len);
        try testing.expectEqual(linux.E.NOENT, m.call(.{ .getattr = .{ .flags = 0, .fh = null } }, g).errno().?);
        try testing.expectEqual(@as(u64, 0), m.opens.openCount());
    }
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
    // Reopen: everything acked is there.
    {
        var m: Mount = undefined;
        try m.open(gpa, &env, path);
        defer m.close();
        try testing.expect(m.be.replayStats().clean_unmount);
        try testing.expectEqual(d_ino, (try Mount.entry(m.call(.{ .lookup = .{ .name = "dir" } }, 1))).nodeid);
        try testing.expectEqual(f_ino, (try Mount.entry(m.call(.{ .lookup = .{ .name = "moved.txt" } }, d_ino))).nodeid);
        const rin: abi.fuse_read_in = .{ .fh = 1, .offset = 7, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        try testing.expectEqualStrings("world!", m.call(.{ .read = rin }, f_ino).body);
        try testing.expectEqualStrings("v1", m.call(.{ .getxattr = .{ .name = "user.tag", .size = 64 } }, f_ino).body);
        try testing.expectEqual(@as(u32, 3), (try Mount.attr(m.call(.{ .getattr = .{ .flags = 0, .fh = null } }, 1))).nlink);
        const st = abi.decodeExact(abi.fuse_statfs_out, m.call(.statfs, 1).outBytes()) orelse return error.TestBadReply;
        try testing.expect(st.st.blocks > 0 and st.st.bfree <= st.st.blocks);
    }
}
