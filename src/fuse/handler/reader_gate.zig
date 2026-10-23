//! The `-o ro` reader's fail-stop gate over the filesystem (charter §5.5
//! "with NO live writer a `-o ro` mount is the QUIESCENT reader of the
//! checkpointed projection that fail-stops `ESTALE` at the first poll that
//! sees a writer"; `meta/sym/tokens/ro_mount.QuiescentReader.admit`): a
//! `Filesystem` over another whose every op asks the quiescent reader's
//! gate first — after the fail-stop every metadata op answers the typed
//! `ReaderFailStopped` (ESTALE), the root's attributes and the virtual
//! control inodes excepted so `.stats` stays readable (the operator reads
//! WHY the mount went stale). PORTABLE: the gate is a function pointer, the
//! wrapper a comptime-built vtable.

const std = @import("std");
const fs_mod = @import("filesystem.zig");
const virtual_inodes = @import("virtual_inodes.zig");
const op_trace = @import("op_trace.zig");
const inos = @import("../../meta/inos.zig");

const Filesystem = fs_mod.Filesystem;
const VTable = fs_mod.VTable;

/// The gate's word on an op: true = refused (the typed ESTALE).
pub const Gate = struct {
    ctx: *anyopaque,
    failStopped: *const fn (ctx: *anyopaque) bool,
};

/// What the gate judges an op by: its op name and the inode it names.
const Target = struct { parent: ?u64, ino: ?u64, name: ?[]const u8 };

fn targetOf(comptime Args: type, args: Args) Target {
    var t: Target = .{ .parent = null, .ino = null, .name = null };
    if (@hasField(Args, "parent")) t.parent = args.parent;
    if (@hasField(Args, "ino")) t.ino = args.ino;
    if (@hasField(Args, "name")) t.name = args.name;
    return t;
}

/// The exception set: the root's attributes (`getattr(1)`), a lookup of a
/// virtual name under the root, any op on a virtual inode.
fn exempt(comptime name: []const u8, t: Target) bool {
    if (t.ino) |ino| {
        if (virtual_inodes.isVirtualIno(ino)) return true;
        if (std.mem.eql(u8, name, "getattr") and ino == inos.ROOT_INO) return true;
    }
    if (t.parent) |p| if (p == inos.ROOT_INO) if (t.name) |n| if (virtual_inodes.classOfName(n) != null) return true;
    return false;
}

pub const ReaderGate = struct {
    inner: Filesystem,
    gate: Gate,

    pub fn filesystem(self: *ReaderGate) Filesystem {
        return .{ .impl = self, .vtable = &vt, .export_support = self.inner.export_support };
    }

    const vt: VTable = build();

    fn build() VTable {
        var v: VTable = .{};
        const info = @typeInfo(VTable).@"struct";
        inline for (info.field_names, info.field_types) |name, FnPtr| {
            @field(v, name) = &Thunk(name, FnPtr).call;
        }
        return v;
    }

    fn Thunk(comptime name: []const u8, comptime FnPtr: type) type {
        const Fn = @typeInfo(FnPtr).pointer.child;
        const info = @typeInfo(Fn).@"fn";
        const Args = info.param_types[2].?;
        const R = info.return_type.?;
        const replies = @typeInfo(R) == .error_union;
        const passes_always = std.mem.eql(u8, name, "init");
        return struct {
            fn call(impl: *anyopaque, c: *fs_mod.Ctx, args: Args) R {
                const self: *ReaderGate = @ptrCast(@alignCast(impl));
                // The no-reply ops (init, destroy, forget) pass: nothing to
                // refuse, nothing a stale answer could mislead.
                if (replies and !passes_always) {
                    if (self.gate.failStopped(self.gate.ctx) and !exempt(name, targetOf(Args, args))) return error.ReaderFailStopped;
                }
                return @field(self.inner.vtable, name)(self.inner.impl, c, args);
            }
        };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const Probe = struct {
    lookups: u32 = 0,
    getattrs: u32 = 0,
    reads: u32 = 0,
    forgets: u32 = 0,

    pub fn lookup(self: *Probe, _: *fs_mod.Ctx, _: fs_mod.LookupArgs) fs_mod.Error!fs_mod.Entry {
        self.lookups += 1;
        return error.FileNotFound;
    }
    pub fn getattr(self: *Probe, _: *fs_mod.Ctx, _: fs_mod.GetattrArgs) fs_mod.Error!fs_mod.Attr {
        self.getattrs += 1;
        return error.FileNotFound;
    }
    pub fn read(self: *Probe, _: *fs_mod.Ctx, _: fs_mod.ReadArgs) fs_mod.Error!fs_mod.Data {
        self.reads += 1;
        return error.FileNotFound;
    }
    pub fn forget(self: *Probe, _: *fs_mod.Ctx, _: fs_mod.ForgetArgs) void {
        self.forgets += 1;
    }
};

const Stopped = struct {
    stopped: bool = false,
    fn failStopped(ctx: *anyopaque) bool {
        const self: *Stopped = @ptrCast(@alignCast(ctx));
        return self.stopped;
    }
};

test "the reader gate passes every op through until the fail-stop; after it every op answers ReaderFailStopped except getattr(root), a virtual lookup under the root, ops on a virtual inode and the no-reply ops" {
    var probe: Probe = .{};
    var stopped: Stopped = .{};
    var g: ReaderGate = .{ .inner = Filesystem.of(Probe, &probe), .gate = .{ .ctx = &stopped, .failStopped = &Stopped.failStopped } };
    const fs = g.filesystem();
    var tr = op_trace.OpTrace.begin(1, .lookup);
    var win: [16]u8 = undefined;
    var c: fs_mod.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &tr, .wd_slot = null, .window = &win };
    try testing.expectError(error.FileNotFound, fs.vtable.lookup(fs.impl, &c, .{ .parent = 1, .name = "x" }));
    try testing.expectError(error.FileNotFound, fs.vtable.getattr(fs.impl, &c, .{ .ino = 7, .fh = null }));
    try testing.expectEqual(@as(u32, 1), probe.lookups);
    try testing.expectEqual(@as(u32, 1), probe.getattrs);
    stopped.stopped = true;
    try testing.expectError(error.ReaderFailStopped, fs.vtable.lookup(fs.impl, &c, .{ .parent = 1, .name = "x" }));
    try testing.expectError(error.ReaderFailStopped, fs.vtable.getattr(fs.impl, &c, .{ .ino = 7, .fh = null }));
    try testing.expectError(error.ReaderFailStopped, fs.vtable.read(fs.impl, &c, .{ .ino = 7, .fh = 0, .offset = 0, .size = 1, .flags = 0 }));
    try testing.expectError(error.ReaderFailStopped, fs.vtable.statfs(fs.impl, &c, .{ .ino = 1 }));
    try testing.expectEqual(@as(u32, 1), probe.lookups);
    // The exceptions reach the inner filesystem.
    try testing.expectError(error.FileNotFound, fs.vtable.getattr(fs.impl, &c, .{ .ino = inos.ROOT_INO, .fh = null }));
    try testing.expectError(error.FileNotFound, fs.vtable.lookup(fs.impl, &c, .{ .parent = 1, .name = virtual_inodes.STATS_NAME }));
    const vino = virtual_inodes.inoOf(1, .stats);
    try testing.expectError(error.FileNotFound, fs.vtable.read(fs.impl, &c, .{ .ino = vino, .fh = 0, .offset = 0, .size = 1, .flags = 0 }));
    try testing.expectEqual(@as(u32, 2), probe.getattrs);
    try testing.expectEqual(@as(u32, 2), probe.lookups);
    try testing.expectEqual(@as(u32, 1), probe.reads);
    fs.vtable.forget(fs.impl, &c, .{ .ino = 7, .nlookup = 1, .evicted = true });
    try testing.expectEqual(@as(u32, 1), probe.forgets);
}
