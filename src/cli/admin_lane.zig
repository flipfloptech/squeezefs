//! The admin lane's CLIENT (charter §8: `job {list, status, pause, resume,
//! cancel, throttle}`, `fsck --online`, `scrub --online`, `defrag --*`,
//! `volume {add-data, remove-data, undrain}` against a LIVE mount):
//! resolve the mount through its own `.stats` inode (`sqmeta_uri`, the
//! daemon's `wire_endpoint`), walk the storage-trust ladder — read the
//! set's `job:enroll` secret through a read-only open of the set (whoever
//! can read tree 0 of volume 0 is inside the trust domain; the same
//! credential every member proves) — dial the ONE listener, and run the
//! verb's typed calls (`ops/jobs/admin.zig`'s `Client`). Linux only.
//!
//! A mount with no wire (`--single-writer`, a `-o ro` reader) publishes no
//! endpoint: the lane refuses BY NAME and points at the offline verb.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const cli = @import("cli.zig");
const uri = @import("uri.zig");
const json_scan = @import("../core/json_scan.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const key = @import("../meta/key.zig");
const meta_set = @import("../meta/meta_set.zig");
const node_cache = @import("../meta/node_cache.zig");
const proc = @import("../meta/guard/proc.zig");
const sym_join = @import("../meta/sym/join.zig");
const uring_fs = @import("../io/uring_fs.zig");
const mem_budget = @import("../mem/budget.zig");
const exec = @import("../exec/exec.zig");
const wire_auth = @import("../wire/auth.zig");
const wire_endpoint = @import("../wire/endpoint.zig");
const dialer = @import("../wire/dialer.zig");
const verbs = @import("../wire/verbs.zig");
const jobs = @import("../ops/jobs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/admin_lane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const admin_mod = jobs.admin;
pub const frames = jobs.admin_frames;

/// The admin client's mount-slot space: `0xFFFF_0000 | pid16`. A mount's
/// slot is its lane index (small); the wire keys peers by `(node_token,
/// mount_slot)`, so an admin connection never collides with a member of
/// this host and two concurrent CLIs on one host differ by pid.
pub const ADMIN_MOUNT_SLOT_BASE: u32 = 0xFFFF_0000;

pub fn adminIdentity() verbs.WireIdentity {
    const local = proc.localIdentity();
    const pid: u32 = @intCast(linux.getpid());
    return .{ .node_token = sym_join.nodeToken(local), .writer_id = @as(u64, pid) << 32 | pid, .mount_slot = ADMIN_MOUNT_SLOT_BASE | (pid & 0xFFFF), .reserved = 0 };
}

/// Why the lane could not open (each reported by name with its remedy).
pub const Refusal = enum {
    stats_unreadable,
    no_endpoint,
    no_sqmeta,
    bad_endpoint,
    set_unopenable,
    enroll_missing,
    dial_failed,

    pub fn text(self: Refusal) []const u8 {
        return switch (self) {
            .stats_unreadable => "its .stats inode is unreadable (no squeezefs daemon serves it)",
            .no_endpoint => "the daemon publishes no wire endpoint (a `--single-writer` set or a `-o ro` reader runs no admin lane; use the offline verb on the unmounted set)",
            .no_sqmeta => "its .stats names no sqmeta:// set",
            .bad_endpoint => "the daemon's wire endpoint does not parse",
            .set_unopenable => "the set could not be opened read-only for the storage-trust proof",
            .enroll_missing => "tree 0 of volume 0 carries no decodable job:enroll record (formatted before the record existed; reformat)",
            .dial_failed => "the daemon's listener refused the dial or the proof (see the errno)",
        };
    }
};

/// What `.stats` told us about the mount.
pub const Resolved = struct {
    sqmeta_uri: []u8,
    endpoint: wire_endpoint.EndpointWire,

    pub fn deinit(self: *Resolved, gpa: Allocator) void {
        gpa.free(self.sqmeta_uri);
    }
};

pub fn resolve(gpa: Allocator, io: Io, mountpoint: []const u8) error{ OutOfMemory, Refused }!union(enum) { ok: Resolved, refused: Refusal } {
    const path = try std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint});
    defer gpa.free(path);
    const text = Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch return .{ .refused = .stats_unreadable };
    defer gpa.free(text);
    const ep_text = json_scan.string(text, "wire_endpoint") orelse return .{ .refused = .no_endpoint };
    if (ep_text.len == 0) return .{ .refused = .no_endpoint };
    const sq = json_scan.string(text, "sqmeta_uri") orelse return .{ .refused = .no_sqmeta };
    if (sq.len == 0) return .{ .refused = .no_sqmeta };
    const ep = wire_endpoint.parseEndpoint(ep_text) catch return .{ .refused = .bad_endpoint };
    return .{ .ok = .{ .sqmeta_uri = try gpa.dupe(u8, sq), .endpoint = ep } };
}

/// The widest admin reply (the fsck report at its cap, the full job list)
/// with its heads: the lane's one reply buffer.
pub const REPLY_BUF_LEN: usize = 128 << 10;

comptime {
    std.debug.assert(REPLY_BUF_LEN >= 8 + frames.REPORT_BYTES_MAX + 8);
    std.debug.assert(REPLY_BUF_LEN >= 8 + frames.LIST_ROWS_MAX * 96);
}

/// The open lane: the dialed client over the executor the set open needed.
pub const Lane = struct {
    gpa: Allocator,
    io: Io,
    client: dialer.Client,
    buf: []u8,

    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *Lane = @ptrCast(@alignCast(ctx));
        return self.client.callTyped(req, self.buf);
    }

    pub fn admin(self: *Lane) admin_mod.Client {
        return .{ .ctx = self, .call = &call };
    }
};

pub const Body = struct {
    ctx: *anyopaque,
    run: *const fn (ctx: *anyopaque, lane: *Lane) u8,
};

/// Resolve, prove, dial, run `body`; every refusal printed by name with
/// the verb's prefix. The read-only set open rides our executor (its pass
/// fibers), so the whole verb runs as the root task.
pub fn withLane(p: cli.Process, verb: []const u8, mountpoint: []const u8, body: Body) u8 {
    const gpa = p.gpa;
    var resolved = switch (resolve(gpa, p.io, mountpoint) catch return oom(p, verb)) {
        .ok => |r| r,
        .refused => |why| return refused(p, verb, mountpoint, why, null),
    };
    defer resolved.deinit(gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.sqmeta_uri, &members, &udiag) catch return refused(p, verb, mountpoint, .no_sqmeta, null);
    const pool = uring_fs.Pool.init(gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs {s}: io pool init failed ({s})\n", .{ verb, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const sqz = exec.SqzIo.init(gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs {s}: executor init failed ({s})\n", .{ verb, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    const Root = struct {
        p: cli.Process,
        verb: []const u8,
        mountpoint: []const u8,
        paths: []const []const u8,
        endpoint: wire_endpoint.EndpointWire,
        pool: *uring_fs.Pool,
        cache: u64,
        cpus: u32,
        body: Body,

        fn main(c: *@This(), io: Io) u8 {
            const gpa_ = c.p.gpa;
            // The storage-trust proof: the secret off tree 0 of volume 0.
            const secret: wire_auth.Secret = blk: {
                const set = meta_set.MetaSet.open(gpa_, io, c.paths, .{ .env = c.p.env, .pool = c.pool, .cache_budget_bytes = c.cache, .cpus = c.cpus, .read_only = true }) catch |e| {
                    c.p.stderr.print("squeezefs {s}: {s}: cannot open the set read-only ({s})\n", .{ c.verb, c.mountpoint, @errorName(e) }) catch {};
                    return refused(c.p, c.verb, c.mountpoint, .set_unopenable, null);
                };
                defer set.close();
                const value = (set.lookup(gpa_, key.TREE_CONTROL, wire_auth.ENROLL_KEY) catch null) orelse return refused(c.p, c.verb, c.mountpoint, .enroll_missing, null);
                defer gpa_.free(value);
                break :blk wire_auth.decodeEnroll(value) catch return refused(c.p, c.verb, c.mountpoint, .enroll_missing, null);
            };
            const buf = gpa_.alloc(u8, REPLY_BUF_LEN) catch return oom(c.p, c.verb);
            defer gpa_.free(buf);
            var lane: Lane = .{ .gpa = gpa_, .io = io, .client = dialer.Client.init(gpa_, io, c.endpoint, secret, adminIdentity()), .buf = buf };
            defer lane.client.deinit();
            // The dial + ladder happen on the first call; a refused proof or
            // a dead listener surfaces there as the typed error.
            _ = lane.client.callTyped(.{ .ping = .{} }, &.{}) catch |e| return refused(c.p, c.verb, c.mountpoint, .dial_failed, e);
            return c.body.run(c.body.ctx, &lane);
        }
    };
    var root: Root = .{ .p = p, .verb = verb, .mountpoint = mountpoint, .paths = meta.paths, .endpoint = resolved.endpoint, .pool = pool, .cache = node_cache.nodeCacheBudgetFromEnv(p.env, budget_bytes), .cpus = facts.possible_cpus, .body = body };
    return sqz.run(Root.main, .{ &root, sqz.io() }) catch |e| {
        p.stderr.print("squeezefs {s}: the root task could not start ({s})\n", .{ verb, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
}

fn refused(p: cli.Process, verb: []const u8, mountpoint: []const u8, why: Refusal, e: ?errors.Error) u8 {
    if (e) |err| {
        p.stderr.print("squeezefs {s}: {s}: refused: {s} ({s}, {t})\n", .{ verb, mountpoint, why.text(), @errorName(err), errors.toErrno(err) }) catch {};
    } else {
        p.stderr.print("squeezefs {s}: {s}: refused: {s}\n", .{ verb, mountpoint, why.text() }) catch {};
    }
    p.stderr.flush() catch {};
    return cli.exit_failed;
}

fn oom(p: cli.Process, verb: []const u8) u8 {
    p.stderr.print("squeezefs {s}: out of memory\n", .{verb}) catch {};
    return cli.exit_failed;
}

/// The verdict's exit + text (one table for every verb).
pub fn verdictText(v: frames.Verdict) []const u8 {
    return switch (v) {
        .ok => "ok",
        .unknown_job => "no such job in the live table",
        .terminal => "the job is terminal (completed, cancelled or failed) — nothing to control",
        .not_coordinator => "this daemon is not the coordinator (volume 0's manager) — run the verb against the manager's mount",
        .table_full => "the live job table is full (256) — wait for jobs to finish",
        .refused => "the lattice refused (the daemon's log names the transition: only Active drains, only Draining undrains, a Retired id never comes back, a device already in the set is refused)",
        .io => "a device or commit error on the daemon (see its log)",
        .no_report => "the job carries no report (not an fsck job, or no pass ran yet)",
    };
}

/// Wait for a job's terminal state through the lane (bounded; the poll
/// rides the fabric's checkpoint cadence — 256 tasks or 5 s — so a quarter
/// of it never misses a flip by more than a beat).
pub const POLL_NS: u64 = jobs.fabric.CHECKPOINT_NS / 4;

pub fn waitTerminal(lane: *Lane, job_id: u64, deadline_ns: u64) errors.Error!?jobs.JobRecord {
    const clock = @import("../core/clock.zig");
    var list: std.ArrayList(jobs.JobRecord) = .empty;
    defer list.deinit(lane.gpa);
    while (true) {
        list.clearRetainingCapacity();
        try lane.admin().list(lane.gpa, &list);
        var found: ?jobs.JobRecord = null;
        for (list.items) |r| if (r.job_id == job_id) {
            found = r;
        };
        if (found) |r| if (r.isTerminal()) return r;
        if (found == null) return null;
        if (clock.monotonicNs() >= deadline_ns) return found;
        lane.io.sleep(.fromNanoseconds(POLL_NS), .awake) catch {};
    }
}

test "the admin identity lives in its own slot space and never collides with a mount's lane index" {
    const id = adminIdentity();
    try std.testing.expect(id.mount_slot >= ADMIN_MOUNT_SLOT_BASE);
    try std.testing.expect(id.mount_slot - ADMIN_MOUNT_SLOT_BASE < 1 << 16);
    try std.testing.expect(id.node_token != 0);
}
