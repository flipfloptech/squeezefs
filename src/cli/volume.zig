//! `squeezefs volume {list, add-meta, remove-meta, repair-set,
//! enable-symmetric, migrate-meta-slot}` (charter §8; §5.4; `docs/porting/
//! dynamic_routing.md` §3, §4): the metadata set's membership verbs over
//! the `sqmeta://` URI. Linux only.
//!
//! `list` is a probe (never blocked by a live mount); `add-meta`,
//! `remove-meta` and `repair-set` are OFFLINE (every member's door; a live
//! writer refuses by name) and run `meta/routing/offline.zig`;
//! `enable-symmetric` is OFFLINE too (`meta/sym/enable_symmetric_bind.zig`:
//! the writer door held, every member of the set converted in URI order,
//! a member already converted skipped — the re-run after a crash converts
//! the rest). `migrate-
//! meta-slot` runs the ONLINE engine (`meta/routing/migrate.zig`: the tree
//! copy, the tee, the gate, the target-first flip) over a set this verb
//! opens exclusively — the live-mount leg (the engine beside a serving
//! daemon, the parked ops, the storm) is exercised in-process by the
//! engine's tests; routing it into a serving daemon is the admin channel
//! of rung 8's job fabric (§7 Q19), so the verb says so when the set is
//! live-mounted instead of pretending. The two `SQUEEZEFS_TEST_MIGRATE_*`
//! seams let the crash matrix run through the binary.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const exec = @import("../exec/exec.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const uring_fs = @import("../io/uring_fs.zig");
const mem_budget = @import("../mem/budget.zig");
const node_cache = @import("../meta/node_cache.zig");
const meta_set = @import("../meta/meta_set.zig");
const routing = @import("../meta/routing.zig");
const set_config = @import("../data/set_config.zig");
const key = @import("../meta/key.zig");
// @anchor:ops_jobs_volume_cli — the data-volume lifecycle verbs (rung 8).
const cli_offline = @import("offline.zig");
const lifecycle = @import("../ops/lifecycle.zig");
const lattice = lifecycle.lattice;
const volume_ops = lifecycle.volume_ops;

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/volume is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const offline = routing.offline;
const migrate = routing.migrate;
const enable_bind = @import("../meta/sym.zig").enable_symmetric_bind;

pub fn run(p: cli.Process, v: args.Volume) u8 {
    const meta_uri = switch (v) {
        inline else => |x| x.meta_uri,
    };
    // The data-volume lifecycle on a LIVE mount: the daemon's admin lane
    // (the lattice over the online world; the add's rebalance and the
    // remove's evacuation run on the fabric at once).
    if (args.targetIsMountpoint(meta_uri)) switch (v) {
        .add_data => |a| {
            var live: LiveData = .{ .p = p, .verb = .{ .add = .{ .device = a.device, .no_rebalance = a.no_rebalance } } };
            return admin_lane.withLane(p, "volume add-data", meta_uri, .{ .ctx = &live, .run = LiveData.run });
        },
        .remove_data => |r| {
            var live: LiveData = .{ .p = p, .verb = .{ .remove = .{ .volume = r.volume } } };
            return admin_lane.withLane(p, "volume remove-data", meta_uri, .{ .ctx = &live, .run = LiveData.run });
        },
        .undrain => |u| {
            var live: LiveData = .{ .p = p, .verb = .{ .undrain = .{ .volume = u.volume } } };
            return admin_lane.withLane(p, "volume undrain", meta_uri, .{ .ctx = &live, .run = LiveData.run });
        },
        else => {},
    };
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs volume: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs volume: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const opts: offline.Options = .{ .env = p.env, .pool = pool, .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(p.env, budget_bytes), .cpus = facts.possible_cpus };
    return switch (v) {
        .list => list(p, opts, meta.paths),
        .add_meta => |a| withExecutor(p, facts, budget_bytes, AddCtx{ .p = p, .opts = opts, .paths = meta.paths, .device = a.device, .take = a.take }, AddCtx.run),
        .remove_meta => |r| withExecutor(p, facts, budget_bytes, RemoveCtx{ .p = p, .opts = opts, .paths = meta.paths, .victim = r.victim }, RemoveCtx.run),
        .repair_set => withExecutor(p, facts, budget_bytes, RepairCtx{ .p = p, .opts = opts, .paths = meta.paths }, RepairCtx.run),
        .enable_symmetric => withExecutor(p, facts, budget_bytes, EnableSymmetricCtx{ .p = p, .opts = opts, .paths = meta.paths }, EnableSymmetricCtx.run),
        .migrate_meta_slot => |m| withExecutor(p, facts, budget_bytes, MigrateCtx{ .p = p, .opts = opts, .paths = meta.paths, .slot = m.slot, .target = m.target }, MigrateCtx.run),
        .add_data => |a| blk: {
            var body: DataBody = .{ .p = p, .verb = .{ .add = .{ .device = a.device, .no_rebalance = a.no_rebalance } } };
            break :blk cli_offline.withSet(p, meta_uri, .{ .with_plane = false, .verb = "volume add-data" }, .{ .ctx = &body, .run = DataBody.run });
        },
        .remove_data => |r| blk: {
            var body: DataBody = .{ .p = p, .verb = .{ .remove = .{ .volume = r.volume } } };
            break :blk cli_offline.withSet(p, meta_uri, .{ .with_plane = true, .verb = "volume remove-data" }, .{ .ctx = &body, .run = DataBody.run });
        },
        .undrain => |u| blk: {
            var body: DataBody = .{ .p = p, .verb = .{ .undrain = .{ .volume = u.volume } } };
            break :blk cli_offline.withSet(p, meta_uri, .{ .with_plane = false, .verb = "volume undrain" }, .{ .ctx = &body, .run = DataBody.run });
        },
    };
}

const admin_lane = @import("admin_lane.zig");

/// The LIVE data-volume verbs through the admin lane: the daemon runs the
/// lattice; the reply's verdict and words are what this prints.
const LiveData = struct {
    p: cli.Process,
    verb: union(enum) {
        add: struct { device: []const u8, no_rebalance: bool },
        remove: struct { volume: []const u8 },
        undrain: struct { volume: []const u8 },
    },

    fn run(ctx: *anyopaque, lane: *admin_lane.Lane) u8 {
        const self: *LiveData = @ptrCast(@alignCast(ctx));
        const p = self.p;
        const c = lane.admin();
        const verb: []const u8 = switch (self.verb) {
            .add => "add-data",
            .remove => "remove-data",
            .undrain => "undrain",
        };
        const rep = switch (self.verb) {
            .add => |a| blk: {
                var abs: [std.Io.Dir.max_path_bytes]u8 = undefined;
                // The daemon opens the path: it must be absolute (the
                // daemon's cwd is not ours).
                const path = if (std.fs.path.isAbsolute(a.device)) a.device else blk2: {
                    const n = std.Io.Dir.cwd().realPathFile(p.io, a.device, &abs) catch {
                        p.stderr.print("squeezefs volume add-data: {s}: cannot resolve to an absolute path (the daemon opens it)\n", .{a.device}) catch {};
                        return cli.exit_failed;
                    };
                    break :blk2 abs[0..n];
                };
                break :blk c.volume(.add_data, 0, path, a.no_rebalance);
            },
            .remove => |r| c.volume(.remove_data, lattice.parseVolumeId(r.volume) orelse return self.badId(verb, r.volume), "", false),
            .undrain => |u| c.volume(.undrain, lattice.parseVolumeId(u.volume) orelse return self.badId(verb, u.volume), "", false),
        } catch |e| {
            p.stderr.print("squeezefs volume {s}: the call failed ({s}, {t})\n", .{ verb, @errorName(e), @import("../core/errors.zig").toErrno(e) }) catch {};
            return cli.exit_failed;
        };
        if (rep.get() != .ok) {
            p.stderr.print("squeezefs volume {s}: refused: {s}\n", .{ verb, admin_lane.verdictText(rep.get()) }) catch {};
            if (rep.get() == .refused and rep.needed > rep.avail) p.stderr.print("  capacity: needed {d} B > avail {d} B (transient {d} B, headroom {d} B)\n", .{ rep.needed, rep.avail, rep.transient, rep.headroom }) catch {};
            p.stderr.flush() catch {};
            return cli.exit_failed;
        }
        switch (self.verb) {
            .add => |a| {
                p.stdout.print("added data volume ", .{}) catch {};
                lattice.writeVolumeId(p.stdout, rep.vol_tag) catch {};
                p.stdout.print(" ({s}, {d} blocks) at config generation {d}, live", .{ a.device, rep.capacity_blocks, rep.config_generation }) catch {};
                if (rep.job_id != 0) p.stdout.print("; rebalance job-{x:0>16} running at 25 %", .{rep.job_id}) catch {};
                p.stdout.writeAll("\n") catch {};
            },
            .remove => |r| {
                p.stdout.print("preflight: needed {d} B, avail {d} B, transient {d} B, headroom {d} B\n{s} is Draining; evacuation job-{x:0>16} running (every block moved once via the ONE mover; the volume retires when its census is empty)\n", .{ rep.needed, rep.avail, rep.transient, rep.headroom, r.volume, rep.job_id }) catch {};
            },
            .undrain => |u| {
                p.stdout.print("{s} is Active again; {d} evacuation job(s) cancelled\n", .{ u.volume, rep.cancelled }) catch {};
            },
        }
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    fn badId(self: *LiveData, verb: []const u8, word: []const u8) u8 {
        self.p.stderr.print("squeezefs volume {s}: {s} is not a vol-{{16 hex}} id\n", .{ verb, word }) catch {};
        self.p.stderr.flush() catch {};
        return cli.exit_usage;
    }
};

/// The OFFLINE data-volume verbs over the writer-door set: the lattice's
/// seven steps through `volume_ops.OfflineWorld`; the add's queued
/// rebalance and the remove's queued evacuation ride the record's tx and
/// are adopted by the next mount's fabric (the online drain).
const DataBody = struct {
    p: cli.Process,
    verb: union(enum) {
        add: struct { device: []const u8, no_rebalance: bool },
        remove: struct { volume: []const u8 },
        undrain: struct { volume: []const u8 },
    },

    fn fail(self: *DataBody, verb: []const u8, e: lattice.Error) u8 {
        const why: []const u8 = switch (e) {
            error.LiveWriter => "a live writer holds a member",
            error.ConfigUndecodable => "the set_config record does not decode (reformat)",
            error.DeviceRefused => "the device is refused: already a member of this set, unsized, unwritable, or a meta volume",
            error.CapacityRefused => "the capacity preflight refused (every term printed above)",
            error.IllegalTransition => "the transition is not in the lattice (only Active drains; only Draining undrains; Retired never comes back — ids are permanent)",
            error.UnknownVolume => "no such data volume in the set",
            error.Io => "a device or commit error (see the log)",
            error.OutOfMemory => "out of memory",
        };
        self.p.stderr.print("squeezefs volume {s}: refused: {s}\n", .{ verb, why }) catch {};
        self.p.stderr.flush() catch {};
        return cli.exit_failed;
    }

    fn run(ctx: *anyopaque, set: *cli_offline.Set) u8 {
        const self: *DataBody = @ptrCast(@alignCast(ctx));
        const p = self.p;
        var world: volume_ops.OfflineWorld = .{ .gpa = p.gpa, .io = set.io, .be = set.be, .plane = set.plane };
        defer world.deinit();
        switch (self.verb) {
            .add => |a| {
                const dev = volume_ops.probeDevice(p.gpa, set.io, a.device, set.be.setUuid(), set.cfg.chunk_size) catch |e| return self.fail("add-data", e);
                const out = lattice.addData(p.gpa, world.world(), dev, !a.no_rebalance) catch |e| return self.fail("add-data", e);
                p.stdout.print("added data volume ", .{}) catch {};
                lattice.writeVolumeId(p.stdout, out.vol_tag) catch {};
                p.stdout.print(" ({s}, {d} blocks) at config generation {d}", .{ a.device, dev.capacity_blocks, out.config_generation }) catch {};
                if (out.rebalance_job != 0) p.stdout.print("; queued rebalance job-{x:0>16} at 25 % (the next mount's fabric runs it)", .{out.rebalance_job}) catch {};
                p.stdout.writeAll("\n") catch {};
            },
            .remove => |r| {
                const tag = (lifecycle.lattice.parseVolumeId(r.volume) orelse return self.fail("remove-data", error.UnknownVolume));
                const plane = set.plane orelse return self.fail("remove-data", error.Io);
                const pf = volume_ops.preflightDrain(p.gpa, plane, tag, 1) catch return self.fail("remove-data", error.Io);
                pf.write(p.stdout) catch {};
                p.stdout.writeAll("\n") catch {};
                const job = lattice.removeData(p.gpa, world.world(), tag, pf, true) catch |e| return self.fail("remove-data", e);
                p.stdout.print("{s} is Draining; queued evacuation job-{x:0>16} — the drain runs on the next mount's fabric (every block moved once via the ONE mover; the volume retires when its census is empty)\n", .{ r.volume, job }) catch {};
            },
            .undrain => |u| {
                const tag = (lifecycle.lattice.parseVolumeId(u.volume) orelse return self.fail("undrain", error.UnknownVolume));
                lattice.undrain(p.gpa, world.world(), tag) catch |e| return self.fail("undrain", e);
                p.stdout.print("{s} is Active again; {d} evacuation job(s) cancelled durably\n", .{ u.volume, world.cancelled.items.len }) catch {};
            },
        }
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

fn list(p: cli.Process, opts: offline.Options, paths: []const []const u8) u8 {
    p.stdout.print("metadata set ({d} member(s) listed):\n", .{paths.len}) catch {};
    offline.list(p.gpa, opts, paths, p.stdout) catch |e| {
        p.stderr.writeAll("squeezefs ") catch {};
        offline.writeError(p.stderr, e, "list") catch {};
        p.stderr.writeAll("\n") catch {};
        return cli.exit_failed;
    };
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

/// The offline verbs open members through the backend, whose pass fibers
/// need our executor: the body runs as the root task.
fn withExecutor(p: cli.Process, facts: derive.Facts, budget_bytes: u64, ctx: anytype, comptime body: fn (*@TypeOf(ctx), Io) u8) u8 {
    const sqz = exec.SqzIo.init(p.gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs volume: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    var c = ctx;
    const Runner = struct {
        fn main(cx: *@TypeOf(c), io: Io) u8 {
            return body(cx, io);
        }
    };
    return sqz.run(Runner.main, .{ &c, sqz.io() }) catch |e| {
        p.stderr.print("squeezefs volume: the root task could not start ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
}

fn refused(p: cli.Process, e: offline.Error, verb: []const u8) u8 {
    p.stderr.writeAll("squeezefs ") catch {};
    offline.writeError(p.stderr, e, verb) catch {};
    p.stderr.writeAll("\n") catch {};
    p.stderr.flush() catch {};
    return cli.exit_failed;
}

/// The set config's cache paths (the KD-8 roots): read off member 0's
/// control tree through a probe-class open of the set; a set with no
/// record (never formatted by this binary) rebinds nothing.
fn stagingRoots(gpa: std.mem.Allocator, io: Io, opts: offline.Options, paths: []const []const u8, out: *std.ArrayList([]u8)) void {
    const set = meta_set.MetaSet.open(gpa, io, paths, .{ .env = opts.env, .pool = opts.pool, .cache_budget_bytes = opts.cache_budget_bytes, .cpus = opts.cpus, .read_only = true }) catch return;
    defer set.close();
    const value = (set.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return) orelse return;
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch return;
    defer cfg.deinit(gpa);
    for (cfg.cache_paths) |cp| out.append(gpa, gpa.dupe(u8, cp) catch return) catch return;
}

fn freeRoots(gpa: std.mem.Allocator, roots: *std.ArrayList([]u8)) void {
    for (roots.items) |r| gpa.free(r);
    roots.deinit(gpa);
}

const AddCtx = struct {
    p: cli.Process,
    opts: offline.Options,
    paths: []const []const u8,
    device: []const u8,
    take: args.TakeSlots,

    fn run(self: *AddCtx, io: Io) u8 {
        const gpa = self.p.gpa;
        var slots: std.ArrayList(u16) = .empty;
        defer slots.deinit(gpa);
        const take: offline.TakeSlots = switch (self.take) {
            .count => |k| .{ .count = k },
            .list => |raw| blk: {
                var it = std.mem.splitScalar(u8, raw, ',');
                while (it.next()) |m| slots.append(gpa, std.fmt.parseInt(u16, m, 10) catch unreachable) catch return cli.exit_failed;
                break :blk .{ .list = slots.items };
            },
        };
        var roots: std.ArrayList([]u8) = .empty;
        defer freeRoots(gpa, &roots);
        stagingRoots(gpa, io, self.opts, self.paths, &roots);
        var opts = self.opts;
        opts.staging_roots = @ptrCast(roots.items);
        const r = offline.addMeta(gpa, io, opts, self.paths, self.device, take) catch |e| return refused(self.p, e, "add-meta");
        defer gpa.free(r.taken);
        self.p.stdout.print("volume add-meta: {s} joined as member {d} at epoch {d}{s}; took {d} slot(s) [", .{ self.device, r.position, r.epoch, if (r.resumed) " (a crashed run resumed)" else "", r.taken.len }) catch {};
        for (r.taken, 0..) |s, i| self.p.stdout.print("{s}{d}", .{ if (i == 0) "" else ", ", s }) catch {};
        self.p.stdout.print("]; {d} records copied. Mount with the extended URI (every member listed).\n", .{r.records_copied}) catch {};
        self.p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

const RemoveCtx = struct {
    p: cli.Process,
    opts: offline.Options,
    paths: []const []const u8,
    victim: []const u8,

    fn run(self: *RemoveCtx, io: Io) u8 {
        const gpa = self.p.gpa;
        var roots: std.ArrayList([]u8) = .empty;
        defer freeRoots(gpa, &roots);
        stagingRoots(gpa, io, self.opts, self.paths, &roots);
        var opts = self.opts;
        opts.staging_roots = @ptrCast(roots.items);
        const r = offline.removeMeta(gpa, io, opts, self.paths, self.victim) catch |e| return refused(self.p, e, "remove-meta");
        self.p.stdout.print("volume remove-meta: {s} retired at epoch {d} (tombstone written LAST); {d} slot(s) moved onto the survivors, {d} records copied. Mount with the survivors' URI (the tombstone refuses discovery if listed).\n", .{ self.victim, r.epoch, r.moved_slots, r.records_copied }) catch {};
        self.p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

const RepairCtx = struct {
    p: cli.Process,
    opts: offline.Options,
    paths: []const []const u8,

    fn run(self: *RepairCtx, io: Io) u8 {
        const r = offline.repairSet(self.p.gpa, io, self.opts, self.paths, self.p.stdout) catch |e| {
            self.p.stdout.flush() catch {};
            return refused(self.p, e, "repair-set");
        };
        self.p.stdout.print("volume repair-set: {d} member(s) re-stamped{s}\n", .{ r.restamped, if (r.inferred) " (one inferred)" else "" }) catch {};
        self.p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

/// `volume enable-symmetric`: every member through the device binding;
/// a member already carrying the plane is skipped (a crashed run's
/// resume); any other refusal stops the walk, named.
const EnableSymmetricCtx = struct {
    p: cli.Process,
    opts: offline.Options,
    paths: []const []const u8,

    fn run(self: *EnableSymmetricCtx, io: Io) u8 {
        const p = self.p;
        var converted: usize = 0;
        var skipped: usize = 0;
        for (self.paths) |path| {
            var diag: enable_bind.Refusal = undefined;
            const out = enable_bind.enableSymmetric(p.gpa, io, path, .{ .env = self.opts.env, .pool = self.opts.pool }, &diag) catch |e| switch (e) {
                error.AlreadySymmetric => {
                    p.stdout.print("volume enable-symmetric: {s}: ", .{path}) catch {};
                    diag.write(p.stdout) catch {};
                    p.stdout.writeAll(" (skipped)\n") catch {};
                    skipped += 1;
                    continue;
                },
                error.Mounted, error.WindowNotEmpty, error.JournalTooSmall, error.NoFreeExtent, error.Corrupt => {
                    p.stderr.print("squeezefs volume enable-symmetric: {s}: refused: ", .{path}) catch {};
                    diag.write(p.stderr) catch {};
                    p.stderr.writeAll("\n") catch {};
                    p.stderr.flush() catch {};
                    p.stdout.flush() catch {};
                    return cli.exit_failed;
                },
                error.DeviceRefused, error.NotFormatted, error.Io, error.OutOfMemory => {
                    p.stderr.print("squeezefs volume enable-symmetric: {s}: {s}\n", .{ path, @errorName(e) }) catch {};
                    p.stderr.flush() catch {};
                    p.stdout.flush() catch {};
                    return cli.exit_failed;
                },
            };
            p.stdout.print("volume enable-symmetric: {s}: SYMMETRIC_PLANE enabled — appender directory at 0x{x} ({s}), ring proper {d} B of the {d} B journal{s}\n", .{ path, out.link.start, if (out.resumed) "a crashed run's link resumed" else "claimed", out.ring_bytes, out.journal_len, if (out.ledger_seq) |_| ", the ledger restated at the claim's generation" else "" }) catch {};
            converted += 1;
        }
        p.stdout.print("volume enable-symmetric: {d} member(s) converted, {d} already symmetric. The next `mount` is the manager; later RW mounts join it over the wire.\n", .{ converted, skipped }) catch {};
        p.stdout.flush() catch {};
        // Nothing to do on a set that was symmetric throughout IS the
        // already-symmetric refusal, loud.
        return if (converted == 0) cli.exit_failed else cli.exit_ok;
    }
};

const MigrateCtx = struct {
    p: cli.Process,
    opts: offline.Options,
    paths: []const []const u8,
    slot: u16,
    target: u16,

    fn run(self: *MigrateCtx, io: Io) u8 {
        const gpa = self.p.gpa;
        const set = meta_set.MetaSet.open(gpa, io, self.paths, .{ .env = self.opts.env, .pool = self.opts.pool, .cache_budget_bytes = self.opts.cache_budget_bytes, .cpus = self.opts.cpus }) catch |e| {
            if (e == error.Busy) {
                self.p.stderr.writeAll("squeezefs volume migrate-meta-slot: the set is live-mounted — the online engine beside a serving daemon rides rung 8's admin channel (the job fabric); until then run the verb with the set unmounted (the same engine over an exclusively opened set)\n") catch {};
                return cli.exit_failed;
            }
            self.p.stderr.print("squeezefs volume migrate-meta-slot: cannot open the set: {s} ({t})\n", .{ @errorName(e), meta_set.openErrno(e) }) catch {};
            return cli.exit_failed;
        };
        defer set.close();
        var opts = migrate.Options.derive(self.opts.cache_budget_bytes, self.opts.cpus);
        var hold: ?migrate.Hold = null;
        const hold_name = knobs.enumKnob("SQUEEZEFS_TEST_MIGRATE_HOLD", self.opts.env);
        if (std.meta.stringToEnum(migrate.HoldPhase, hold_name)) |phase| if (phase != .off) {
            hold = migrate.Hold.init(phase);
            opts.hold = &hold.?;
        };
        if (knobs.optIntKnob("SQUEEZEFS_TEST_MIGRATE_CRASH_AFTER_WRITE", u8, self.opts.env)) |n| opts.crash_after_write = n;
        const report = migrate.migrateSlot(set, self.slot, self.target, opts) catch |e| {
            self.p.stderr.writeAll("squeezefs volume migrate-meta-slot: ") catch {};
            migrate.writeRefusal(self.p.stderr, e, self.slot, self.target, set.volumeCount()) catch {};
            self.p.stderr.writeAll("\n") catch {};
            self.p.stderr.flush() catch {};
            // The crash seam leaves the devices as a kill would and exits
            // without the clean close.
            if (e == error.CrashSeam) std.process.exit(cli.exit_failed);
            return cli.exit_failed;
        };
        self.p.stdout.writeAll("volume migrate-meta-slot: ") catch {};
        report.write(self.p.stdout) catch {};
        self.p.stdout.writeAll("\n") catch {};
        self.p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};
