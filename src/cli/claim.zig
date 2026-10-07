//! `squeezefs claim clear <sqmeta uri>` (charter §8; §5.7 the D0 ladder's
//! operator rung; `docs/porting/stats_cli.md` §4.5): per volume of the URI,
//! in set order, the guard's own door in `.claim_clear` mode — the flock
//! (held ⇒ "live-mounted" refusal), the fresh-claim and own-claim refusals,
//! else the stale claim removed durably. The verb prints one line per
//! volume and exits 1 when any volume refused: "No mount flag bypasses the
//! guard" — this verb is the operator's attestation. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const cli = @import("cli.zig");
const args = @import("args.zig");
const guard = @import("../meta/guard.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/claim is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, c: args.ClaimClear) u8 {
    var report = guard.gate.claimClear(p.gpa, p.io, p.env, c.meta_uri, .{}) catch |e| {
        switch (e) {
            error.InvalidUri => p.stderr.print("squeezefs claim clear: {s} is not a sqmeta:// URI of absolute paths\n", .{c.meta_uri}) catch {},
            error.OutOfMemory => p.stderr.writeAll("squeezefs claim clear: out of memory\n") catch {},
            error.ExecutorInit => p.stderr.writeAll("squeezefs claim clear: the executor could not start\n") catch {},
        }
        return cli.exit_failed;
    };
    defer report.deinit();
    report.write(p.stdout) catch return cli.exit_failed;
    var refused: usize = 0;
    for (report.volumes) |v| {
        if (v.refused()) refused += 1;
    }
    if (refused != 0) {
        p.stderr.print("squeezefs claim clear: refused/failed on {d} of {d} volume(s)\n", .{ refused, report.volumes.len }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_failed;
    }
    p.stdout.flush() catch {};
    return cli.exit_ok;
}
