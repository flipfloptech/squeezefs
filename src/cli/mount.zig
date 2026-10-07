//! `squeezefs mount` (charter §8; §4 "Bring-up (ordered)"; §5.7 layer A;
//! §9 "`main` drives the CLI with a thread-park `block_on`"): the daemon's
//! bring-up in the charter's order INSIDE `SqzIo.run`, the serve loop, and
//! the teardown in the charter's order. Linux only.
//!
//! Bring-up (the root task on the `main` lane):
//!   resolve the budget (pure) → open the backend (the D0 ladder's layer A:
//!   the flock; a FAILED/refusal class is loud and names its errno through
//!   the ONE table) → the set config (`set_config` in tree 0: present,
//!   decodable, the stamp's set; every data volume's device read and
//!   checked against its record — the §5.4 URI-disagreement refusals; an
//!   encrypted set resolves and verifies its key here — a wrong key file
//!   refuses the mount instead of failing every read) → start the budget
//!   sampler → the handler tables (watchdog registry, op guards, open
//!   table) → the DATA PLANE (`data/plane.zig`: devices, allocators seeded
//!   from the ledger, the router, the reclaimer, staging + the NVMe tier
//!   under the cache paths, the transform keyed from the verified key, the
//!   write and read paths) → `KvFs(KvBackend, DataPlane)` + the ring
//!   `Binding` with the plane's queue-thread arms → the FUSE fd: root opens
//!   `/dev/fuse` inside the session's `mount(2)`; anyone else receives it
//!   from `fusermount3` (`fusermount.zig`) → `Session.mount` (INIT,
//!   geometry, `enable_uring`, workers, the arm barrier) → the plane's zc
//!   legs armed from the session's buffer mode → the watchdog loop and the
//!   reclaim cadence on `spawnMeta` → `ready` (logged; written to the
//!   supervisor pipe under `--daemon`).
//!
//! Serve: the main task parks on the daemon's eventfd through its lane ring
//! (`SqzIo.readable`): a signal (`daemon.Signals`) or the session's
//! `on_shutdown` hook (DESTROY from an external `umount`, a disconnect, a
//! failed queue) wakes it. SIGUSR1 dumps every stats family to the log —
//! the `.stats` inode's stand-in until rung 5. Stop latches; nothing is
//! aborted from outside.
//!
//! Teardown: `Session.close` (drains synthesize every owed reply, the
//! unmount, the workers join) → the watchdog and the cadence cancelled →
//! `Session.deinit` → the filesystem's dismount sweep → `DataPlane.close`
//! (the dismount pass: staged files promote, packs seal DISMOUNT, the
//! reclaimer drains, the devices close — charter §8 `umount`) →
//! `KvBackend.close` (the final checkpoint: an empty replay window for the
//! next mount) → the sampler → exit 0; a volume that latched FAILED exits
//! 1 naming the reason.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const daemon = @import("daemon.zig");
const fusermount = @import("fusermount.zig");
const exec = @import("../exec/exec.zig");
const exec_stats = @import("../exec/stats.zig");
const backend = @import("../meta/backend.zig");
const checkpoint = @import("../meta/checkpoint.zig");
const journal_lane = @import("../meta/journal_lane.zig");
const node_cache = @import("../meta/node_cache.zig");
const dlm = @import("../meta/dlm.zig");
const handler = @import("../fuse/handler.zig");
const transport = @import("../fuse/transport.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const mount_options = @import("../fuse/mount_options.zig");
const mem_budget = @import("../mem/budget.zig");
const key = @import("../meta/key.zig");
const set_config = @import("../data/set_config.zig");
const transform = @import("../data/transform.zig");
const data_superblock = @import("../data/data_superblock.zig");
const plane_mod = @import("../data/plane.zig");
const slot = @import("../fuse/transport/slot.zig");
const sync_tick = @import("../sync/tick.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/mount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const SqzIo = exec.SqzIo;
const Session = transport.session.Session;
const BackendFs = handler.BackendFs;
const watchdog = handler.watchdog;
const op_guard = handler.op_guard;
const op_trace = handler.op_trace;
const open_table = handler.open_table;
const filesystem = handler.filesystem;

/// The reclaim cadence: a partial corpse batch (< `kv_fs.RECLAIM_BATCH`)
/// waits at most one watchdog tick (5 s) for its destroy entry. The
/// entries/op economy (≤ 1.05 per unlink, charter §5.2) is measured on
/// FULL batches under storms; a tail of corpses idle for one tick costs
/// one entry, and the tick is the slowest periodic cadence the daemon
/// already runs.
pub const RECLAIM_CADENCE_NS: u64 = watchdog.TICK_NS;

/// The verb's state, pinned in `run`'s frame for the root task.
const Ctx = struct {
    p: cli.Process,
    sqz: *SqzIo,
    facts: derive.Facts,
    budget_bytes: u64,
    budget_host: mem_budget.Host,
    path: []const u8,
    mountpoint: [:0]const u8,
    opts: mount_options.MountOptions,
    /// The raw `-o` string (the fusermount3 path forwards it).
    reporter: ?daemon.Reporter,
    session_ended: std.atomic.Value(bool) = .init(false),
    /// The verified volume key of an encrypted set (zeroized once the plane
    /// holds its crypto state).
    derived_key: ?transform.DerivedKey = null,

    /// Report a bring-up failure: the log, stderr (foreground) or the
    /// supervisor pipe (daemon); exit 1.
    fn failed(self: *Ctx, comptime fmt: []const u8, a: anytype) u8 {
        var buf: [1024]u8 = undefined;
        const text = std.fmt.bufPrint(&buf, fmt, a) catch buf[0..];
        log.err("mount: {s}", .{text});
        if (self.reporter) |*r| {
            r.fail(text);
            r.close();
        } else {
            self.p.stderr.print("squeezefs mount: {s}\n", .{text}) catch {};
            self.p.stderr.flush() catch {};
        }
        return cli.exit_failed;
    }
};

fn onSessionShutdown(ctx_ptr: *anyopaque) void {
    const ctx: *Ctx = @ptrCast(@alignCast(ctx_ptr));
    // release pairs with the serve loop's acquire after the eventfd wake.
    ctx.session_ended.store(true, .release);
    daemon.Signals.ring();
}

const UnmountCtx = struct {
    io: Io,
    fn call(ctx_ptr: *anyopaque, mountpoint: [:0]const u8) bool {
        const self: *UnmountCtx = @ptrCast(@alignCast(ctx_ptr));
        return fusermount.unmount(self.io, mountpoint);
    }
};

pub fn run(p: cli.Process, m: args.Mount) u8 {
    // The URI: one member at this rung.
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, m.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs mount: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    if (meta.paths.len != 1) {
        p.stderr.print("squeezefs mount: {s} names {d} metadata volumes; this rung mounts ONE (a metadata set is rung 5's)\n", .{ m.meta_uri, meta.paths.len }) catch {};
        return cli.exit_failed;
    }
    // The mountpoint: an existing directory.
    var mp_buf: [linux.PATH_MAX:0]u8 = undefined;
    const mountpoint = std.fmt.bufPrintSentinel(&mp_buf, "{s}", .{m.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs mount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    {
        var st: linux.Statx = undefined;
        const rc = linux.statx(linux.AT.FDCWD, mountpoint.ptr, 0, .{ .TYPE = true }, &st);
        if (linux.errno(rc) != .SUCCESS) {
            p.stderr.print("squeezefs mount: mountpoint {s}: {t}\n", .{ mountpoint, linux.errno(rc) }) catch {};
            return cli.exit_failed;
        }
        if (!linux.S.ISDIR(st.mode)) {
            p.stderr.print("squeezefs mount: mountpoint {s} is not a directory\n", .{mountpoint}) catch {};
            return cli.exit_failed;
        }
    }
    // The options: the env TTLs as the base, the process's ids, root's
    // `allow_other` posture (an unprivileged mount needs
    // `user_allow_other` and asks for it with `-o allow_other`).
    const root = linux.geteuid() == 0;
    var opts: mount_options.MountOptions = .{ .ttls = mount_options.Ttls.fromEnv(p.env), .uid = linux.geteuid(), .gid = linux.getegid(), .allow_other = root };
    if (m.options) |text| switch (mount_options.parseOptionString(&opts, text)) {
        .ok => {},
        .invalid => |d| {
            p.stderr.print("squeezefs mount: {f}\n", .{d}) catch {};
            return cli.exit_usage;
        },
    };

    // `--daemon`: the log file first (its refusal reaches the terminal),
    // then the fork — before any thread exists.
    var reporter: ?daemon.Reporter = null;
    if (m.daemon) {
        var lf_buf: [linux.PATH_MAX:0]u8 = undefined;
        const lfz = std.fmt.bufPrintSentinel(&lf_buf, "{s}", .{m.log_file.?}, 0) catch {
            p.stderr.writeAll("squeezefs mount: the --log-file path is too long\n") catch {};
            return cli.exit_usage;
        };
        const log_fd = daemon.openLogFile(lfz, p.stderr) catch return cli.exit_failed;
        p.stderr.flush() catch {};
        switch (daemon.detach(p.gpa, p.io, .{ .supervise = m.supervise, .mountpoint = mountpoint, .log_fd = log_fd }, p.stderr)) {
            .parent => |code| return code,
            .daemon => |r| reporter = r,
        }
    }
    daemon.Signals.install() catch {
        p.stderr.writeAll("squeezefs mount: eventfd failed\n") catch {};
        return cli.exit_failed;
    };

    // The budget (pure resolution now; the sampler after the backend) and
    // the executor: fuse lanes sized to the transport's desired geometry.
    const facts = derive.facts();
    const host: mem_budget.Host = .{ .io = p.io, .env = p.env, .facts = facts };
    const budget_bytes = mem_budget.authority.resolveBudgetNow(host);
    const sqz = SqzIo.init(p.gpa, .{
        .env = p.env,
        .facts = facts,
        .budget_bytes = budget_bytes,
        .budget = &mem_budget.authority,
        .fuse = .{ .nqueues = facts.possible_cpus, .q_depth = fuse_geometry.Q_DEPTH_DESIRED },
    }) catch |e| {
        p.stderr.print("squeezefs mount: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        if (reporter) |*r| {
            r.fail("executor init failed");
            r.close();
        }
        return cli.exit_failed;
    };
    defer sqz.deinit();
    var ctx: Ctx = .{
        .p = p,
        .sqz = sqz,
        .facts = facts,
        .budget_bytes = budget_bytes,
        .budget_host = host,
        .path = meta.first(),
        .mountpoint = mountpoint,
        .opts = opts,
        .reporter = reporter,
    };
    return sqz.run(mountMain, .{&ctx}) catch |e| ctx.failed("the root task could not start ({s})", .{@errorName(e)});
}

/// The root task (module doc).
fn mountMain(ctx: *Ctx) u8 {
    const gpa = ctx.p.gpa;
    const env = ctx.p.env;
    const io = ctx.sqz.io();
    const facts = ctx.facts;

    // 1. The backend: layer A, the ladder, the lane, the checkpoint task.
    const be = backend.KvBackend.open(gpa, io, ctx.path, .{
        .env = env,
        .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(env, ctx.budget_bytes),
        .cpus = facts.possible_cpus,
    }) catch |e| return ctx.failed("cannot open metadata volume {s}: {s} ({t}, {s})", .{ ctx.path, openReason(e), backend.openErrno(e), @tagName(errors.classOf(backend.openCoreError(e))) });
    var be_open = true;
    defer if (be_open) be.close();

    // 1b. The set config: the one control record, the stamp's set, the key
    //     gate on an encrypted set.
    var cfg = loadSetConfig(ctx, be) orelse return cli.exit_failed;
    defer cfg.deinit(gpa);

    // 2. The budget sampler (the registry already holds the fiber stacks;
    //    the arena joins at the session).
    _ = mem_budget.authority.spawnSampler(ctx.budget_host, mem_budget.sampler_period_ns) catch |e| return ctx.failed("cannot start the memory-budget sampler ({s})", .{@errorName(e)});
    var sampler_live = true;
    defer if (sampler_live) mem_budget.authority.stopSampler(ctx.p.io);

    // 3. The handler tables, the data plane, the filesystem, the binding.
    var registry = watchdog.Registry.init(gpa, watchdog.derivedShards(facts.possible_cpus)) catch return ctx.failed("out of memory (watchdog registry)", .{});
    defer registry.deinit();
    var guards = op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (op guards)", .{});
    defer guards.deinit();
    var opens = open_table.OpenTable.init(gpa, open_table.derivedCapacity(facts.possible_cpus, fuse_geometry.Q_DEPTH_DESIRED)) catch return ctx.failed("out of memory (open table)", .{});
    defer opens.deinit();
    const plane = plane_mod.DataPlane.open(gpa, io, ctx.sqz, be, &cfg, .{ .env = env, .facts = facts, .budget_bytes = ctx.budget_bytes, .derived_key = ctx.derived_key, .guards = &guards }) catch |e| return ctx.failed("the data plane could not open ({s}) — see the log", .{@errorName(e)});
    if (ctx.derived_key) |*k| k.zeroize();
    var plane_open = true;
    defer if (plane_open) plane.close();
    var fs = BackendFs.mount(gpa, io, be, plane, .{
        .ttls = ctx.opts.ttls,
        .page_size = facts.page_size,
        .dlm_stripes = dlm.resolveWidth(env, facts, fuse_geometry.Q_DEPTH_DESIRED),
    }) catch return ctx.failed("out of memory mounting the filesystem", .{});
    defer fs.unmount();
    // The corpse sweep (kv_fs module doc): what a crashed daemon's forgets
    // left at nlink 0 is destroyed before any request is served.
    _ = fs.sweepCorpsesAtMount() catch |e| return ctx.failed("the corpse sweep at mount failed ({t})", .{e});
    op_trace.arm(env);
    var binding: handler.ring.Binding = .{};
    binding.bind(.{
        .fs = filesystem.Filesystem.of(BackendFs, &fs),
        .io = io,
        .registry = &registry,
        .guards = &guards,
        .opens = &opens,
        .probe = plane.probe(),
        .shared_enabled = op_guard.sharedEnabled(env),
    });
    binding.bindData(.{ .plane = plane });

    // 4. The FUSE fd (root: inside the session; else fusermount3) and the
    //    session's ordered bring-up.
    var unmount_ctx: UnmountCtx = .{ .io = io };
    var fuse_fd: ?linux.fd_t = null;
    var unmounter: ?transport.session.Unmounter = null;
    if (linux.geteuid() != 0) {
        var ob: [1024]u8 = undefined;
        var ow: Io.Writer = .fixed(&ob);
        ctx.opts.writeFusermountOptions(&ow) catch return ctx.failed("the -o string is too long", .{});
        fuse_fd = fusermount.mount(gpa, io, env, ctx.mountpoint, ow.buffered()) catch |e| return ctx.failed("fusermount3 could not mount {s} ({s})", .{ ctx.mountpoint, @errorName(e) });
        unmounter = .{ .ctx = &unmount_ctx, .call = UnmountCtx.call };
    }
    const session = Session.mount(gpa, ctx.sqz, binding.filesystem(), ctx.mountpoint, env, facts, .{
        .opts = ctx.opts,
        .block_size = cfg.block_size,
        .mem_budget_bytes = ctx.budget_bytes,
        .budget = &mem_budget.authority,
        .fuse_fd = fuse_fd,
        .unmounter = unmounter,
        .on_shutdown = .{ .ctx = ctx, .call = onSessionShutdown },
    }) catch |e| {
        if (fuse_fd) |fd| {
            _ = fusermount.unmount(io, ctx.mountpoint);
            _ = linux.close(fd);
        }
        return ctx.failed("the FUSE session could not arm at {s} ({s}) — see the log", .{ ctx.mountpoint, @errorName(e) });
    };
    var session_live = true;
    defer if (session_live) session.deinit();
    // The zc legs: the held-slot direct store on a zero-copy session, the
    // direct device leg, the fd-source probe serve.
    var held_store: HeldStore = .{ .session = session };
    plane.armSession(if (session.heldStoreArmed()) held_store.face() else null, session.mode == .zero_copy, session.readZcServe());

    // 5. The watchdog loop and the reclaim cadence on the meta pool.
    var wd = watchdog.Watchdog.init(&registry);
    var wd_fut = ctx.sqz.spawnMeta(watchdogMain, .{ &wd, io }) catch return ctx.failed("cannot spawn the watchdog", .{});
    var reclaim: ReclaimCadence = .{ .fs = &fs, .io = io };
    var reclaim_fut = ctx.sqz.spawnMeta(ReclaimCadence.run, .{&reclaim}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        return ctx.failed("cannot spawn the reclaim cadence", .{});
    };

    // 6. Ready.
    log.info("ready: {s} mounted at {s} (pid {d}; {d} queue(s) × depth {d}; budget {d} MiB; TTLs attr {d} ms entry {d} ms dir_entry {d} ms negative {d} ms)", .{ ctx.path, ctx.mountpoint, linux.getpid(), session.plan.nqueues, session.plan.depth, ctx.budget_bytes >> 20, ctx.opts.ttls.attr_ns / std.time.ns_per_ms, ctx.opts.ttls.entry_ns / std.time.ns_per_ms, ctx.opts.ttls.dir_entry_ns / std.time.ns_per_ms, ctx.opts.ttls.negative_ns / std.time.ns_per_ms });
    if (ctx.reporter) |*r| {
        r.ready();
        r.close();
    }

    // 7. Serve until a stop latches or the session ends.
    serve(ctx, session, be, plane);

    // 8. Teardown in order.
    session.close();
    wd.requestStop();
    wd_fut.cancel(io);
    reclaim.requestStop();
    reclaim_fut.cancel(io);
    const failed = be.isFailed();
    const stranded = counters.tripwireCount("open_count_stranded");
    const abandoned = transport.slot.requests_abandoned.load();
    log.info("teardown: session closed; open_count_stranded={d} transport_requests_abandoned={d} tick_rescues={d}/{d}", .{ stranded, abandoned, transport.worker.park_tick_commit_rescues.load(), transport.worker.park_tick_cqe_rescues.load() });
    session_live = false;
    session.deinit();
    // The sampler stops BEFORE the plane's components (its gauges) go; the
    // dismount pass BEFORE the backend's final checkpoint (its promotions
    // are layout transactions); the `defer`s unwind the filesystem and the
    // tables after it.
    sampler_live = false;
    mem_budget.authority.stopSampler(ctx.p.io);
    plane_open = false;
    plane.close();
    be_open = false;
    be.close();
    if (failed) {
        log.err("the volume {s} latched FAILED during this mount (EIO until remount) — exiting 1", .{ctx.path});
        return cli.exit_failed;
    }
    return cli.exit_ok;
}

/// Read + decode `set_config` from tree 0 (the record a format writes;
/// its absence or refusal is a mount refusal by name), discover the data
/// volumes (the record names the stamp's set; every non-retired record's
/// device reads back as itself — order-independent, every disagreement
/// refused by name), and on an encrypted set resolve the key through the
/// ONE resolution order (`SQUEEZEFS_ENCRYPT_KEY_FILE` → the key dir;
/// `mount` grows `--encrypt-key` with the data plane) and verify its id.
/// Logs the summary at ready; null = refused (reported).
fn loadSetConfig(ctx: *Ctx, be: *backend.KvBackend) ?set_config.Config {
    const gpa = ctx.p.gpa;
    const value = be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch |e| {
        _ = ctx.failed("cannot read the set_config record of {s}: {s}", .{ ctx.path, @errorName(e) });
        return null;
    } orelse {
        _ = ctx.failed("{s} carries no set_config record in its control tree — not member 0 of its set, or formatted before the record existed; reformat", .{ctx.path});
        return null;
    };
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        diag.write(&w) catch {};
        _ = ctx.failed("{s}: {s}", .{ ctx.path, w.buffered() });
        return null;
    };
    {
        // Every data device read once (O_DIRECT, both superblock copies)
        // and released: the data plane's own opens come with its wiring.
        const obs = gpa.alloc(set_config.Observation, cfg.data.len) catch {
            _ = ctx.failed("out of memory (data volume discovery)", .{});
            cfg.deinit(gpa);
            return null;
        };
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
        set_config.discover(&cfg, be.mounted.membership_stamp.set_uuid, obs, &ddiag) catch {
            var buf: [1024]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            ddiag.write(&w) catch {};
            _ = ctx.failed("data volume discovery: {s}", .{w.buffered()});
            cfg.deinit(gpa);
            return null;
        };
    }
    if (cfg.encryption != .none) {
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        const derived = transform.linux_keys.resolveVolumeKey(ctx.p.env, null, &cfg.kdf_salt, cfg.key_id, &path_buf, &served) catch |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, if (served.len != 0) served else transform.defaultKeyPath(&path_buf, cfg.key_id)) catch {};
            _ = ctx.failed("encrypted set: {s}", .{w.buffered()});
            cfg.deinit(gpa);
            return null;
        };
        // The KEK and key id travel to the data plane's crypto state (which
        // copies them); the caller zeroizes this copy right after.
        ctx.derived_key = derived;
        log.info("mount: encryption key verified ({s}, id {x}, from {s})", .{ transform.KEY_WRAP_SCHEME_NAME, &cfg.key_id, served });
    }
    var out: Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    cfg.writeSummary(&out.writer) catch {};
    log.info("mount: set config\n{s}", .{std.mem.trimEnd(u8, out.written(), "\n")});
    return cfg;
}

fn openReason(e: backend.OpenError) []const u8 {
    return switch (e) {
        error.Busy => "another writer holds the single-writer guard (flock)",
        error.NotFormatted => "not formatted (zeroed superblock) — run `squeezefs format` first",
        error.LegacyFormat => "a pre-v4 squeezefs format, which this binary does not mount — reformat required",
        error.UnsupportedFormat => "unknown or later-rung capability bits — reformat or a later binary",
        error.Corrupt => "corruption outside the replay window (see the log)",
        error.DeviceRefused => "the device refused registration (O_DIRECT posture) or does not exist",
        error.Io => "device I/O failed",
        error.OutOfMemory => "out of memory",
        error.LaneSpawnFailed, error.ThreadSpawnFailed => "cannot spawn the journal lane or the checkpoint thread",
    };
}

fn watchdogMain(wd: *watchdog.Watchdog, io: Io) void {
    wd.run(io);
}

/// The periodic drain of the corpse gather (module doc).
const ReclaimCadence = struct {
    fs: *BackendFs,
    io: Io,
    stop: std.atomic.Value(bool) = .init(false),

    fn requestStop(self: *ReclaimCadence) void {
        self.stop.store(true, .release);
    }

    fn run(self: *ReclaimCadence) void {
        while (!self.stop.load(.acquire)) {
            self.io.sleep(.fromNanoseconds(RECLAIM_CADENCE_NS), .awake) catch return;
            if (self.stop.load(.acquire)) return;
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            self.fs.reclaimDrain(&ctx) catch |e| log.err("reclaim cadence: drain failed: {t}", .{e});
        }
    }
};

/// The plane's `write_fixed` face: the session's `storeHeld`.
const HeldStore = struct {
    session: *Session,

    fn face(self: *HeldStore) plane_mod.HeldStoreFace {
        return .{ .ctx = self, .store = &store };
    }

    fn store(ctx: *anyopaque, src: slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) slot.ExtractError!void {
        const self: *HeldStore = @ptrCast(@alignCast(ctx));
        return self.session.storeHeld(src, fd, off, io);
    }
};

fn serve(ctx: *Ctx, session: *Session, be: *backend.KvBackend, plane: *plane_mod.DataPlane) void {
    const efd = daemon.Signals.eventfd();
    while (true) {
        _ = ctx.sqz.readable(efd) catch |e| {
            log.err("mount: waiting on the signal eventfd failed ({t}) — stopping", .{e});
            return;
        };
        daemon.Signals.drain();
        const bits = daemon.Signals.take();
        if (bits & daemon.SIG_STATS != 0) dumpStats(ctx, session, be, plane);
        if (bits & daemon.SIG_STOP != 0) {
            log.info("stop requested (SIGTERM/SIGINT): unmounting {s}", .{ctx.mountpoint});
            return;
        }
        // acquire pairs with the hook's release.
        if (ctx.session_ended.load(.acquire) or !session.isReady()) {
            log.info("the FUSE session ended ({s}): tearing down", .{if (session.isMounted()) "shutdown" else "unmounted"});
            return;
        }
    }
}

/// Every stats family, one log line each (the `.stats` inode's rung-5 JSON,
/// delivered through the log on SIGUSR1).
fn dumpStats(ctx: *Ctx, session: *Session, be: *backend.KvBackend, plane: *plane_mod.DataPlane) void {
    // The transport family is ~6 KiB with the zc legs' keys; a truncated
    // line would hide the must-stay-0 gauges the smoke reads.
    var buf: [16384]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    session.writeStatsJson(&w) catch {};
    log.info("stats transport: {s}", .{w.buffered()});
    {
        // Every data family under one object (the largest line: a 64 KiB
        // buffer; a truncated dump names itself).
        var dbuf: [65536]u8 = undefined;
        var dw: Io.Writer = .fixed(&dbuf);
        plane.writeFamiliesJson(&dw) catch {};
        log.info("stats data: {s}", .{dw.buffered()});
    }
    {
        // The metadata plane: the conveyor, the checkpoint's park-driven
        // cycles, the journal lane — the family that names a commit stall.
        w = .fixed(&buf);
        w.writeAll("{\"conveyor\":") catch {};
        be.conveyorStats().writeJson(&w) catch {};
        w.writeAll(",\"conveyor_phases\":") catch {};
        be.conveyorStats().writePhasesJson(&w) catch {};
        w.writeAll(",\"meta_barrier\":") catch {};
        checkpoint.stats.meta_barrier.snapshot().writeJson(&w) catch {};
        w.print(",\"checkpoint\":{{\"meta_kv_checkpoints\":{d},\"meta_kv_ledger_writes\":{d},\"meta_device_syncs\":{d},\"meta_kv_checkpoint_park_cycles\":{d},\"meta_kv_maintenance_items\":{d},\"pending_free_stalled_cycles\":{d}}},\"journal_lane\":", .{
            checkpoint.stats.meta_kv_checkpoints.load(),
            checkpoint.stats.meta_kv_ledger_writes.load(),
            checkpoint.stats.meta_device_syncs.load(),
            checkpoint.stats.meta_kv_checkpoint_park_cycles.load(),
            checkpoint.stats.meta_kv_maintenance_items.load(),
            checkpoint.stats.pending_free_stalled_cycles.load(),
        }) catch {};
        journal_lane.writeStatsJson(&w) catch {};
        w.writeAll("}") catch {};
        log.info("stats meta: {s}", .{w.buffered()});
    }
    w = .fixed(&buf);
    ctx.sqz.writeStatsJson(&w) catch {};
    log.info("stats exec: {s}", .{w.buffered()});
    w = .fixed(&buf);
    mem_budget.authority.writeStatsJson(&w) catch {};
    log.info("stats mem_budget: {s}", .{w.buffered()});
    log.info("stats daemon: {{\"open_count_stranded\":{d},\"invariant_tripwires\":{d},\"fuse_ops\":{d},\"fuse_op_watchdog_overdue\":{d},\"fuse_ops_inflight\":{d},\"fuse_write_no_data_volume_refused\":{d},\"fuse_write_beyond_inline_map_refused\":{d},\"fuse_write_inline\":{d},\"fuse_write_staged\":{d},\"fuse_write_striped\":{d},\"fuse_write_promotions_to_striped\":{d},\"fuse_truncate_packed_clips\":{d},\"fuse_truncate_striped_clips\":{d},\"fuse_truncate_restages\":{d},\"fuse_truncate_zero_tail_bytes\":{d},\"fuse_reclaim_destroys\":{d},\"fuse_corpse_sweep_destroyed\":{d},\"fuse_reclaim_drains\":{d},\"fuse_reclaim_blocks_freed\":{d},\"exec_lane_tick_rescues\":{d},\"transport_park_tick_commit_rescues\":{d},\"transport_park_tick_cqe_rescues\":{d},\"lock_ticked_reregisters\":{d},\"channel_ticked_reregisters\":{d},\"volume_failed\":{}}}", .{
        counters.tripwireCount("open_count_stranded"),
        counters.invariantTripwires(),
        handler.lifecycle.fuse_ops.load(),
        watchdog.fuse_op_watchdog_overdue.load(),
        watchdog.fuse_ops_inflight.load(),
        handler.kv_fs.fuse_write_no_data_volume_refused.load(),
        handler.kv_fs.fuse_write_beyond_inline_map_refused.load(),
        handler.kv_fs.fuse_write_inline.load(),
        handler.kv_fs.fuse_write_staged.load(),
        handler.kv_fs.fuse_write_striped.load(),
        handler.kv_fs.fuse_write_promotions_to_striped.load(),
        handler.kv_fs.fuse_truncate_packed_clips.load(),
        handler.kv_fs.fuse_truncate_striped_clips.load(),
        handler.kv_fs.fuse_truncate_restages.load(),
        handler.kv_fs.fuse_truncate_zero_tail_bytes.load(),
        handler.kv_fs.fuse_reclaim_destroys.load(),
        handler.kv_fs.fuse_corpse_sweep_destroyed.load(),
        handler.kv_fs.fuse_reclaim_drains.load(),
        handler.kv_fs.fuse_reclaim_blocks_freed.load(),
        exec_stats.lane_tick_rescues.load(),
        transport.worker.park_tick_commit_rescues.load(),
        transport.worker.park_tick_cqe_rescues.load(),
        sync_tick.lock_ticked_reregisters.load(),
        sync_tick.channel_ticked_reregisters.load(),
        be.isFailed(),
    });
}

const testing = std.testing;

test "the reclaim cadence is one watchdog tick" {
    try testing.expectEqual(watchdog.TICK_NS, RECLAIM_CADENCE_NS);
}
