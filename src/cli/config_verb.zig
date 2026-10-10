//! `squeezefs config show [--json] <target>` and `squeezefs config
//! set-cache-paths [--force] <target> <paths>` (charter §8; `docs/porting/
//! stats_cli.md` §4.3): the set's ONE control record (`set_config`, tree 0
//! of member 0). `show` reads it OFFLINE at the newest checkpoint (the
//! `status` pattern; a mountpoint resolves through `.config`). `set-cache-
//! paths` runs through `set_config.setCachePaths` (the pure verb: the
//! denylist, no repeats, the generation bump) and lands as ONE control-tree
//! transaction under the writer guard, AFTER every new directory passed the
//! precheck (ALL dirs before ANY is created: absolute, outside the system
//! roots, creatable 0700; a non-empty directory with no staging marker
//! needs `--force` — a stranger's data is never adopted silently). Linux
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
const key = @import("../meta/key.zig");
const geometry = @import("../meta/geometry.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");
const align_probe = @import("../io/align_probe.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/config_verb is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, c: args.Config) u8 {
    return switch (c) {
        .show => |s| show(p, s.target, s.json),
        .set_cache_paths => |s| setCachePaths(p, s.target, s.paths, s.force),
        .set_fabric_endpoints => |s| setFabricEndpoints(p, s.target, s.specs.slice()),
        .get_fabric_endpoints => |g| getFabricEndpoints(p, g.target, g.json),
    };
}

// ---------------------------------------------------------------------------
// `fabric_endpoint` records (`ops_nvmet_keys.md` §3.7, the proposed design):
// one per DATA volume inside the set config — never a second home.
// ---------------------------------------------------------------------------

const FabricBody = struct {
    p: cli.Process,
    specs: []const []const u8,

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *FabricBody = @ptrCast(@alignCast(ctx));
        const p = self.p;
        var cur = set.cfg.clone(p.gpa) catch {
            p.stderr.writeAll("squeezefs config set-fabric-endpoints: out of memory\n") catch {};
            return cli.exit_failed;
        };
        defer cur.deinit(p.gpa);
        for (self.specs) |word| {
            const spec = args.parseEndpointSpec(word) orelse unreachable; // the grammar admitted it
            var ep: set_config.FabricEndpoint = .{ .trtype = if (spec.transport == .tcp) .tcp else .loop, .adrfam = .none, .trsvcid = spec.trsvcid, .traddr = @splat(0), .subnqn = spec.subnqn };
            if (spec.transport == .tcp) {
                if (std.Io.net.Ip4Address.parse(spec.traddr, 0)) |v4| {
                    ep.adrfam = .ipv4;
                    ep.traddr[0..4].* = v4.bytes;
                } else |_| if (std.Io.net.Ip6Address.parse(spec.traddr, 0)) |v6| {
                    ep.adrfam = .ipv6;
                    ep.traddr = v6.bytes;
                } else |_| {
                    p.stderr.print("squeezefs config set-fabric-endpoints: '{s}': traddr '{s}' is not an IPv4/IPv6 address\n", .{ word, spec.traddr }) catch {};
                    return cli.exit_failed;
                }
            }
            var m = set_config.setFabricEndpoint(p.gpa, &cur, spec.vol_tag, ep) catch |e| {
                p.stderr.print("squeezefs config set-fabric-endpoints: '{s}': {s}\n", .{ word, switch (e) {
                    error.UnknownVolume => "no data volume with that vol-tag in the set (`squeezefs config show`)",
                    error.IllegalTransition => "the volume is retired",
                    error.BadFabricEndpoint => "the subnqn or transport shape is not one the record represents",
                    else => @errorName(e),
                } }) catch {};
                return cli.exit_failed;
            };
            cur.deinit(p.gpa);
            cur = m.next;
            m.next = undefined;
        }
        const bytes = set_config.encode(p.gpa, &cur, @intCast(geometry.recordValueCap(set.be.nodeSize()))) catch |e| {
            p.stderr.print("squeezefs config set-fabric-endpoints: the record would not encode: {s}\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        defer p.gpa.free(bytes);
        const t = set.be.begin(@src()) catch {
            p.stderr.writeAll("squeezefs config set-fabric-endpoints: out of memory\n") catch {};
            return cli.exit_failed;
        };
        t.stagePut(key.TREE_CONTROL, set_config.CONTROL_KEY, bytes) catch {
            set.be.abandon(t);
            p.stderr.writeAll("squeezefs config set-fabric-endpoints: out of memory\n") catch {};
            return cli.exit_failed;
        };
        set.be.commit(t) catch |e| {
            p.stderr.print("squeezefs config set-fabric-endpoints: the record commit failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        set.be.fsync() catch |e| {
            p.stderr.print("squeezefs config set-fabric-endpoints: the durability barrier failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("fabric endpoints set (config generation {d}):\n", .{cur.config_generation}) catch {};
        for (cur.data) |d| if (d.fabric) |f| {
            var ab: [48]u8 = undefined;
            p.stdout.print("  vol-{x:0>16} {s}:{s}:{d}:{s}\n", .{ d.vol_tag, @tagName(f.trtype), f.traddrText(&ab), f.trsvcid, f.subnqn }) catch {};
        };
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

fn setFabricEndpoints(p: cli.Process, tgt: args.Target, specs: []const []const u8) u8 {
    var resolved = target.resolve(p.gpa, p.io, tgt) catch |e| {
        target.writeRefusal(p.stderr, "config set-fabric-endpoints", tgt, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var body: FabricBody = .{ .p = p, .specs = specs };
    return offline.withSet(p, resolved.meta_uri, .{ .with_plane = false, .verb = "config set-fabric-endpoints" }, .{ .ctx = &body, .run = FabricBody.run });
}

fn getFabricEndpoints(p: cli.Process, tgt: args.Target, json: bool) u8 {
    var resolved = target.resolve(p.gpa, p.io, tgt) catch |e| {
        target.writeRefusal(p.stderr, "config get-fabric-endpoints", tgt, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs config get-fabric-endpoints: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const path = meta.first();
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs config get-fabric-endpoints: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var reason_buf: [512]u8 = undefined;
    const snap = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
        p.stderr.print("squeezefs config get-fabric-endpoints: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    const sb = snap.sb orelse {
        p.stderr.print("squeezefs config get-fabric-endpoints: {s} is not a formatted v4 metadata volume\n", .{path}) catch {};
        return cli.exit_failed;
    };
    var rec = (status.newestLedger(p.gpa, p.env, pool, path, sb) catch null) orelse {
        p.stderr.print("squeezefs config get-fabric-endpoints: {s} has no valid ledger record\n", .{path}) catch {};
        return cli.exit_failed;
    };
    defer rec.deinit(p.gpa);
    var cfg_reason: [512]u8 = undefined;
    const read = status.readSetConfig(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg_reason) catch |e| {
        p.stderr.print("squeezefs config get-fabric-endpoints: cannot read the control tree of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    switch (read) {
        .absent, .refused => {
            p.stderr.print("squeezefs config get-fabric-endpoints: {s} carries no readable set_config record\n", .{path}) catch {};
            return cli.exit_failed;
        },
        .config => |cfg_const| {
            var cfg = cfg_const;
            defer cfg.deinit(p.gpa);
            if (json) {
                p.stdout.writeAll("{\"fabric_endpoints\":[") catch {};
                var first = true;
                for (cfg.data) |d| if (d.fabric) |f| {
                    if (!first) p.stdout.writeByte(',') catch {};
                    first = false;
                    p.stdout.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"endpoint\":", .{d.vol_tag}) catch {};
                    f.writeJson(p.stdout) catch {};
                    p.stdout.writeByte('}') catch {};
                };
                p.stdout.writeAll("]}\n") catch {};
            } else {
                var n: usize = 0;
                for (cfg.data) |d| if (d.fabric) |f| {
                    var ab: [48]u8 = undefined;
                    p.stdout.print("vol-{x:0>16} {s}:{s}:{d}:{s}\n", .{ d.vol_tag, @tagName(f.trtype), f.traddrText(&ab), f.trsvcid, f.subnqn }) catch {};
                    n += 1;
                };
                if (n == 0) p.stdout.writeAll("no fabric endpoints recorded (every data volume is local or operator-connected)\n") catch {};
            }
            p.stdout.flush() catch {};
            return cli.exit_ok;
        },
    }
}

fn show(p: cli.Process, tgt: args.Target, json: bool) u8 {
    var resolved = target.resolve(p.gpa, p.io, tgt) catch |e| {
        target.writeRefusal(p.stderr, "config show", tgt, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs config show: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const path = meta.first();
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs config show: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var reason_buf: [512]u8 = undefined;
    const snap = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
        p.stderr.print("squeezefs config show: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    const sb = snap.sb orelse {
        p.stderr.print("squeezefs config show: {s} is not a formatted v4 metadata volume\n", .{path}) catch {};
        return cli.exit_failed;
    };
    var rec = (status.newestLedger(p.gpa, p.env, pool, path, sb) catch |e| {
        p.stderr.print("squeezefs config show: cannot read the ledger of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    }) orelse {
        p.stderr.print("squeezefs config show: {s} has no valid ledger record\n", .{path}) catch {};
        return cli.exit_failed;
    };
    defer rec.deinit(p.gpa);
    var cfg_reason: [512]u8 = undefined;
    const read = status.readSetConfig(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg_reason) catch |e| {
        p.stderr.print("squeezefs config show: cannot read the control tree of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    switch (read) {
        .absent => {
            p.stderr.print("squeezefs config show: {s} carries no set_config record — not member 0 of a set\n", .{path}) catch {};
            return cli.exit_failed;
        },
        .refused => |why| {
            p.stderr.print("squeezefs config show: {s}: set config refused: {s}\n", .{ path, why }) catch {};
            return cli.exit_failed;
        },
        .config => |cfg_const| {
            var cfg = cfg_const;
            defer cfg.deinit(p.gpa);
            if (json) {
                cfg.writeJson(p.stdout) catch return cli.exit_failed;
                p.stdout.writeAll("\n") catch {};
            } else {
                p.stdout.print("{s} (checkpoint seq {d}):\n", .{ resolved.meta_uri, rec.seq }) catch {};
                cfg.writeSummary(p.stdout) catch return cli.exit_failed;
            }
            p.stdout.flush() catch {};
            return cli.exit_ok;
        },
    }
}

/// ENG-4's directory precheck, pure over what the filesystem says: a
/// directory is admissible when absent (created 0700), when empty, or when
/// it carries a staging marker (ours); a non-empty stranger needs
/// `--force`.
pub const DirVerdict = enum { absent, empty_or_ours, stranger, not_a_directory };

pub fn classifyDir(io: Io, path: []const u8) DirVerdict {
    var d = Io.Dir.cwd().openDir(io, path, .{ .iterate = true }) catch |e| return switch (e) {
        error.FileNotFound => .absent,
        error.NotDir => .not_a_directory,
        else => .stranger,
    };
    defer d.close(io);
    var it = d.iterate();
    var any = false;
    while (it.next(io) catch null) |entry| {
        if (std.mem.eql(u8, entry.name, staging.Ring.MARKER_NAME)) return .empty_or_ours;
        any = true;
    }
    return if (any) .stranger else .empty_or_ours;
}

const SetBody = struct {
    p: cli.Process,
    paths: [set_config.MAX_CACHE_PATHS][]const u8,
    n: usize,
    force: bool,

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *SetBody = @ptrCast(@alignCast(ctx));
        const p = self.p;
        const paths = self.paths[0..self.n];
        // ALL directories prechecked before ANY is created.
        for (paths) |path| switch (classifyDir(p.io, path)) {
            .absent, .empty_or_ours => {},
            .not_a_directory => {
                p.stderr.print("squeezefs config set-cache-paths: cannot change cache paths: {s} exists and is not a directory\n", .{path}) catch {};
                return cli.exit_failed;
            },
            .stranger => if (!self.force) {
                p.stderr.print("squeezefs config set-cache-paths: cannot change cache paths: {s} is a non-empty directory without a staging marker — not ours; pass --force to adopt it (its contents stay untouched; the ring binds by generation)\n", .{path}) catch {};
                return cli.exit_failed;
            },
        };
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        for (paths) |path| {
            const z = align_probe.pathZ(&zbuf, path) catch {
                p.stderr.print("squeezefs config set-cache-paths: {s}: path too long\n", .{path}) catch {};
                return cli.exit_failed;
            };
            const rc = linux.mkdir(z, 0o700);
            const e = linux.errno(rc);
            if (e != .SUCCESS and e != .EXIST) {
                p.stderr.print("squeezefs config set-cache-paths: cannot create {s}: {t}\n", .{ path, e }) catch {};
                return cli.exit_failed;
            }
        }
        const mutation = set_config.setCachePaths(p.gpa, set.cfg, paths) catch |e| {
            p.stderr.print("squeezefs config set-cache-paths: cannot change cache paths: {s}\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        var next = mutation.next;
        defer next.deinit(p.gpa);
        const bytes = set_config.encode(p.gpa, &next, @intCast(geometry.recordValueCap(set.be.nodeSize()))) catch |e| {
            p.stderr.print("squeezefs config set-cache-paths: the record would not encode: {s}\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        defer p.gpa.free(bytes);
        // ONE control-tree transaction, then the barrier (durable before
        // the verb reports success).
        const t = set.be.begin(@src()) catch {
            p.stderr.writeAll("squeezefs config set-cache-paths: out of memory\n") catch {};
            return cli.exit_failed;
        };
        t.stagePut(key.TREE_CONTROL, set_config.CONTROL_KEY, bytes) catch {
            set.be.abandon(t);
            p.stderr.writeAll("squeezefs config set-cache-paths: out of memory\n") catch {};
            return cli.exit_failed;
        };
        set.be.commit(t) catch |e| {
            p.stderr.print("squeezefs config set-cache-paths: the record commit failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        set.be.fsync() catch |e| {
            p.stderr.print("squeezefs config set-cache-paths: the durability barrier failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("cache paths set (config generation {d}):", .{next.config_generation}) catch {};
        for (paths) |path| p.stdout.print(" {s}", .{path}) catch {};
        p.stdout.writeAll("\n") catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

fn setCachePaths(p: cli.Process, tgt: args.Target, raw_paths: []const u8, force: bool) u8 {
    var resolved = target.resolve(p.gpa, p.io, tgt) catch |e| {
        target.writeRefusal(p.stderr, "config set-cache-paths", tgt, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var body: SetBody = .{ .p = p, .paths = undefined, .n = 0, .force = force };
    var it = args.cachePaths(raw_paths);
    while (it.next()) |path| {
        body.paths[body.n] = path;
        body.n += 1;
    }
    return offline.withSet(p, resolved.meta_uri, .{ .with_plane = false, .verb = "config set-cache-paths" }, .{ .ctx = &body, .run = SetBody.run });
}

const testing = std.testing;

test "the directory precheck: absent, empty, a marker-bearing root (ours) pass; a non-empty stranger and a file do not" {
    try testing.expectEqual(DirVerdict.absent, classifyDir(testing.io, "/build/tmp/config-verb-absent-dir"));
    try testing.expectEqual(DirVerdict.not_a_directory, classifyDir(testing.io, "/proc/self/status"));
    try testing.expectEqual(DirVerdict.stranger, classifyDir(testing.io, "/proc/self"));
}
