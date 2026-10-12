//! `squeezefs job {list, status, pause, resume, cancel, throttle, worker,
//! rotate-enroll}` (charter §8; `ops_jobs_lifecycle.md` §1.10). Linux only.
//!
//! `list` / `status` on a `sqmeta://` set are PROBES of the durable
//! `Tag.job` range (a read-only open; never blocked by a live mount).
//! `rotate-enroll` is OFFLINE on a quiesced set: the `job:enroll` record is
//! REMOVED under the writer door; the next coordinator mints a fresh
//! secret and every member re-enrolls at its next arm (the secret is minted
//! once per set — a per-mount re-mint broke every member that outlived a
//! coordinator's incarnation). The live controls (`pause/resume/cancel/
//! throttle` and `list/status` on a MOUNTPOINT) ride the daemon's admin
//! lane (`admin_lane.zig`: the ONE listener behind the storage-trust
//! ladder); `worker` dials a set's coordinator over the same listener and
//! serves the job block's pull loop (`--enroll` is implied: a worker IS an
//! enrollment).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const offline = @import("offline.zig");
const derive = @import("../core/derive.zig");
const key = @import("../meta/key.zig");
const meta_set = @import("../meta/meta_set.zig");
const node_cache = @import("../meta/node_cache.zig");
const uring_fs = @import("../io/uring_fs.zig");
const mem_budget = @import("../mem/budget.zig");
const exec = @import("../exec/exec.zig");
const wire_auth = @import("../wire/auth.zig");
const wire_endpoint = @import("../wire/endpoint.zig");
const dialer = @import("../wire/dialer.zig");
const wire_tls = @import("../wire/tls.zig");
const verbs = @import("../wire/verbs.zig");
const errors = @import("../core/errors.zig");
const clock = @import("../core/clock.zig");
const cr = @import("../meta/sym/control_records.zig");
const tree = @import("../meta/tree.zig");
const jobs = @import("../ops/jobs.zig");
const volume_ops = @import("../ops/lifecycle.zig").volume_ops;
const admin_lane = @import("admin_lane.zig");
const daemon = @import("daemon.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/job is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, j: args.Job) u8 {
    return switch (j) {
        .list => |l| if (args.targetIsMountpoint(l.target)) live(p, l.target, .{ .list = l.json }) else probe(p, l.target, .{ .list = l.json }),
        .status => |s| if (args.targetIsMountpoint(s.target)) live(p, s.target, .{ .status = s.job_id }) else probe(p, s.target, .{ .status = s.job_id }),
        .pause => |c| control(p, c.target, c.job_id, .pause, 0),
        .@"resume" => |c| control(p, c.target, c.job_id, .@"resume", 0),
        .cancel => |c| control(p, c.target, c.job_id, .cancel, 0),
        .throttle => |c| control(p, c.target, c.job_id, .throttle, c.pct),
        .worker => |w| worker(p, w.meta_uri),
        .rotate_enroll => |r| blk: {
            var body: RotateBody = .{ .p = p };
            break :blk offline.withSet(p, r.meta_uri, .{ .with_plane = false, .verb = "job rotate-enroll" }, .{ .ctx = &body, .run = RotateBody.run });
        },
    };
}

const Probe = union(enum) { list: bool, status: u64 };

/// One row of the list / the status line (shared by the probe and the lane).
fn writeRow(p: cli.Process, r: jobs.JobRecord, json: bool, first: bool) void {
    if (json) {
        if (!first) p.stdout.writeAll(",") catch {};
        p.stdout.print("{{\"id\":\"{f}\",\"type\":\"{s}\",\"state\":\"{s}\",\"throttle_pct\":{d},\"tasks_done\":{d},\"tasks_total\":{d},\"error_class\":\"{s}\"}}", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total, @tagName(r.error_class) }) catch {};
    } else {
        p.stdout.print("{f}  {s:<16} {s:<16} throttle {d:>3} %  {d}/{d}\n", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total }) catch {};
    }
}

fn writeStatus(p: cli.Process, r: jobs.JobRecord) void {
    p.stdout.print("{f}: {s} {s}, throttle {d} %, {d}/{d} tasks, error class {s}, created {d} by node {x} slot {d} pid {d}, updated {d}\n", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total, @tagName(r.error_class), r.created_realtime_s, r.created_by.node_token, r.created_by.mount_slot, r.created_pid, r.updated_realtime_s }) catch {};
}

/// `list` / `status` against the LIVE table over the admin lane.
fn live(p: cli.Process, mountpoint: []const u8, what: Probe) u8 {
    const Body = struct {
        p: cli.Process,
        what: Probe,
        fn run(ctx: *anyopaque, lane: *admin_lane.Lane) u8 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const gpa = self.p.gpa;
            var list = std.ArrayList(jobs.JobRecord).empty;
            defer list.deinit(gpa);
            lane.admin().list(gpa, &list) catch |e| {
                self.p.stderr.print("squeezefs job: the live table could not be read ({s}, {t})\n", .{ @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            };
            switch (self.what) {
                .list => |json| {
                    if (json) self.p.stdout.writeAll("[") catch {};
                    for (list.items, 0..) |r, i| writeRow(self.p, r, json, i == 0);
                    if (json) self.p.stdout.writeAll("]\n") catch {};
                    if (!json and list.items.len == 0) self.p.stdout.writeAll("no jobs in the live table\n") catch {};
                },
                .status => |id| {
                    for (list.items) |r| if (r.job_id == id) {
                        writeStatus(self.p, r);
                        self.p.stdout.flush() catch {};
                        return cli.exit_ok;
                    };
                    self.p.stderr.print("squeezefs job status: job-{x:0>16}: not in the live table\n", .{id}) catch {};
                    return cli.exit_failed;
                },
            }
            self.p.stdout.flush() catch {};
            return cli.exit_ok;
        }
    };
    var body: Body = .{ .p = p, .what = what };
    return admin_lane.withLane(p, if (what == .list) "job list" else "job status", mountpoint, .{ .ctx = &body, .run = Body.run });
}

/// `pause / resume / cancel / throttle` over the admin lane: the verdict
/// decides the exit, never a string.
fn control(p: cli.Process, target: []const u8, job_id: u64, op: admin_lane.frames.ControlOp, pct: u8) u8 {
    const verb = @tagName(op);
    if (!args.targetIsMountpoint(target)) {
        p.stderr.print("squeezefs job {s}: refused: a live control takes the MOUNTPOINT (the daemon's admin lane); `sqmeta://` targets are the durable-record probes (`job list|status`)\n", .{verb}) catch {};
        p.stderr.flush() catch {};
        return cli.exit_usage;
    }
    const Body = struct {
        p: cli.Process,
        job_id: u64,
        op: admin_lane.frames.ControlOp,
        pct: u8,
        fn run(ctx: *anyopaque, lane: *admin_lane.Lane) u8 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const v = lane.admin().control(self.job_id, self.op, self.pct) catch |e| {
                self.p.stderr.print("squeezefs job {s}: the control call failed ({s}, {t})\n", .{ @tagName(self.op), @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            };
            if (v != .ok) {
                self.p.stderr.print("squeezefs job {s}: job-{x:0>16}: refused: {s}\n", .{ @tagName(self.op), self.job_id, admin_lane.verdictText(v) }) catch {};
                self.p.stderr.flush() catch {};
                return cli.exit_failed;
            }
            switch (self.op) {
                .throttle => self.p.stdout.print("job-{x:0>16}: throttle {d} % (live; the next slice re-prices)\n", .{ self.job_id, self.pct }) catch {},
                else => self.p.stdout.print("job-{x:0>16}: {s} requested (durable intent; the worker settles it at its next task boundary)\n", .{ self.job_id, @tagName(self.op) }) catch {},
            }
            self.p.stdout.flush() catch {};
            return cli.exit_ok;
        }
    };
    var body: Body = .{ .p = p, .job_id = job_id, .op = op, .pct = pct };
    var vb: [24]u8 = undefined;
    return admin_lane.withLane(p, std.fmt.bufPrint(&vb, "job {s}", .{verb}) catch "job", target, .{ .ctx = &body, .run = Body.run });
}

fn probe(p: cli.Process, target: []const u8, what: Probe) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, target, &members, &udiag) catch {
        p.stderr.print("squeezefs job: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs job: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const sqz = exec.SqzIo.init(p.gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs job: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    const Ctx = struct {
        p: cli.Process,
        paths: []const []const u8,
        what: Probe,
        pool: *uring_fs.Pool,
        cache: u64,
        cpus: u32,
        fn main(c: *@This(), io: Io) u8 {
            const set = meta_set.MetaSet.open(c.p.gpa, io, c.paths, .{ .env = c.p.env, .pool = c.pool, .cache_budget_bytes = c.cache, .cpus = c.cpus, .read_only = true }) catch |e| {
                c.p.stderr.print("squeezefs job: cannot open the set: {s}\n", .{@errorName(e)}) catch {};
                return cli.exit_failed;
            };
            defer set.close();
            var list = volume_ops.listJobs(c.p.gpa, set) catch |e| {
                c.p.stderr.print("squeezefs job: the job range could not be read ({s})\n", .{@errorName(e)}) catch {};
                return cli.exit_failed;
            };
            defer list.deinit(c.p.gpa);
            switch (c.what) {
                .list => |json| {
                    if (json) c.p.stdout.writeAll("[") catch {};
                    for (list.items, 0..) |r, i| writeRow(c.p, r, json, i == 0);
                    if (json) c.p.stdout.writeAll("]\n") catch {};
                    if (!json and list.items.len == 0) c.p.stdout.writeAll("no job records\n") catch {};
                },
                .status => |id| {
                    for (list.items) |r| if (r.job_id == id) {
                        writeStatus(c.p, r);
                        c.p.stdout.flush() catch {};
                        return cli.exit_ok;
                    };
                    c.p.stderr.print("squeezefs job status: job-{x:0>16}: no such durable record\n", .{id}) catch {};
                    return cli.exit_failed;
                },
            }
            c.p.stdout.flush() catch {};
            return cli.exit_ok;
        }
    };
    var ctx: Ctx = .{ .p = p, .paths = meta.paths, .what = what, .pool = pool, .cache = node_cache.nodeCacheBudgetFromEnv(p.env, budget_bytes), .cpus = facts.possible_cpus };
    return sqz.run(Ctx.main, .{ &ctx, sqz.io() }) catch |e| {
        p.stderr.print("squeezefs job: the root task could not start ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
}

/// The worker lane's mount-slot space: `0xFFFE_0000 | pid16` — its own
/// space beside the admin lane's (`0xFFFF_xxxx`) and a mount's lane
/// index, so a worker on the coordinator's host never collides with a
/// member or an admin connection.
pub const WORKER_MOUNT_SLOT_BASE: u32 = 0xFFFE_0000;

/// `job worker sqmeta://…`: the remote worker over TCP (charter §8
/// "remote workers over TCP/TLS with HMAC enrollment"): the coordinator's
/// endpoint off tree 0 of volume 0 (the `endpoint:` record of appender 0
/// — the manager's own entry, the word a joiner reads before it holds any
/// wire), the storage-trust proof off the same tree, ONE dialed session,
/// `job_enroll`, then the pull loop until SIGTERM/SIGINT. A dead
/// coordinator ends the loop typed (the dial's EAGAIN class), never a
/// spin.
fn worker(p: cli.Process, meta_uri: []const u8) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs job worker: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs job worker: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const sqz = exec.SqzIo.init(p.gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs job worker: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    daemon.Signals.install() catch {
        p.stderr.writeAll("squeezefs job worker: the signal eventfd could not be created\n") catch {};
        return cli.exit_failed;
    };
    const Root = struct {
        p: cli.Process,
        paths: []const []const u8,
        pool: *uring_fs.Pool,
        cache: u64,
        cpus: u32,
        sqz: *exec.SqzIo,

        const Caller = struct {
            client: *dialer.Client,
            buf: []u8,
            fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
                const self: *@This() = @ptrCast(@alignCast(ctx));
                return self.client.callTyped(req, self.buf);
            }
        };

        /// The stop latch: the signal word rings the eventfd; the watcher
        /// turns it into the worker's `requestStop` (the loop parks on the
        /// worker's own notify, bounded by the heartbeat quarter).
        fn watch(w: *jobs.wire.Worker, sqz_: *exec.SqzIo) void {
            const Signals = daemon.Signals;
            while (!w.stop.load(.acquire)) {
                _ = sqz_.readable(Signals.eventfd()) catch break;
                Signals.drain();
                const bits = Signals.take();
                if (bits & daemon.SIG_STOP != 0) break;
            }
            w.requestStop();
        }

        fn main(c: *@This(), io: Io) u8 {
            const gpa = c.p.gpa;
            var posture = wire_tls.linux_files.installFromEnv(gpa, c.p.env) catch |e| {
                var tb: [1024]u8 = undefined;
                var tw: Io.Writer = .fixed(&tb);
                wire_tls.writeInstallRefusal(&tw, e, wire_tls.Paths.fromEnv(c.p.env)) catch {};
                c.p.stderr.print("squeezefs job worker: cluster wire: {s}\n", .{tw.buffered()}) catch {};
                return cli.exit_failed;
            };
            defer posture.deinit(gpa);
            var secret: wire_auth.Secret = undefined;
            var ep: wire_endpoint.EndpointWire = .none;
            {
                const set = meta_set.MetaSet.open(gpa, io, c.paths, .{ .env = c.p.env, .pool = c.pool, .cache_budget_bytes = c.cache, .cpus = c.cpus, .read_only = true }) catch |e| {
                    c.p.stderr.print("squeezefs job worker: cannot open the set read-only: {s}\n", .{@errorName(e)}) catch {};
                    return cli.exit_failed;
                };
                defer set.close();
                const value = (set.lookup(gpa, key.TREE_CONTROL, wire_auth.ENROLL_KEY) catch null) orelse {
                    c.p.stderr.writeAll("squeezefs job worker: tree 0 of volume 0 carries no job:enroll record (reformat)\n") catch {};
                    return cli.exit_failed;
                };
                defer gpa.free(value);
                secret = wire_auth.decodeEnroll(value) catch {
                    c.p.stderr.writeAll("squeezefs job worker: the job:enroll record does not decode — refused, never re-minted\n") catch {};
                    return cli.exit_failed;
                };
                const ek = cr.endpointKey(0);
                const ev = (set.lookup(gpa, key.TREE_CONTROL, ek.slice()) catch null) orelse {
                    c.p.stderr.writeAll("squeezefs job worker: the set's manager publishes no endpoint record (no live coordinator; mount the set first)\n") catch {};
                    return cli.exit_failed;
                };
                defer gpa.free(ev);
                const e = cr.EndpointRecordWire.decode(ev) catch {
                    c.p.stderr.writeAll("squeezefs job worker: the endpoint record does not decode\n") catch {};
                    return cli.exit_failed;
                };
                ep = .{ .family = e.family, .reserved = 0, .port = e.port, .addr = e.addr };
            }
            const local = @import("../meta/guard/proc.zig").localIdentity();
            const pid: u32 = @intCast(std.os.linux.getpid());
            const identity: verbs.WireIdentity = .{ .node_token = @import("../meta/sym/join.zig").nodeToken(local), .writer_id = @as(u64, pid) << 32 | pid, .mount_slot = WORKER_MOUNT_SLOT_BASE | (pid & 0xFFFF), .reserved = 0 };
            var client = dialer.Client.init(gpa, io, ep, secret, identity);
            defer client.deinit();
            const buf = gpa.alloc(u8, admin_lane.REPLY_BUF_LEN) catch return cli.exit_failed;
            defer gpa.free(buf);
            var caller: Caller = .{ .client = &client, .buf = buf };
            var w: jobs.wire.Worker = .{ .io = io, .caller = .{ .ctx = &caller, .call = &Caller.call }, .gpa = gpa, .hold_submission = @import("../core/knobs.zig").boolKnob("SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION", c.p.env) };
            const en = w.enroll() catch |e| {
                c.p.stderr.print("squeezefs job worker: enrollment refused at {f} ({s})\n", .{ ep, @errorName(e) }) catch {};
                return cli.exit_failed;
            };
            c.p.stdout.print("job worker: enrolled at {f} as {f} (heartbeat {d} ms, lease TTL {d} ms); pulling shards until SIGTERM\n", .{ ep, identity, en.heartbeat_ms, en.lease_ttl_ms }) catch {};
            c.p.stdout.flush() catch {};
            var watcher = io.concurrent(watch, .{ &w, c.sqz }) catch return cli.exit_failed;
            const served = w.serve();
            w.requestStop();
            daemon.Signals.ring();
            watcher.cancel(io);
            served catch |e| {
                const errno_text: []const u8 = switch (e) {
                    error.EnrollRefused, error.Incapable => @errorName(e),
                    else => |core| @tagName(errors.toErrno(core)),
                };
                c.p.stderr.print("squeezefs job worker: the coordinator went away ({s}, {s}) after {d} shard(s) done, {d} refused, {d} heartbeat(s)\n", .{ @errorName(e), errno_text, w.shards_done, w.shards_refused, w.heartbeats }) catch {};
                return cli.exit_failed;
            };
            c.p.stdout.print("job worker: stopped; {d} shard(s) done, {d} refused, {d} heartbeat(s)\n", .{ w.shards_done, w.shards_refused, w.heartbeats }) catch {};
            c.p.stdout.flush() catch {};
            return cli.exit_ok;
        }
    };
    var root: Root = .{ .p = p, .paths = meta.paths, .pool = pool, .cache = node_cache.nodeCacheBudgetFromEnv(p.env, budget_bytes), .cpus = facts.possible_cpus, .sqz = sqz };
    return sqz.run(Root.main, .{ &root, sqz.io() }) catch |e| {
        p.stderr.print("squeezefs job worker: the root task could not start ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
}

/// `rotate-enroll`: refuse while a job record is non-terminal (a worker
/// mid-shard would lose its channel), then delete the record in ONE tx.
const RotateBody = struct {
    p: cli.Process,

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *RotateBody = @ptrCast(@alignCast(ctx));
        const p = self.p;
        var list = volume_ops.listJobs(p.gpa, set.be) catch {
            p.stderr.writeAll("squeezefs job rotate-enroll: the job range could not be read\n") catch {};
            return cli.exit_failed;
        };
        defer list.deinit(p.gpa);
        for (list.items) |r| if (!r.isTerminal()) {
            p.stderr.print("squeezefs job rotate-enroll: refused: {f} is {s} — rotate only on a quiesced set (cancel or let it finish)\n", .{ r, @tagName(r.state) }) catch {};
            return cli.exit_failed;
        };
        const existed = (set.be.lookup(p.gpa, key.TREE_CONTROL, wire_auth.ENROLL_KEY) catch null) != null;
        if (existed) {
            const t = set.be.begin(@src()) catch {
                p.stderr.writeAll("squeezefs job rotate-enroll: out of memory\n") catch {};
                return cli.exit_failed;
            };
            t.stageDelete(key.TREE_CONTROL, wire_auth.ENROLL_KEY) catch {
                set.be.abandon(t);
                p.stderr.writeAll("squeezefs job rotate-enroll: out of memory\n") catch {};
                return cli.exit_failed;
            };
            set.be.commit(t) catch |e| {
                p.stderr.print("squeezefs job rotate-enroll: the commit failed: {t}\n", .{e}) catch {};
                return cli.exit_failed;
            };
            set.be.fsync() catch |e| {
                p.stderr.print("squeezefs job rotate-enroll: the durability barrier failed: {t}\n", .{e}) catch {};
                return cli.exit_failed;
            };
        }
        p.stdout.print("job:enroll {s}; the next coordinator mints a fresh secret and every member re-enrolls at its next arm\n", .{if (existed) "record removed" else "record was absent (nothing to rotate)"}) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};
