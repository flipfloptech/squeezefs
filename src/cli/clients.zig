//! `squeezefs clients [--json] <sqmeta uri|mountpoint>` (charter §8;
//! `docs/porting/stats_cli.md` §4.7): the registrations of every metadata
//! volume of the set — the writer claim under the ONE staleness law and the
//! ONE dead-holder law (`meta/guard`), read OFFLINE off the device without
//! the flock (a read-only probe beside a live daemon is legal), classified
//! `live` / `stale` / `dead` — plus rung 6's membership rows: every
//! `member:` record of the volume's tree 0 (a writer that JOINED the
//! volume's manager over the wire, `WriterClaimV1.kind = 1`) through the
//! same classifier. A member row carries no heartbeat (it is written ONCE
//! per change — the beats are the owner's RAM), so offline its state is
//! the dead-pid proof's: a same-host member whose pid is alive is `live`,
//! one whose pid is gone is `dead`, another host's is `stale` (unknown
//! here; its home shard's owner knows). Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const target = @import("target.zig");
const status = @import("status.zig");
const guard = @import("../meta/guard.zig");
const proc = @import("../meta/guard/proc.zig");
const tree = @import("../meta/tree.zig");
const cr = @import("../meta/sym/control_records.zig");
const shard = @import("../meta/sym/membership/shard.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");
const clock = @import("../core/clock.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/clients is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The durable member rows of one volume as registrations (module doc).
const MemberRows = struct {
    p: cli.Process,
    pool: *uring_fs.Pool,
    path: []const u8,

    fn source(self: *MemberRows) guard.claim.RegistrationSource {
        return .{ .ctx = self, .append = &append };
    }

    fn append(ctx: ?*anyopaque, gpa: std.mem.Allocator, out: *std.ArrayList(guard.claim.Registration)) std.mem.Allocator.Error!void {
        const self: *MemberRows = @ptrCast(@alignCast(ctx.?));
        var reason: [512]u8 = undefined;
        const snap = status.classify(gpa, self.p.env, self.pool, self.path, &reason) catch return;
        const sb = snap.sb orelse return;
        const rec = (status.newestLedger(gpa, self.p.env, self.pool, self.path, sb) catch return) orelse return;
        const r = cr.rangeOf(.member);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        status.readControlRange(gpa, self.p.io, self.p.env, self.pool, self.path, sb, &rec, r.start.slice(), r.end.slice(), &kvs) catch return;
        const local = proc.localIdentity();
        const now: i64 = @intCast(clock.realtimeCoarseNs());
        for (kvs.items) |kv| {
            const row = shard.MemberRow.decode(kv.value) catch continue;
            // Offline, "live" is the same-host pid proof; another host's
            // member is unknown here (`stale`).
            try out.append(gpa, row.registration(local, now, proc.probe, row.pidAlive(local, proc.probe) orelse false));
        }
    }
};

pub fn run(p: cli.Process, c: args.Clients) u8 {
    var resolved = target.resolve(p.gpa, p.io, c.target) catch |e| {
        target.writeRefusal(p.stderr, "clients", c.target, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs clients: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs clients: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    if (c.json) p.stdout.writeAll("[") catch {};
    for (meta.paths, 0..) |path, i| {
        var rows_src: MemberRows = .{ .p = p, .pool = pool, .path = path };
        const rows = guard.gate.clients(p.gpa, p.io, p.env, pool, path, rows_src.source()) catch |e| {
            p.stderr.print("squeezefs clients: cannot probe metadata volume '{s}': {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        defer p.gpa.free(rows);
        if (c.json) {
            if (i != 0) p.stdout.writeAll(",") catch {};
            guard.claim.writeClientsJson(p.stdout, rows, path, meta.paths.len) catch return cli.exit_failed;
        } else {
            guard.claim.writeClientsText(p.stdout, rows, path, meta.paths.len) catch return cli.exit_failed;
        }
    }
    if (c.json) p.stdout.writeAll("]\n") catch {};
    p.stdout.flush() catch {};
    return cli.exit_ok;
}
