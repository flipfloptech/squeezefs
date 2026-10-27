//! The OFFLINE set: what `clone`, `staging promote` and `config
//! set-cache-paths` share (`docs/porting/stats_cli.md` §4.1, §4.3, §4.4) —
//! the metadata volume opened through the writer door (the D0 ladder's
//! layer A: a live mount's flock refuses the verb by name — "the
//! single-writer guard forbids offline mutations under a live write
//! mount"), the set config loaded, optionally the data plane and the
//! filesystem bound exactly as `mount` binds them (no FUSE session), a
//! body run on the executor's main lane, then the teardown in `mount`'s
//! order (filesystem → plane's dismount pass → the backend's final
//! checkpoint). Every path through here releases the guard. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const cli = @import("cli.zig");
const uri = @import("uri.zig");
const exec = @import("../exec/exec.zig");
const meta_set = @import("../meta/meta_set.zig");
const node_cache = @import("../meta/node_cache.zig");
const dlm = @import("../meta/dlm.zig");
const sym_xv = @import("../meta/sym/xv.zig");
const key = @import("../meta/key.zig");
const handler = @import("../fuse/handler.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const mem_budget = @import("../mem/budget.zig");
const set_config = @import("../data/set_config.zig");
const data_superblock = @import("../data/data_superblock.zig");
const transform = @import("../data/transform.zig");
const plane_mod = @import("../data/plane.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/offline is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const SqzIo = exec.SqzIo;
const BackendFs = handler.BackendFs;
const watchdog = handler.watchdog;
const op_guard = handler.op_guard;
const open_table = handler.open_table;
const op_trace = handler.op_trace;
const filesystem = handler.filesystem;

/// The opened set a body works on.
pub const Set = struct {
    gpa: Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    /// The member paths as spelled.
    paths: []const []const u8,
    be: *meta_set.MetaSet,
    cfg: *set_config.Config,
    /// Present when the verb asked for the data plane.
    plane: ?*plane_mod.DataPlane,
    fs: ?*BackendFs,
    uid: u32,
    gid: u32,

    /// A handler context for the filesystem's ops (the invoker's identity).
    pub fn ctx(self: *Set, trace: *op_trace.OpTrace) filesystem.Ctx {
        return .{ .unique = 0, .uid = self.uid, .gid = self.gid, .pid = 0, .io = self.io, .trace = trace, .wd_slot = null, .window = &.{} };
    }
};

pub const Options = struct {
    /// Open the data plane and bind the filesystem (clone, promote); a
    /// control-record verb needs the backend alone.
    with_plane: bool,
    verb: []const u8,
};

/// The body: runs on the main lane with the set open; returns the exit code.
pub const Body = struct {
    ctx: *anyopaque,
    run: *const fn (ctx: *anyopaque, set: *Set) u8,
};

const Ctx = struct {
    p: cli.Process,
    meta_uri: []const u8,
    paths: []const []const u8,
    opts: Options,
    body: Body,
    sqz: *SqzIo,
    facts: derive.Facts,
    budget_bytes: u64,

    fn failed(self: *Ctx, comptime fmt: []const u8, a: anytype) u8 {
        self.p.stderr.print("squeezefs {s}: ", .{self.opts.verb}) catch {};
        self.p.stderr.print(fmt, a) catch {};
        self.p.stderr.writeAll("\n") catch {};
        self.p.stderr.flush() catch {};
        return cli.exit_failed;
    }
};

/// Open the set named by `meta_uri` (one member at this rung) and run the
/// body. The refusals: a held guard ("live-mounted"), an unformatted or
/// refused volume, a missing set config, a key that does not verify.
pub fn withSet(p: cli.Process, meta_uri: []const u8, opts: Options, body: Body) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs {s}: {f}\n", .{ opts.verb, udiag }) catch {};
        return cli.exit_usage;
    };
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const sqz = SqzIo.init(p.gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs {s}: executor init failed ({s})\n", .{ opts.verb, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    var ctx: Ctx = .{ .p = p, .meta_uri = meta_uri, .paths = meta.paths, .opts = opts, .body = body, .sqz = sqz, .facts = facts, .budget_bytes = budget_bytes };
    return sqz.run(main, .{&ctx}) catch |e| ctx.failed("the root task could not start ({s})", .{@errorName(e)});
}

fn main(ctx: *Ctx) u8 {
    const p = ctx.p;
    const gpa = p.gpa;
    const io = ctx.sqz.io();
    const be = meta_set.MetaSet.open(gpa, io, ctx.paths, .{ .env = p.env, .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(p.env, ctx.budget_bytes), .cpus = ctx.facts.possible_cpus }) catch |e| {
        if (e == error.Busy) return ctx.failed("refused: metadata set '{s}' is live-mounted (the writer guard is held) — the single-writer guard forbids offline mutations under a live write mount; run the verb on the mountpoint or unmount first", .{ctx.meta_uri});
        return ctx.failed("cannot open metadata set {s}: {s} ({t}, {s})", .{ ctx.meta_uri, @errorName(e), meta_set.openErrno(e), @tagName(errors.classOf(meta_set.openCoreError(e))) });
    };
    defer be.close();
    const value = (be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch |e| return ctx.failed("cannot read the set_config record of {s}: {s}", .{ ctx.meta_uri, @errorName(e) })) orelse return ctx.failed("{s}: member 0 carries no set_config record in its control tree; reformat", .{ctx.meta_uri});
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        diag.write(&w) catch {};
        return ctx.failed("{s}: {s}", .{ ctx.meta_uri, w.buffered() });
    };
    defer cfg.deinit(gpa);
    be.setDirSpreadDepth(cfg.dir_spread_depth);
    var set: Set = .{ .gpa = gpa, .io = io, .env = p.env, .paths = ctx.paths, .be = be, .cfg = &cfg, .plane = null, .fs = null, .uid = std.os.linux.geteuid(), .gid = std.os.linux.getegid() };
    if (!ctx.opts.with_plane) return ctx.body.run(ctx.body.ctx, &set);

    // The data plane as `mount` opens it: discovery, the key, the plane,
    // the filesystem.
    {
        const obs = gpa.alloc(set_config.Observation, cfg.data.len) catch return ctx.failed("out of memory", .{});
        defer gpa.free(obs);
        for (cfg.data, obs) |d, *o| {
            if (d.state == .retired) {
                o.* = .blank;
                continue;
            }
            const opened = data_superblock.linux_dev.open(gpa, d.uri);
            o.* = data_superblock.observation(if (opened) |op| op.classified else |e| e);
        }
        var ddiag: set_config.DiscoveryRefusal = undefined;
        set_config.discover(&cfg, be.setUuid(), obs, &ddiag) catch {
            var buf: [1024]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            ddiag.write(&w) catch {};
            return ctx.failed("data volume discovery: {s}", .{w.buffered()});
        };
    }
    var derived_key: ?transform.DerivedKey = null;
    if (cfg.encryption != .none) {
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        derived_key = transform.linux_keys.resolveVolumeKey(p.env, null, &cfg.kdf_salt, cfg.key_id, &path_buf, &served) catch |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, if (served.len != 0) served else transform.defaultKeyPath(&path_buf, cfg.key_id)) catch {};
            return ctx.failed("encrypted set: {s}", .{w.buffered()});
        };
    }
    var guards = op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (op guards)", .{});
    defer guards.deinit();
    const plane = plane_mod.DataPlane.open(gpa, io, ctx.sqz, be, &cfg, .{ .env = p.env, .facts = ctx.facts, .budget_bytes = ctx.budget_bytes, .derived_key = derived_key, .guards = &guards }) catch |e| return ctx.failed("the data plane could not open ({s}) — see the log", .{@errorName(e)});
    if (derived_key) |*k| k.zeroize();
    set.plane = plane;
    // A body may run the dismount pass itself (`staging promote`) and
    // null the plane; the teardown closes it otherwise.
    defer if (set.plane) |pl| pl.close();
    var fs = BackendFs.mount(gpa, io, be, plane, .{ .page_size = ctx.facts.page_size, .dlm_stripes = dlm.resolveWidth(p.env, ctx.facts, fuse_geometry.Q_DEPTH_DESIRED), .uid = set.uid, .gid = set.gid }) catch return ctx.failed("out of memory mounting the filesystem", .{});
    defer fs.unmount();
    const xv_bound = sym_xv.set_bind.Bound.create(gpa, io, be, &fs.locks, p.env) catch return ctx.failed("out of memory (cross-owner plane)", .{});
    defer xv_bound.destroy(gpa);
    _ = fs.bindXv(&xv_bound.plane) catch |e| return ctx.failed("the cross-owner roll-forward failed ({t})", .{e});
    _ = fs.sweepCorpsesAtMount() catch |e| return ctx.failed("the corpse sweep failed ({t})", .{e});
    set.fs = &fs;
    return ctx.body.run(ctx.body.ctx, &set);
}

/// Resolve an absolute set path (`/a/b/c`) to its inode through the
/// filesystem's own lookups; `parent_of` yields the parent and the leaf
/// name for a creation. Null = a component is missing.
pub const PathWalk = struct {
    parent: u64,
    leaf: []const u8,
    /// The path's own ino when it exists.
    ino: ?u64,
};

pub fn walk(fs: *BackendFs, c: *filesystem.Ctx, path: []const u8) errors.Error!?PathWalk {
    var parent: u64 = 1;
    var it = std.mem.tokenizeScalar(u8, path, '/');
    var leaf: []const u8 = "";
    var last: ?u64 = 1;
    while (it.next()) |name| {
        if (last == null) return null; // a missing intermediate
        if (leaf.len != 0) parent = last.?;
        leaf = name;
        const e = fs.lookup(c, .{ .parent = parent, .name = name }) catch |err| switch (err) {
            error.FileNotFound => {
                last = null;
                continue;
            },
            else => return err,
        };
        last = if (e.nodeid == 0) null else e.nodeid;
    }
    if (leaf.len == 0) return .{ .parent = 1, .leaf = "", .ino = 1 };
    return .{ .parent = parent, .leaf = leaf, .ino = last };
}
