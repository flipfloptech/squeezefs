//! `squeezefs clone <sqmeta uri|mountpoint> /src /dest` (charter §8
//! "instant CoW"; `docs/porting/stats_cli.md` §4.1): OFFLINE, under the D0
//! guard — the set opened through the writer door (a live mount refuses by
//! name), both paths resolved through the filesystem's own lookups, the
//! source refused if it is the root, a directory, a staged file (its acked
//! payload lives in the mount's isolated staging) or tree-mapped, then
//! `KvFs.cloneFile`: a NEW dest inode, the layout record copied, ONE
//! transaction with a durable SHARED `+ref` per binding on BOTH owners
//! (refcount 1 → 2), zero bytes DMA'd. Output: `cloned <src> → <dest>: <n>
//! block(s) shared, <b> bytes copied`. A mountpoint target resolves to the
//! set's URI and then refuses as live-mounted: the live clone is rung 7's
//! `copy_file_range` (`cp --reflink=always`). Linux only.

const std = @import("std");
const builtin = @import("builtin");
const cli = @import("cli.zig");
const args = @import("args.zig");
const target = @import("target.zig");
const offline = @import("offline.zig");
const handler = @import("../fuse/handler.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/clone is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const Body = struct {
    p: cli.Process,
    src: []const u8,
    dest: []const u8,

    fn refuse(self: *Body, comptime fmt: []const u8, a: anytype) u8 {
        self.p.stderr.writeAll("squeezefs clone: refused: ") catch {};
        self.p.stderr.print(fmt, a) catch {};
        self.p.stderr.writeAll("\n") catch {};
        return cli.exit_failed;
    }

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *Body = @ptrCast(@alignCast(ctx));
        const fs = set.fs.?;
        var trace = handler.op_trace.OpTrace.begin(0, .other);
        defer trace.finish();
        var c = set.ctx(&trace);
        const src = (offline.walk(fs, &c, self.src) catch |e| return self.refuse("source '{s}' cannot be resolved ({t})", .{ self.src, e })) orelse return self.refuse("source '{s}' cannot be resolved — a component is missing", .{self.src});
        const src_ino = src.ino orelse return self.refuse("source '{s}' does not exist", .{self.src});
        if (src_ino == 1) return self.refuse("source '{s}' resolves to the filesystem root — name a file", .{self.src});
        const dest = (offline.walk(fs, &c, self.dest) catch |e| return self.refuse("destination '{s}' cannot be resolved ({t})", .{ self.dest, e })) orelse return self.refuse("destination '{s}': its parent directory does not exist", .{self.dest});
        if (dest.ino != null) return self.refuse("destination '{s}' already exists", .{self.dest});
        const out = fs.cloneFile(&c, src_ino, dest.parent, dest.leaf) catch |e| return switch (e) {
            error.NotAFile => self.refuse("source '{s}' is not a regular file", .{self.src}),
            error.StagedSource => self.refuse("source '{s}' is a STAGED file — its acked payload lives in the mount's isolated staging; cloning it would mint a zero-filled clone. Mount and `fsync`/unmount to promote it first", .{self.src}),
            error.TreeMappedSource => self.refuse("source '{s}' is tree-mapped (past the inline block map); the block-map tree's clone is owed", .{self.src}),
            else => self.refuse("'{s}' → '{s}': {t}", .{ self.src, self.dest, e }),
        };
        self.p.stdout.print("cloned {s} → {s}: {d} block(s) shared, {d} bytes copied (dest ino {d}; metadata-only CoW, nothing DMA'd)\n", .{ self.src, self.dest, out.blocks_shared, out.inline_bytes, out.dest_ino }) catch {};
        self.p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

pub fn run(p: cli.Process, c: args.Clone) u8 {
    var resolved = target.resolve(p.gpa, p.io, c.target) catch |e| {
        target.writeRefusal(p.stderr, "clone", c.target, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var body: Body = .{ .p = p, .src = c.src, .dest = c.dest };
    return offline.withSet(p, resolved.meta_uri, .{ .with_plane = true, .verb = "clone" }, .{ .ctx = &body, .run = Body.run });
}
