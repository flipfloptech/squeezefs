//! `squeezefs clients [--json] <sqmeta uri|mountpoint>` (charter §8;
//! `docs/porting/stats_cli.md` §4.7): the registrations of every metadata
//! volume of the set — the writer claim under the ONE staleness law and the
//! ONE dead-holder law (`meta/guard`), read OFFLINE off the device without
//! the flock (a read-only probe beside a live daemon is legal), classified
//! `live` / `stale` / `dead`. Rung 6's membership census appends readers
//! through the guard's `RegistrationSource` seam. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const target = @import("target.zig");
const guard = @import("../meta/guard.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/clients is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

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
        const rows = guard.gate.clients(p.gpa, p.io, p.env, pool, path, null) catch |e| {
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
