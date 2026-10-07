//! `squeezefs staging list [--json] <target>` and `squeezefs staging
//! promote <sqmeta uri>` (charter §8 `staging`; `docs/porting/stats_cli.md`
//! §4.4 — the port adds `list`, which the reference lacked).
//!
//! `list`: this node's staging roots for the set as the set config records
//! them, each with its marker (the set uuid, epoch, mount slot — `bound`
//! / `foreign` / `none`), the ring segment's size, and the live custody
//! units a recovery scan finds (records, bytes, torn) — read-only, no
//! guard (the scan maps the segment and adopts nothing).
//!
//! `promote`: the DISMOUNT PASS run offline — the set opened through the
//! writer door with the data plane, which recovers the ring's residue (a
//! previous daemon's acked-but-unpromoted bytes), then closed: every staged
//! file promotes to its own block / the pack, the packs seal, the
//! reclaimer drains. The "drain" of a staged set IS this pass (and `fsync`
//! / `umount` on a live mount); there is no separate verb for it. Linux
//! only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const target = @import("target.zig");
const status = @import("status.zig");
const offline = @import("offline.zig");
const set_config = @import("../data/set_config.zig");
const staging = @import("../data/staging.zig");
const plane_mod = @import("../data/plane.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/staging_verb is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, s: args.Staging) u8 {
    return switch (s) {
        .list => |l| list(p, l.target, l.json),
        .promote => |pr| promote(p, pr.target),
    };
}

pub const Binding = enum { none, bound, foreign, unreadable };

pub const RootRow = struct {
    path: []const u8,
    binding: Binding,
    mount_slot: u32,
    set_epoch: u64,
    segment_bytes: u64,
    live_units: u64,
    live_bytes: u64,
    torn: u64,

    pub fn writeJson(self: RootRow, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"dir\":\"{s}\",\"binding\":\"{s}\",\"mount_slot\":{d},\"set_epoch\":{d},\"segment_bytes\":{d},\"live_units\":{d},\"live_bytes\":{d},\"torn\":{d}}}", .{ self.path, @tagName(self.binding), self.mount_slot, self.set_epoch, self.segment_bytes, self.live_units, self.live_bytes, self.torn });
    }

    pub fn writeText(self: RootRow, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{s}: {s}", .{ self.path, @tagName(self.binding) });
        if (self.binding == .bound) try w.print(" (mount slot {d}, set epoch {d})", .{ self.mount_slot, self.set_epoch });
        try w.print("; segment {d} MiB; {d} live custody unit(s), {d} B; {d} torn\n", .{ self.segment_bytes >> 20, self.live_units, self.live_bytes, self.torn });
    }
};

/// The marker's verdict against the set's uuid (pure).
pub fn bindingOf(marker: ?staging.Marker, set_uuid: [16]u8) Binding {
    const m = marker orelse return .none;
    return if (std.mem.eql(u8, &m.set_uuid, &set_uuid)) .bound else .foreign;
}

fn segmentBytes(root: []const u8) u64 {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&buf, "{s}/{s}/{s}", .{ root, staging.Ring.SEGMENT_DIR, plane_mod.STAGING_SEGMENT_NAME }, 0) catch return 0;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z.ptr, 0, .{ .SIZE = true }, &st)) != .SUCCESS) return 0;
    return st.size;
}

/// One root's row: the marker, the segment, the recovery scan (a mapped
/// segment adopts into a private index that is freed at once).
fn inspectRoot(gpa: std.mem.Allocator, pool: *uring_fs.Pool, root: []const u8, set_uuid: [16]u8) RootRow {
    var row: RootRow = .{ .path = root, .binding = .none, .mount_slot = 0, .set_epoch = 0, .segment_bytes = segmentBytes(root), .live_units = 0, .live_bytes = 0, .torn = 0 };
    const marker = staging.Ring.readMarker(pool, root) catch {
        row.binding = .unreadable;
        return row;
    };
    row.binding = bindingOf(marker, set_uuid);
    if (marker) |m| {
        row.mount_slot = m.mount_slot;
        row.set_epoch = m.set_epoch;
    }
    if (row.segment_bytes == 0) return row;
    const root_fd = staging.Ring.openRoot(root) catch return row;
    defer _ = linux.close(root_fd);
    var mapped = staging.Ring.openSegment(root_fd, plane_mod.STAGING_SEGMENT_NAME, row.segment_bytes) catch return row;
    defer mapped.close();
    var seg = staging.Segment.init(gpa, mapped.map, mapped.backing()) catch return row;
    defer seg.deinit();
    const rep = seg.recover();
    row.live_units = rep.adopted;
    row.live_bytes = rep.bytes;
    row.torn = rep.torn;
    return row;
}

fn list(p: cli.Process, tgt: args.Target, json: bool) u8 {
    var resolved = target.resolve(p.gpa, p.io, tgt) catch |e| {
        target.writeRefusal(p.stderr, "staging list", tgt, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs staging list: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const path = meta.first();
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs staging list: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var reason_buf: [512]u8 = undefined;
    const snap = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
        p.stderr.print("squeezefs staging list: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    const sb = snap.sb orelse {
        p.stderr.print("squeezefs staging list: {s} is not a formatted v4 metadata volume\n", .{path}) catch {};
        return cli.exit_failed;
    };
    var rec = (status.newestLedger(p.gpa, p.env, pool, path, sb) catch |e| {
        p.stderr.print("squeezefs staging list: cannot read the ledger of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    }) orelse {
        p.stderr.print("squeezefs staging list: {s} has no valid ledger record\n", .{path}) catch {};
        return cli.exit_failed;
    };
    defer rec.deinit(p.gpa);
    var cfg_reason: [512]u8 = undefined;
    const read = status.readSetConfig(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg_reason) catch |e| {
        p.stderr.print("squeezefs staging list: cannot read the control tree of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    var cfg = switch (read) {
        .config => |c| c,
        .absent => {
            p.stderr.print("squeezefs staging list: {s} carries no set_config record\n", .{path}) catch {};
            return cli.exit_failed;
        },
        .refused => |why| {
            p.stderr.print("squeezefs staging list: {s}: set config refused: {s}\n", .{ path, why }) catch {};
            return cli.exit_failed;
        },
    };
    defer cfg.deinit(p.gpa);
    if (json) p.stdout.writeAll("{\"staging_roots\":[") catch {};
    if (!json and cfg.cache_paths.len == 0) p.stdout.writeAll("cache-less set: no staging roots\n") catch {};
    for (cfg.cache_paths, 0..) |root, i| {
        const row = inspectRoot(p.gpa, pool, root, sb.set_uuid);
        if (json) {
            if (i != 0) p.stdout.writeAll(",") catch {};
            row.writeJson(p.stdout) catch return cli.exit_failed;
        } else row.writeText(p.stdout) catch return cli.exit_failed;
    }
    if (json) p.stdout.writeAll("]}\n") catch {};
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

const PromoteBody = struct {
    p: cli.Process,

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *PromoteBody = @ptrCast(@alignCast(ctx));
        // The plane recovered the ring at open; its close IS the dismount
        // pass (promote → seal → drain). The verdict reads off the gauges.
        const files0 = staging.stats.dismount_promoted_files.load();
        const bytes0 = staging.stats.dismount_promoted_bytes.load();
        const failed0 = staging.stats.dismount_promote_failures.load();
        const live = staging.stats.staged_writes_in_flight.load();
        set.plane.?.close();
        set.plane = null;
        const files = staging.stats.dismount_promoted_files.load() - files0;
        const bytes = staging.stats.dismount_promoted_bytes.load() - bytes0;
        const failed = staging.stats.dismount_promote_failures.load() - failed0;
        self.p.stdout.print("staging promote {s}: {d} staged file(s) found, {d} promoted ({d} B), {d} failed\n", .{ set.path, live, files, bytes, failed }) catch {};
        self.p.stdout.flush() catch {};
        return if (failed != 0) cli.exit_failed else cli.exit_ok;
    }
};

fn promote(p: cli.Process, meta_uri: []const u8) u8 {
    var body: PromoteBody = .{ .p = p };
    return offline.withSet(p, meta_uri, .{ .with_plane = true, .verb = "staging promote" }, .{ .ctx = &body, .run = PromoteBody.run });
}

const testing = std.testing;

test "the binding verdict: no marker → none; the set's uuid → bound; another → foreign; the rows render" {
    const u: [16]u8 = @splat(7);
    try testing.expectEqual(Binding.none, bindingOf(null, u));
    try testing.expectEqual(Binding.bound, bindingOf(.{ .set_uuid = u, .set_epoch = 3, .node_token = 1, .mount_slot = 2 }, u));
    try testing.expectEqual(Binding.foreign, bindingOf(.{ .set_uuid = @splat(9), .set_epoch = 3, .node_token = 1, .mount_slot = 2 }, u));
    var out: Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    const row: RootRow = .{ .path = "/c", .binding = .bound, .mount_slot = 2, .set_epoch = 3, .segment_bytes = 64 << 20, .live_units = 5, .live_bytes = 4096, .torn = 0 };
    try row.writeJson(&out.writer);
    try testing.expectEqualStrings("{\"dir\":\"/c\",\"binding\":\"bound\",\"mount_slot\":2,\"set_epoch\":3,\"segment_bytes\":67108864,\"live_units\":5,\"live_bytes\":4096,\"torn\":0}", out.written());
    out.clearRetainingCapacity();
    try row.writeText(&out.writer);
    try testing.expectEqualStrings("/c: bound (mount slot 2, set epoch 3); segment 64 MiB; 5 live custody unit(s), 4096 B; 0 torn\n", out.written());
    try testing.expectEqual(@as(u64, 0), segmentBytes("/build/tmp/no-such-staging-root"));
}
