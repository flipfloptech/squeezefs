//! The set-level verbs' ONE target resolution (`docs/porting/stats_cli.md`
//! §2, §7 Q8/Q9): a positional that is either a `sqmeta://` URI or a
//! MOUNTPOINT. A mountpoint resolves through the live mount's `.config`
//! inode — its `sqmeta_uri` is "the exact string the CLI verbs accept" —
//! never through a cache directory and never through an env default.
//! Linux only (it reads a mounted filesystem).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const args = @import("args.zig");
const uri = @import("uri.zig");
const json_scan = @import("../core/json_scan.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/target is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// `.config` is small (the set summary); a 64 KiB bound is 100× its size.
const config_read_bound: usize = 64 << 10;

pub const ResolveError = error{
    /// The mountpoint has no readable `.config` (not a squeezefs mount, or
    /// the daemon is gone).
    NotMounted,
    /// `.config` carries no `sqmeta_uri`.
    ConfigWithoutUri,
    OutOfMemory,
};

pub const Resolved = struct {
    /// The URI (owned when resolved from a mountpoint).
    meta_uri: []const u8,
    owned: ?[]u8,
    /// Whether the target named a live mount.
    via_mount: bool,

    pub fn deinit(self: *Resolved, gpa: std.mem.Allocator) void {
        if (self.owned) |o| gpa.free(o);
        self.* = undefined;
    }
};

/// A URI passes through; a mountpoint reads `<mountpoint>/.config`.
pub fn resolve(gpa: std.mem.Allocator, io: Io, target: args.Target) ResolveError!Resolved {
    if (!args.targetIsMountpoint(target)) return .{ .meta_uri = target, .owned = null, .via_mount = false };
    const path = try std.fmt.allocPrint(gpa, "{s}/.config", .{target});
    defer gpa.free(path);
    const text = Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(config_read_bound)) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.NotMounted,
    };
    defer gpa.free(text);
    const found = json_scan.string(text, "sqmeta_uri") orelse return error.ConfigWithoutUri;
    if (found.len == 0 or uri.schemeOf(found) != .meta) return error.ConfigWithoutUri;
    const owned = try gpa.dupe(u8, found);
    return .{ .meta_uri = owned, .owned = owned, .via_mount = true };
}

/// The refusal text, one line.
pub fn writeRefusal(w: *Io.Writer, verb: []const u8, target: []const u8, e: ResolveError) Io.Writer.Error!void {
    switch (e) {
        error.NotMounted => try w.print("squeezefs {s}: {s} is not a live squeezefs mount (no readable {s}/.config) — pass the sqmeta:// URI instead\n", .{ verb, target, target }),
        error.ConfigWithoutUri => try w.print("squeezefs {s}: {s}/.config names no sqmeta_uri — a daemon of another build?\n", .{ verb, target }),
        error.OutOfMemory => try w.print("squeezefs {s}: out of memory\n", .{verb}),
    }
}

const testing = std.testing;

test "a URI passes through unowned; a non-mount directory is NotMounted; a file is not a mount" {
    var r = try resolve(testing.allocator, testing.io, "sqmeta:///dev/m");
    defer r.deinit(testing.allocator);
    try testing.expectEqualStrings("sqmeta:///dev/m", r.meta_uri);
    try testing.expect(!r.via_mount and r.owned == null);
    try testing.expectError(error.NotMounted, resolve(testing.allocator, testing.io, "/build/tmp/not-a-mount-target-test"));
    try testing.expectError(error.NotMounted, resolve(testing.allocator, testing.io, "/proc/self"));
}
