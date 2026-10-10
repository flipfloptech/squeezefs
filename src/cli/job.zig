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
//! throttle` and `list/status` on a mountpoint) and `worker` ride the
//! daemon's admin lane and the ONE listener's job block; this binary's
//! daemon serves the job block to remote workers but has no admin-lane
//! dispatcher yet, so those arms are REFUSED by name here — never a silent
//! no-op, never a fake success.

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
const jobs = @import("../ops/jobs.zig");
const volume_ops = @import("../ops/lifecycle.zig").volume_ops;

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/job is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, j: args.Job) u8 {
    return switch (j) {
        .list => |l| probe(p, l.target, .{ .list = l.json }),
        .status => |s| probe(p, s.target, .{ .status = s.job_id }),
        .pause, .@"resume", .cancel, .throttle => liveOnly(p, @tagName(j)),
        .worker => liveOnly(p, "worker"),
        .rotate_enroll => |r| blk: {
            var body: RotateBody = .{ .p = p };
            break :blk offline.withSet(p, r.meta_uri, .{ .with_plane = false, .verb = "job rotate-enroll" }, .{ .ctx = &body, .run = RotateBody.run });
        },
    };
}

fn liveOnly(p: cli.Process, verb: []const u8) u8 {
    p.stderr.print("squeezefs job {s}: refused: this arm controls a LIVE mount through the daemon's admin lane, which this build does not serve yet; the daemon runs its fabric (adoption, the local pool, the job block for remote workers) and `job list|status sqmeta://…` probes the durable records\n", .{verb}) catch {};
    p.stderr.flush() catch {};
    return cli.exit_failed;
}

const Probe = union(enum) { list: bool, status: u64 };

fn probe(p: cli.Process, target: []const u8, what: Probe) u8 {
    if (args.targetIsMountpoint(target)) return liveOnly(p, if (what == .list) "list" else "status");
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
                    for (list.items, 0..) |r, i| {
                        if (json) {
                            if (i != 0) c.p.stdout.writeAll(",") catch {};
                            c.p.stdout.print("{{\"id\":\"{f}\",\"type\":\"{s}\",\"state\":\"{s}\",\"throttle_pct\":{d},\"tasks_done\":{d},\"tasks_total\":{d},\"error_class\":\"{s}\"}}", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total, @tagName(r.error_class) }) catch {};
                        } else {
                            c.p.stdout.print("{f}  {s:<16} {s:<16} throttle {d:>3} %  {d}/{d}\n", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total }) catch {};
                        }
                    }
                    if (json) c.p.stdout.writeAll("]\n") catch {};
                    if (!json and list.items.len == 0) c.p.stdout.writeAll("no job records\n") catch {};
                },
                .status => |id| {
                    for (list.items) |r| if (r.job_id == id) {
                        c.p.stdout.print("{f}: {s} {s}, throttle {d} %, {d}/{d} tasks, error class {s}, created {d} by node {x} slot {d} pid {d}, updated {d}\n", .{ r, r.job_type.name(), @tagName(r.state), r.throttle_pct, r.tasks_done, r.tasks_total, @tagName(r.error_class), r.created_realtime_s, r.created_by.node_token, r.created_by.mount_slot, r.created_pid, r.updated_realtime_s }) catch {};
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
