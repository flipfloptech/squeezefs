//! The CLI binding of `nvmeof *` and `storage *` (charter §8): argv →
//! the verb modules with the real configfs face, the ledger store under
//! the state dir, the port-id base from its knob, and the initiator's
//! connect / disconnect reported in the shape the fidelity tier reads.
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const cli = @import("../../cli/cli.zig");
const args = @import("../../cli/args.zig");
const log = @import("../../core/log.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const configfs = @import("configfs.zig");
const ledger = @import("ledger.zig");
const verbs = @import("verbs.zig");
const initiator = @import("initiator.zig");
const storage = @import("storage.zig");
const pr_legs = @import("pr_legs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/run is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

fn trtypeOf(t: args.Transport) configfs.Trtype {
    return switch (t) {
        .tcp => .tcp,
        .loop => .loop,
    };
}

fn code(o: verbs.Outcome) u8 {
    return switch (o) {
        .ok => cli.exit_ok,
        .failed => cli.exit_failed,
    };
}

/// The target verbs: root (configfs is root's), the ledger pool ONE worker.
pub fn run(p: cli.Process, v: args.Nvmeof) u8 {
    switch (v) {
        .connect => |c| return connect(p, c),
        .disconnect => |d| return disconnect(p, d),
        .resv_report => |r| {
            if (r.pr_matrix) return pr_legs.matrixVerb(p.gpa, p.env, r.device, r.peer.?, r.force, r.json, p.stdout, p.stderr);
            return pr_legs.resvReport(p.gpa, r.device, r.json, p.stdout, p.stderr);
        },
        else => {},
    }
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound())) catch |e| {
        p.stderr.print("squeezefs nvmeof: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var store = ledger.Store.init(p.gpa, pool, ledger.stateDir(p.env)) catch {
        p.stderr.writeAll("squeezefs nvmeof: the state dir path is too long\n") catch {};
        return cli.exit_failed;
    };
    var ctx: verbs.Ctx = .{ .gpa = p.gpa, .io = p.io, .fs = configfs.RealFs.face(), .store = &store, .port_base = verbs.portBase(p.env), .stdout = p.stdout, .stderr = p.stderr };
    switch (v) {
        .share => |s| {
            return code(verbs.share(&ctx, .{
                .backing_path = s.backing,
                .subnqn = s.subnqn,
                .trtype = trtypeOf(s.transport),
                .ips = s.ips.slice(),
                .port = s.port,
                .allow_hosts = s.allow_hosts.slice(),
                .create_size = s.create_size,
                .ns_uuid = s.ns_uuid,
            }));
        },
        .unshare => |u| return code(verbs.unshare(&ctx, u.subnqn)),
        .list => |l| return code(verbs.list(&ctx, l.json)),
        .restore => return code(verbs.restore(&ctx)),
        .adopt => |a| return code(verbs.adopt(&ctx, a.subnqn)),
        .target => |t| return switch (t) {
            .status => |s| code(verbs.targetStatus(&ctx, s.json)),
            .setup => code(verbs.targetSetup(&ctx)),
            .systemd_unit => code(verbs.targetSystemdUnit(&ctx, p.argv[0])),
            .stop => code(verbs.targetStop(&ctx)),
        },
        .connect, .disconnect, .resv_report => unreachable,
    }
}

/// `nvmeof connect`: the identity (explicit pair > /etc/nvme > machine
/// id), the host-scoped announcement, the write + read-back + the
/// uevent-driven discovery; prints the block node, the char node, nsid,
/// uuid, instance, cntlid.
fn connect(p: cli.Process, c: args.NvmeofConnect) u8 {
    if (linux.geteuid() != 0) {
        p.stderr.writeAll("squeezefs nvmeof connect: root is required (/dev/nvme-fabrics)\n") catch {};
        return cli.exit_failed;
    }
    const resolved = initiator.resolveHostIdentity(p.env, .{ .hostnqn = c.hostnqn, .hostid = c.hostid });
    const id = switch (resolved) {
        .refusal => |r| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            r.write(&w) catch {};
            p.stderr.print("squeezefs nvmeof connect: {s}\n", .{w.buffered()}) catch {};
            return cli.exit_failed;
        },
        .identity => |i| i,
    };
    const trtype = trtypeOf(c.transport);
    if (trtype == .tcp and c.ip == null) {
        p.stderr.writeAll("squeezefs nvmeof connect: --transport tcp needs --ip <target address> (the share's listener; see `squeezefs nvmeof list`)\n") catch {};
        return cli.exit_failed;
    }
    const spec: initiator.ConnectSpec = .{
        .trtype = trtype,
        .traddr = c.ip orelse "",
        .trsvcid = c.port,
        .subnqn = c.subnqn,
        .hostnqn = id.hostnqn(),
        .hostid = id.hostid(),
        .host_traddr = c.host_traddr,
        .host_iface = c.host_iface,
        .nr_io_queues = c.nr_io_queues,
    };
    const hs = initiator.readHostScoped();
    log.info("nvmeof connect: identity {s} / {s} ({s}); {s}", .{ id.hostnqn(), id.hostid(), @tagName(id.source), hs.announce() });
    const got = initiator.connect(p.gpa, spec, c.ns_uuid) catch |e| {
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        initiator.writeConnectRefusal(&w, e, spec) catch {};
        p.stderr.print("squeezefs nvmeof connect: {s}\n", .{w.buffered()}) catch {};
        return cli.exit_failed;
    };
    var bp: [40]u8 = undefined;
    var cp: [40]u8 = undefined;
    var ub: [36]u8 = undefined;
    p.stdout.print("connected {s}\n  device {s}\n  char {s}\n  nsid {d}\n  uuid {s}\n  instance {d}\n  cntlid {d}\n  hostnqn {s}\n  hostid {s}\n  identity_source {s}\n", .{ c.subnqn, got.ns.blockPath(&bp), got.ns.charPath(&cp), got.ns.nsid, if (got.ns.uuid) |u| configfs.uuidText(&ub, u) else "(none)", got.instance, got.cntlid, id.hostnqn(), id.hostid(), @tagName(id.source) }) catch {};
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

fn disconnect(p: cli.Process, d: @FieldType(args.Nvmeof, "disconnect")) u8 {
    if (linux.geteuid() != 0) {
        p.stderr.writeAll("squeezefs nvmeof disconnect: root is required (delete_controller)\n") catch {};
        return cli.exit_failed;
    }
    var filter: initiator.DisconnectFilter = .{ .subsysnqn = d.subnqn };
    var id: ?initiator.Identity = null;
    if (d.identity_only) {
        id = switch (initiator.resolveHostIdentity(p.env, .{ .hostnqn = d.hostnqn, .hostid = d.hostid })) {
            .refusal => |r| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                r.write(&w) catch {};
                p.stderr.print("squeezefs nvmeof disconnect: {s}\n", .{w.buffered()}) catch {};
                return cli.exit_failed;
            },
            .identity => |i| i,
        };
        filter.identity = .{ .hostnqn = id.?.hostnqn(), .hostid = id.?.hostid() };
    }
    // Remember the instances so the verb can await their disappearance.
    const ctrls = initiator.listControllers(p.gpa, initiator.SYSFS_ROOT) catch {
        p.stderr.writeAll("squeezefs nvmeof disconnect: out of memory\n") catch {};
        return cli.exit_failed;
    };
    defer p.gpa.free(ctrls);
    const n = initiator.disconnect(p.gpa, filter) catch |e| {
        p.stderr.print("squeezefs nvmeof disconnect: delete_controller failed: {s}\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    if (n == 0) {
        p.stderr.print("squeezefs nvmeof disconnect: no connected controller found for {s}{s}\n", .{ d.subnqn, if (d.identity_only) " under this identity" else "" }) catch {};
        return cli.exit_failed;
    }
    for (ctrls) |c| {
        if (!std.mem.eql(u8, c.subsysnqnOf(), d.subnqn)) continue;
        if (filter.identity) |fi| if (!c.hasIdentity(fi.hostnqn, fi.hostid)) continue;
        _ = initiator.awaitControllerGone(c.instance);
    }
    p.stdout.print("disconnected {d} controller(s) of {s}\n", .{ n, d.subnqn }) catch {};
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

pub fn runStorage(p: cli.Process, v: args.Storage) u8 {
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound())) catch |e| {
        p.stderr.print("squeezefs storage: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var ctx: storage.Ctx = .{ .gpa = p.gpa, .pool = pool, .stdout = p.stdout, .stderr = p.stderr };
    return switch (v) {
        .list => |l| @backingInt(storage.list(&ctx, l.json)),
        .validate => |val| @backingInt(storage.validateVerb(&ctx, val.path)),
    };
}
